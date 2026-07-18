//! ratatui render loop. Widgets are auto-tiled vertically (explicit `pack`/`grid`
//! geometry arrives later). Each widget's `source` pipeline is evaluated against
//! the shared stream every frame; `text`/`tail`/`list` render the resulting
//! lines and `gauge` renders a scalar against `-max`. Widget kinds without a
//! renderer yet show an honest placeholder rather than faking output.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
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
    /// Enter pressed in fzf mode — the run loop resolves `selected` and exits.
    pub submit: bool,
    /// The chosen line (fzf mode), set by the run loop on submit; printed to
    /// stdout by `main`. `None` = aborted with no selection.
    pub selected: Option<String>,
    /// Whether the key handler interprets nav/Enter as fzf select controls.
    pub fzf: bool,
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
            let mut buf = [0u8; 1];
            while let Ok(1) = tty.read(&mut buf) {
                let b = buf[0];
                let mut c = controls.lock().unwrap();
                let fzf = c.fzf;
                match b {
                    0x03 => {
                        c.quit = true;
                        break;
                    }
                    // fzf select mode: Enter submits; Ctrl-N/J move down (toward
                    // newer), Ctrl-P/K move up (toward older).
                    0x0d if fzf => {
                        c.submit = true;
                        break;
                    }
                    0x0e | 0x0a if fzf => c.cursor = c.cursor.saturating_sub(1),
                    0x10 | 0x0b if fzf => c.cursor = c.cursor.saturating_add(1),
                    0x1b => {
                        // Esc: fzf mode aborts; otherwise clear the filter (or quit
                        // when it is already empty).
                        if fzf || c.filter.is_empty() {
                            c.quit = true;
                            break;
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
        if fzf {
            // fzf select mode: fuzzy-match + rank the stream, best first; a cursor
            // (from the top) highlights one line, Enter resolves it and exits.
            let st = state.lock().unwrap();
            let mut scored: Vec<(i32, String)> = st
                .lines
                .iter()
                .filter_map(|l| fuzzy_score(l, &filter).map(|s| (s, l.clone())))
                .collect();
            drop(st);
            // Stable sort by score desc — ties keep stream order.
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            let matched: Vec<String> = scored.into_iter().map(|(_, l)| l).collect();
            let sel = cursor.min(matched.len().saturating_sub(1));
            if submit {
                controls.lock().unwrap().selected = matched.get(sel).cloned();
                break Ok(());
            }
            let draw = terminal.draw(|f| render_fzf(f, &matched, &filter, sel));
            if let Err(e) = draw {
                break Err(e);
            }
            thread::sleep(Duration::from_millis(60));
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
            render(f, spec, &st, &filter, down_ref);
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
) {
    // Reserve a one-row filter bar at the very bottom; widgets fill the rest.
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(f.area());
    let mut area = chunks[0];
    let bar = chunks[1];

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
/// Full-screen fzf select view: an fzf-style prompt line at the top, the filtered
/// list below with the cursor line highlighted. ratatui auto-scrolls to keep the
/// selection visible. Cursor 0 is the newest (bottom) line.
fn render_fzf(f: &mut Frame, matched: &[String], filter: &str, sel: usize) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(f.area());
    let prompt = format!("> {filter}\u{258f}   {} match(es)  ·  Enter select · Ctrl-C abort · Ctrl-J/K move", matched.len());
    f.render_widget(Paragraph::new(prompt), chunks[0]);

    let inner_w = chunks[1].width as usize;
    let items: Vec<ListItem> = matched
        .iter()
        .map(|l| ListItem::new(clip(l, inner_w)))
        .collect();
    let mut state = ListState::default();
    if !matched.is_empty() {
        state.select(Some(sel.min(matched.len() - 1)));
    }
    let list = List::new(items)
        .highlight_symbol("> ")
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
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
