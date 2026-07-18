//! ratatui render loop. Widgets are auto-tiled vertically (explicit `pack`/`grid`
//! geometry arrives later). Each widget's `source` pipeline is evaluated against
//! the shared stream every frame; `text`/`tail`/`list` render the resulting
//! lines and `gauge` renders a scalar against `-max`. Widget kinds without a
//! renderer yet show an honest placeholder rather than faking output.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{BarChart, Block, Borders, Gauge, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use crate::query::{eval, QueryResult};
use crate::spec::{Spec, Widget, WidgetKind};
use crate::stream::StreamState;

/// Whether an interactive TUI can run: key events need a controlling terminal.
/// When stdin carries the data pipe (`find / | arb`), crossterm reads key events
/// from `/dev/tty` — exactly how `vipe` reads the keyboard mid-pipeline — so we
/// probe that it opens. If it can't (no controlling tty: CI, a detached exec, a
/// terminal without `/dev/tty`), the caller falls back to a non-interactive path
/// instead of entering raw mode and crashing with "failed to initialize input
/// reader". stdin itself stays the data stream and is never consumed for events.
pub fn events_available() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .is_ok()
}

/// Interactive control state shared between the key reader, the render loop, and
/// the stdin→stdout tee: the live filter text and the quit flag. Keys are read
/// straight from `/dev/tty` (like `vipe`) rather than crossterm's event source,
/// whose `mio`-on-tty reader fails to initialize on some hosts.
#[derive(Default)]
pub struct Controls {
    pub filter: String,
    pub quit: bool,
    /// fzf select mode: cursor offset from the newest (bottom) filtered line.
    pub cursor: usize,
    /// Enter pressed in fzf mode — the run loop fills `result` and exits.
    pub submit: bool,
    /// Whether the key handler interprets nav/Tab/Enter as fzf select controls.
    pub fzf: bool,
    /// Tab-marked lines (multi-select). Emitted on Enter when non-empty.
    pub marks: Vec<String>,
    /// Tab pressed — the run loop toggles the cursor line in `marks`, since only
    /// it knows the current filtered list + cursor.
    pub toggle: bool,
    /// Final selection (fzf mode), filled by the run loop on submit and printed
    /// to stdout by `main`: the marks if any, else the cursor line.
    pub result: Vec<String>,
    /// The line currently under the cursor (fzf mode), published by the run loop
    /// so a `--preview` thread can run its command on it as you move.
    pub current: String,
}

/// The megafilter predicate: a line is kept iff it matches the interactive
/// filter (case-insensitive substring); an empty filter keeps everything. The
/// SAME test narrows the on-screen dashboard and the passthrough to a downstream
/// consumer, so what you type reshapes both live.
pub fn filter_matches(line: &str, filter: &str) -> bool {
    filter.is_empty() || line.to_lowercase().contains(&filter.to_lowercase())
}

/// Fuzzy match (fzf-style, smart-case): the pattern chars must appear in `line`
/// in order but not necessarily contiguously. Returns `None` if not a match, else
/// a score where higher is better — contiguous runs and word-boundary starts are
/// rewarded, gaps penalized. An empty pattern matches everything with score 0.
pub fn fuzzy_score(line: &str, pat: &str) -> Option<i32> {
    if pat.is_empty() {
        return Some(0);
    }
    // Smart case: a pattern with any uppercase is case-sensitive, else insensitive.
    let cased = pat.chars().any(|c| c.is_uppercase());
    let norm = |c: char| if cased { c } else { c.to_ascii_lowercase() };
    let l: Vec<char> = line.chars().collect();
    let p: Vec<char> = pat.chars().collect();
    let mut score = 0i32;
    let mut li = 0usize;
    let mut prev: Option<usize> = None;
    for &pc in &p {
        let target = norm(pc);
        let idx = loop {
            if li >= l.len() {
                return None;
            }
            if norm(l[li]) == target {
                break li;
            }
            li += 1;
        };
        if let Some(pi) = prev {
            if idx == pi + 1 {
                score += 15; // consecutive
            } else {
                score -= (idx - pi - 1).min(10) as i32; // gap penalty (bounded)
            }
        }
        if idx == 0 || !l[idx - 1].is_alphanumeric() {
            score += 10; // word-boundary / start bonus
        }
        prev = Some(idx);
        li = idx + 1;
    }
    Some(score)
}

/// Truncate a line to `width` characters so it never overflows its box. (Wide
/// upstream `stderr` — e.g. `find /` permission errors — must be redirected by
/// the user; arb can only clip what flows through its own stream.)
fn clip(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        s.chars().take(width).collect()
    }
}

/// Read key bytes from `/dev/tty` and drive `Controls`: printable chars build
/// the filter live, Backspace/Ctrl-U edit it, Esc clears it (or quits when it is
/// already empty), Ctrl-C quits. Raw mode delivers each keypress immediately.
fn spawn_key_handler(controls: Arc<Mutex<Controls>>) {
    if let Ok(mut tty) = OpenOptions::new().read(true).open("/dev/tty") {
        thread::spawn(move || {
            // Read several bytes at once so a `\x1b[A/B` arrow escape (which the
            // terminal delivers as one burst) is parsed as a unit, while a lone
            // Esc (1 byte) still reads as Esc. Fast typing/paste is handled too.
            let mut buf = [0u8; 32];
            'read: loop {
                let n = match tty.read(&mut buf) {
                    Ok(n) if n > 0 => n,
                    _ => break,
                };
                let mut c = controls.lock().unwrap();
                let fzf = c.fzf;
                let mut i = 0;
                while i < n {
                    let b = buf[i];
                    // Arrow keys: ESC [ A (up) / B (down).
                    if b == 0x1b && i + 2 < n && buf[i + 1] == b'[' {
                        match buf[i + 2] {
                            b'A' if fzf => c.cursor = c.cursor.saturating_sub(1),
                            b'B' if fzf => c.cursor = c.cursor.saturating_add(1),
                            _ => {}
                        }
                        i += 3;
                        continue;
                    }
                    match b {
                        0x03 => {
                            c.quit = true;
                            break 'read;
                        }
                        // fzf select: Enter submits; Ctrl-N/J = down, Ctrl-P/K = up.
                        0x0d if fzf => {
                            c.submit = true;
                            break 'read;
                        }
                        0x0e | 0x0a if fzf => c.cursor = c.cursor.saturating_add(1),
                        0x10 | 0x0b if fzf => c.cursor = c.cursor.saturating_sub(1),
                        0x09 if fzf => c.toggle = true, // Tab: mark/unmark
                        0x1b => {
                            // Esc: fzf aborts; else clear the filter (or quit if empty).
                            if fzf || c.filter.is_empty() {
                                c.quit = true;
                                break 'read;
                            }
                            c.filter.clear();
                            c.cursor = 0;
                        }
                        0x08 | 0x7f => {
                            c.filter.pop();
                            c.cursor = 0;
                        }
                        0x15 => {
                            c.filter.clear();
                            c.cursor = 0;
                        }
                        0x20..=0x7e => {
                            c.filter.push(b as char);
                            c.cursor = 0;
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
        });
    }
}

/// Run the TUI until the user quits (`q`/Esc/Ctrl-C). Renders to `/dev/tty` (the
/// terminal), NOT stdout — like fzf — so stdout stays a clean data channel for a
/// downstream consumer (`find / | arb | consumer`). Unlike fzf, arb never blocks
/// the pipeline: the caller tees stdin→stdout live in a separate thread while
/// this loop draws. The terminal is always restored (raw mode off, alternate
/// screen left, cursor shown) before returning, even on a draw error.
pub fn run(
    spec: &Spec,
    state: Arc<Mutex<StreamState>>,
    controls: Arc<Mutex<Controls>>,
    down: Option<(Arc<Mutex<StreamState>>, String)>,
    err: Option<(Arc<Mutex<StreamState>>, String)>,
    fzf: bool,
) -> io::Result<()> {
    let tty: File = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    enable_raw_mode()?;
    // Wrap the tty in the backend before entering the alternate screen: the
    // backend is write-only, so `execute!` isn't ambiguous over `File`'s Read +
    // Write `by_ref`.
    let mut terminal = Terminal::new(CrosstermBackend::new(tty))?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;

    controls.lock().unwrap().fzf = fzf;
    spawn_key_handler(controls.clone());

    // fzf-mode incremental match state. Each stream line is scored ONCE as it
    // arrives (indices are stable — the fzf buffer never drops), not the whole
    // buffer every frame. Empty filter appends in stream order; a real filter
    // accumulates scored hits, re-sorted into the display list on a short debounce.
    let mut fzf_filter = String::from("\u{0}"); // sentinel: forces initial reset
    let mut fzf_processed = 0usize; // stream lines already scored
    let mut fzf_hits: Vec<(i32, String)> = Vec::new(); // matching lines (non-empty filter)
    let mut fzf_matched: Vec<String> = Vec::new(); // display list
    let mut fzf_last_sort = Instant::now() - Duration::from_secs(1);

    // Redraw on a fixed cadence so live stream updates show; the key handler runs
    // independently, so the render loop never blocks on input (the pipeline keeps
    // flowing regardless of keypresses).
    let outcome = loop {
        let (filter, quit, submit, cursor) = {
            let c = controls.lock().unwrap();
            (c.filter.clone(), c.quit, c.submit, c.cursor)
        };
        if quit {
            break Ok(());
        }
        // Snapshot the error pane (spawned producer's stderr) — a bordered strip
        // at the bottom, so upstream errors show inside arb, never on the terminal.
        let err_snap: Option<(Vec<String>, String)> = err.as_ref().map(|(es, label)| {
            let e = es.lock().unwrap();
            let n = e.lines.len();
            let tail = e.lines.iter().skip(n.saturating_sub(200)).cloned().collect();
            (tail, format!("{label} ({})", e.total))
        });
        let err_ref = err_snap
            .as_ref()
            .filter(|(l, _)| !l.is_empty())
            .map(|(l, lab)| (l.as_slice(), lab.as_str()));
        if fzf {
            // fzf select mode: incrementally fuzzy-match the stream (each line
            // scored once), rank best-first, cursor highlights one, Enter resolves.
            let total;
            {
                let st = state.lock().unwrap();
                total = st.total;
                if filter != fzf_filter {
                    // Filter changed — restart matching from scratch.
                    fzf_hits.clear();
                    fzf_matched.clear();
                    fzf_processed = 0;
                    fzf_filter = filter.clone();
                }
                let empty = filter.is_empty();
                for i in fzf_processed..st.lines.len() {
                    let line = &st.lines[i];
                    if empty {
                        fzf_matched.push(line.clone()); // stream order, no scoring
                    } else if let Some(sc) = fuzzy_score(line, &filter) {
                        fzf_hits.push((sc, line.clone()));
                    }
                }
                fzf_processed = st.lines.len();
            }
            // Non-empty filter: re-sort accumulated hits into the display list on a
            // short debounce (cheap — only matching lines, not the whole buffer).
            if !filter.is_empty() {
                let now = Instant::now();
                if now.duration_since(fzf_last_sort) >= Duration::from_millis(150) {
                    let mut h = fzf_hits.clone();
                    h.sort_by(|a, b| b.0.cmp(&a.0));
                    fzf_matched = h.into_iter().map(|(_, l)| l).collect();
                    fzf_last_sort = now;
                }
            }
            let matched = &fzf_matched;
            let sel = cursor.min(matched.len().saturating_sub(1));

            let mut c = controls.lock().unwrap();
            // Publish the cursor line so a `--preview` thread can act on it.
            c.current = matched.get(sel).cloned().unwrap_or_default();
            if c.toggle {
                // Tab: toggle the cursor line in the mark set, then advance.
                c.toggle = false;
                if let Some(line) = matched.get(sel) {
                    match c.marks.iter().position(|m| m == line) {
                        Some(pos) => {
                            c.marks.remove(pos);
                        }
                        None => c.marks.push(line.clone()),
                    }
                }
                c.cursor = c.cursor.saturating_add(1);
            }
            if submit {
                // Emit the marks if any (multi-select), else the cursor line.
                c.result = if c.marks.is_empty() {
                    matched.get(sel).cloned().into_iter().collect()
                } else {
                    c.marks.clone()
                };
                break Ok(());
            }
            let marks = c.marks.clone();
            drop(c);
            // Snapshot the `--preview` pane (command output for the cursor line).
            let prev_snap: Option<(Vec<String>, String)> = down.as_ref().map(|(ds, label)| {
                let d = ds.lock().unwrap();
                (d.lines.iter().cloned().collect(), label.clone())
            });
            let prev_ref = prev_snap.as_ref().map(|(l, lab)| (l.as_slice(), lab.as_str()));
            let draw = terminal
                .draw(|f| render_fzf(f, matched, &filter, sel, &marks, total, err_ref, prev_ref));
            if let Err(e) = draw {
                break Err(e);
            }
            // Snappy input response; frames are cheap now (windowed rendering).
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        // Snapshot the downstream pane's recent output (tail) before drawing, so
        // the render closure doesn't hold a second lock.
        let down_snap: Option<(Vec<String>, String)> = down.as_ref().map(|(ds, label)| {
            let d = ds.lock().unwrap();
            let n = d.lines.len();
            let tail = d.lines.iter().skip(n.saturating_sub(1000)).cloned().collect();
            (tail, label.clone())
        });
        let st = state.lock().unwrap();
        let draw = terminal.draw(|f| {
            let down_ref = down_snap.as_ref().map(|(l, lab)| (l.as_slice(), lab.as_str()));
            render(f, spec, &st, &filter, down_ref, err_ref);
        });
        drop(st);
        if let Err(e) = draw {
            break Err(e);
        }
        thread::sleep(Duration::from_millis(120));
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    outcome
}

fn render(
    f: &mut Frame,
    spec: &Spec,
    st: &StreamState,
    filter: &str,
    down: Option<(&[String], &str)>,
    err: Option<(&[String], &str)>,
) {
    // Bottom: an optional stderr strip (spawned producer errors) above the filter bar.
    let err_h = match err {
        Some((lines, _)) => ((lines.len() as u16) + 2).clamp(3, 8),
        None => 0,
    };
    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(err_h),
        Constraint::Length(1),
    ])
    .split(f.area());
    let mut area = chunks[0];
    let bar = chunks[2];
    if let Some((lines, label)) = err {
        render_err_pane(f, chunks[1], label, lines);
    }

    // With a downstream command, split the main area: stream dashboard on the
    // left, the captured `-- CMD` output pane on the right.
    if let Some((dlines, label)) = down {
        let cols =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).split(area);
        area = cols[0];
        render_output_pane(f, cols[1], label, dlines);
    }

    let matched = st.lines.iter().filter(|l| filter_matches(l, filter)).count();
    let hint = if filter.is_empty() {
        "  type to filter  ·  Bksp edit  ·  Esc clear  ·  Ctrl-C quit".to_string()
    } else {
        format!("  filter: {filter}▏   {matched}/{} lines", st.lines.len())
    };
    f.render_widget(Paragraph::new(hint), bar);

    // Materialize the ring once, narrowed by the interactive filter — so the
    // whole dashboard (tail, counts, tallies) reflects what you typed.
    let raw: Vec<String> = st
        .lines
        .iter()
        .filter(|l| filter_matches(l, filter))
        .cloned()
        .collect();

    if spec.widgets.is_empty() {
        let msg = Paragraph::new("arb: spec has no widgets")
            .block(Block::default().borders(Borders::ALL).title(" arb "));
        f.render_widget(msg, area);
        return;
    }

    let rects = compute_rects(area, spec);
    let elapsed = st.start.elapsed().as_secs_f64();
    for (i, w) in spec.widgets.iter().enumerate() {
        let result = w.source.as_ref().map(|s| eval(&s.pipeline, &raw, elapsed));
        render_widget(f, rects[i], w, st, &raw, result);
    }
}

/// One rect per widget. Auto vertical stack unless any widget has a grid cell,
/// in which case a rows×cols grid is built and each widget placed in its cell
/// (widgets sharing a cell overlap; last-drawn wins). Spans arrive later.
/// Char indices in `line` that the fuzzy pattern matched (greedy, in order) —
/// used to highlight matched characters, fzf-style. Smart-case like `fuzzy_score`.
pub fn match_positions(line: &str, pat: &str) -> Vec<usize> {
    if pat.is_empty() {
        return Vec::new();
    }
    let cased = pat.chars().any(|c| c.is_uppercase());
    let norm = |c: char| if cased { c } else { c.to_ascii_lowercase() };
    let l: Vec<char> = line.chars().collect();
    let mut positions = Vec::new();
    let mut li = 0;
    for pc in pat.chars() {
        let target = norm(pc);
        while li < l.len() {
            if norm(l[li]) == target {
                positions.push(li);
                li += 1;
                break;
            }
            li += 1;
        }
    }
    positions
}

/// Build a styled list line for fzf mode: a mark gutter (`+` for a Tab-marked
/// line) followed by the text with fuzzy-matched characters highlighted.
fn fzf_line(line: &str, filter: &str, width: usize, marked: bool) -> Line<'static> {
    let text: String = line.chars().take(width.saturating_sub(2)).collect();
    let gutter = if marked {
        Span::styled("+ ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
    } else {
        Span::raw("  ")
    };
    if filter.is_empty() {
        return Line::from(vec![gutter, Span::raw(text)]);
    }
    let pos: std::collections::HashSet<usize> = match_positions(&text, filter).into_iter().collect();
    let hl = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let mut spans = vec![gutter];
    let mut cur = String::new();
    let mut cur_hl = false;
    for (i, ch) in text.chars().enumerate() {
        let h = pos.contains(&i);
        if h != cur_hl && !cur.is_empty() {
            let s = std::mem::take(&mut cur);
            spans.push(if cur_hl { Span::styled(s, hl) } else { Span::raw(s) });
        }
        cur_hl = h;
        cur.push(ch);
    }
    if !cur.is_empty() {
        spans.push(if cur_hl { Span::styled(cur, hl) } else { Span::raw(cur) });
    }
    Line::from(spans)
}

/// Full-screen fzf select view: an fzf-style prompt line at the top, the filtered
/// list below with the cursor line highlighted. ratatui auto-scrolls to keep the
/// selection visible. Cursor 0 is the newest (bottom) line.
/// A red-bordered pane for a spawned command's stderr (`--run` producer errors),
/// so upstream errors show inside arb instead of scribbling over the TUI.
fn render_err_pane(f: &mut Frame, area: Rect, label: &str, lines: &[String]) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .title(format!(" \u{26a0} {label} "));
    let inner_w = (area.width as usize).saturating_sub(2);
    let inner_h = area.height.saturating_sub(2) as usize;
    let skip = lines.len().saturating_sub(inner_h);
    let items: Vec<ListItem> = lines
        .iter()
        .skip(skip)
        .map(|l| ListItem::new(clip(l, inner_w)))
        .collect();
    f.render_widget(List::new(items).block(block), area);
}

#[allow(clippy::too_many_arguments)]
fn render_fzf(
    f: &mut Frame,
    matched: &[String],
    filter: &str,
    sel: usize,
    marks: &[String],
    total: u64,
    err: Option<(&[String], &str)>,
    preview: Option<(&[String], &str)>,
) {
    // Reserve a bottom strip for the stderr pane when present.
    let (top, err_area) = match err {
        Some((lines, _)) => {
            let h = ((lines.len() as u16) + 2).clamp(3, 10);
            let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(h)]).split(f.area());
            (rows[0], Some(rows[1]))
        }
        None => (f.area(), None),
    };
    if let (Some(ea), Some((lines, label))) = (err_area, err) {
        render_err_pane(f, ea, label, lines);
    }
    // With `--preview`, split the top: the select list on the left, the preview
    // pane (command output for the cursor line) on the right.
    let (main_top, prev_area) = match preview {
        Some(_) => {
            let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(top);
            (cols[0], Some(cols[1]))
        }
        None => (top, None),
    };
    if let (Some(pa), Some((lines, label))) = (prev_area, preview) {
        render_output_pane(f, pa, label, lines);
    }
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(main_top);
    let marked = if marks.is_empty() {
        String::new()
    } else {
        format!(" ({})", marks.len())
    };
    // fzf-style prompt: cyan "> ", the query with a cursor bar, then a cyan
    // matched/total(marked) counter and dim key hints.
    let cyan = Style::default().fg(Color::Cyan);
    let prompt = Line::from(vec![
        Span::styled("> ", cyan.add_modifier(Modifier::BOLD)),
        Span::raw(format!("{filter}\u{258f}")),
        Span::styled(format!("   {}/{total}{marked}", matched.len()), cyan),
        Span::styled(
            "   Enter select · Tab mark · \u{2191}\u{2193} move · Ctrl-C abort",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(prompt), chunks[0]);

    let inner_w = chunks[1].width as usize;
    // Only build ListItems for the VISIBLE window around the cursor — not the
    // whole (possibly million-line) match list. This is what keeps arb as fast as
    // fzf: fuzzy-highlighting and allocation happen for ~a screenful, not all rows.
    let list_h = chunks[1].height as usize;
    let n = matched.len();
    let sel = sel.min(n.saturating_sub(1));
    let start = if list_h > 0 && sel >= list_h {
        sel + 1 - list_h
    } else {
        0
    };
    let end = (start + list_h.max(1)).min(n);
    let mark_set: std::collections::HashSet<&str> = marks.iter().map(String::as_str).collect();
    let items: Vec<ListItem> = matched[start..end]
        .iter()
        .map(|l| ListItem::new(fzf_line(l, filter, inner_w, mark_set.contains(l.as_str()))))
        .collect();
    let mut state = ListState::default();
    if n > 0 {
        state.select(Some(sel - start));
    }
    // Cyan pointer + a subtle highlight bar on the cursor line, fzf-style.
    let list = List::new(items)
        .highlight_symbol("\u{25b6} ")
        .highlight_style(Style::default().bg(Color::Rgb(38, 38, 46)).add_modifier(Modifier::BOLD));
    f.render_stateful_widget(list, chunks[1], &mut state);
}

/// Render the captured downstream output (`arb -- CMD`) as a tailed list pane —
/// the child's stdout+stderr, hooked to a temp file and shown here so it never
/// touches the terminal.
fn render_output_pane(f: &mut Frame, area: Rect, label: &str, lines: &[String]) {
    let title = format!(" -- {label} · {} ln ", lines.len());
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner_w = (area.width as usize).saturating_sub(2);
    let inner_h = area.height.saturating_sub(2) as usize;
    let skip = lines.len().saturating_sub(inner_h);
    let items: Vec<ListItem> = lines
        .iter()
        .skip(skip)
        .map(|l| ListItem::new(clip(l, inner_w)))
        .collect();
    f.render_widget(List::new(items).block(block), area);
}

fn compute_rects(area: Rect, spec: &Spec) -> Vec<Rect> {
    let ws = &spec.widgets;
    let grid_mode = ws.iter().any(|w| w.grid.is_some());
    if !grid_mode {
        let n = ws.len().max(1) as u32;
        let cons: Vec<Constraint> = (0..n).map(|_| Constraint::Ratio(1, n)).collect();
        return Layout::vertical(cons).split(area).to_vec();
    }
    let pos: Vec<(usize, usize)> = ws.iter().map(|w| w.grid.unwrap_or((0, 0))).collect();
    let rows = pos.iter().map(|(r, _)| *r).max().unwrap_or(0) + 1;
    let cols = pos.iter().map(|(_, c)| *c).max().unwrap_or(0) + 1;
    let row_cons: Vec<Constraint> = (0..rows).map(|_| Constraint::Ratio(1, rows as u32)).collect();
    let row_chunks = Layout::vertical(row_cons).split(area);
    pos.iter()
        .map(|(r, c)| {
            let col_cons: Vec<Constraint> =
                (0..cols).map(|_| Constraint::Ratio(1, cols as u32)).collect();
            Layout::horizontal(col_cons).split(row_chunks[*r])[*c]
        })
        .collect()
}

fn render_widget(
    f: &mut Frame,
    area: Rect,
    w: &Widget,
    st: &StreamState,
    lines: &[String],
    result: Option<QueryResult>,
) {
    let title = format!(
        " {} · {} · {} ln {:.0}/s ",
        w.path,
        w.kind.label(),
        st.total,
        st.rate()
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    // Inner width for clipping long lines so they never overflow the box.
    let inner_w = (area.width as usize).saturating_sub(2);
    match w.kind {
        WidgetKind::Text => {
            let s = match &result {
                Some(QueryResult::Scalar(v)) => format!("{v:.2}"),
                Some(QueryResult::Lines(ls)) => ls.last().cloned().unwrap_or_default(),
                Some(QueryResult::Pairs(p)) => p
                    .first()
                    .map(|(k, v)| format!("{k} ({v})"))
                    .unwrap_or_default(),
                None => lines.last().cloned().unwrap_or_default(),
            };
            f.render_widget(Paragraph::new(clip(&s, inner_w)).block(block), area);
        }
        WidgetKind::Tail | WidgetKind::List => {
            let owned: Vec<String> = match &result {
                Some(QueryResult::Lines(ls)) => ls.clone(),
                Some(QueryResult::Scalar(v)) => vec![format!("{v}")],
                Some(QueryResult::Pairs(p)) => {
                    p.iter().map(|(k, v)| format!("{k}  {v}")).collect()
                }
                None => lines.to_vec(),
            };
            let inner_h = area.height.saturating_sub(2) as usize;
            let skip = owned.len().saturating_sub(inner_h);
            let items: Vec<ListItem> = owned
                .iter()
                .skip(skip)
                .map(|l| ListItem::new(clip(l, inner_w)))
                .collect();
            f.render_widget(List::new(items).block(block), area);
        }
        WidgetKind::Gauge => {
            let val = match &result {
                Some(QueryResult::Scalar(v)) => *v,
                _ => 0.0,
            };
            let max = w
                .opts
                .get("max")
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(100.0);
            let ratio = if max > 0.0 { (val / max).clamp(0.0, 1.0) } else { 0.0 };
            let g = Gauge::default()
                .block(block)
                .ratio(ratio)
                .label(format!("{val:.0}/{max:.0}"));
            f.render_widget(g, area);
        }
        WidgetKind::Bars | WidgetKind::Histo => {
            let pairs: Vec<(String, u64)> = match &result {
                Some(QueryResult::Pairs(p)) => p.clone(),
                _ => Vec::new(),
            };
            let top = w
                .opts
                .get("top")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(20);
            let shown: Vec<(&str, u64)> =
                pairs.iter().take(top).map(|(k, v)| (k.as_str(), *v)).collect();
            let n = shown.len().max(1);
            let inner_w = area.width.saturating_sub(2) as usize;
            let bw = ((inner_w / n).saturating_sub(1)).clamp(1, 12) as u16;
            let chart = BarChart::default()
                .block(block)
                .bar_width(bw)
                .bar_gap(1)
                .data(&shown[..]);
            f.render_widget(chart, area);
        }
        _ => {
            let msg = format!("{} — not yet rendered (M2b)", w.kind.label());
            f.render_widget(Paragraph::new(msg).block(block), area);
        }
    }
}
