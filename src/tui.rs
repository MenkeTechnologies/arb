//! ratatui render loop. Widgets are auto-tiled vertically (explicit `pack`/`grid`
//! geometry arrives later). Each widget's `source` pipeline is evaluated against
//! the shared stream every frame; `text`/`tail`/`list` render the resulting
//! lines and `gauge` renders a scalar against `-max`. Widget kinds without a
//! renderer yet show an honest placeholder rather than faking output.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, size, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{BarChart, Block, Borders, Gauge, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

use rayon::prelude::*;

use crate::query::{eval, is_line_streamable, QueryOp, QueryResult};
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

/// Parse a `--height` spec into inline viewport rows: `N` (absolute) or `N%` of
/// the terminal height. `None` (doesn't parse) → full-screen.
fn parse_height(spec: &str) -> Option<u16> {
    let s = spec.trim();
    if let Some(p) = s.strip_suffix('%') {
        let pct: u32 = p.trim().parse().ok()?;
        let rows = size().map(|(_, h)| h).unwrap_or(24) as u32;
        Some((rows * pct / 100).clamp(3, rows) as u16)
    } else {
        s.parse::<u16>().ok().map(|n| n.max(3))
    }
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
    /// fzf prompt string (`--prompt`); empty falls back to `> `.
    pub prompt: String,
    /// fzf header line shown above the list (`--header`); empty = no header.
    pub header: String,
    /// `input .name` widget values (name, current text) for DSL form mode. When
    /// non-empty the TUI is a form: typing edits the focused input, Tab cycles.
    pub inputs: Vec<(String, String)>,
    /// Index of the focused input in `inputs`.
    pub focus: usize,
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

/// Parse a line's ANSI SGR colour codes into a styled ratatui line, so command
/// output (`bat --color`, `ls --color`, a `--preview`) shows its colours instead
/// of literal escape sequences. Plain text passes through unchanged. ratatui
/// clips the rendered line to the pane width.
fn ansi_line(s: &str) -> Line<'static> {
    use ansi_to_tui::IntoText;
    match s.as_bytes().into_text() {
        Ok(text) => text.lines.into_iter().next().unwrap_or_default(),
        Err(_) => Line::from(s.to_string()),
    }
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
                // Form mode: `input` widgets present (and not fzf) — typing edits
                // the focused input, Tab cycles focus.
                let form = !fzf && !c.inputs.is_empty();
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
                        0x09 if form => {
                            // Tab: cycle focus between inputs.
                            let nlen = c.inputs.len();
                            c.focus = (c.focus + 1) % nlen;
                        }
                        0x1b => {
                            if form {
                                let f = c.focus;
                                c.inputs[f].1.clear();
                            } else if fzf || c.filter.is_empty() {
                                c.quit = true;
                                break 'read;
                            } else {
                                c.filter.clear();
                                c.cursor = 0;
                            }
                        }
                        0x08 | 0x7f => {
                            if form {
                                let f = c.focus;
                                c.inputs[f].1.pop();
                            } else {
                                c.filter.pop();
                                c.cursor = 0;
                            }
                        }
                        0x15 => {
                            if form {
                                let f = c.focus;
                                c.inputs[f].1.clear();
                            } else {
                                c.filter.clear();
                                c.cursor = 0;
                            }
                        }
                        0x20..=0x7e => {
                            if form {
                                let f = c.focus;
                                c.inputs[f].1.push(b as char);
                            } else {
                                c.filter.push(b as char);
                                c.cursor = 0;
                            }
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
    height: Option<String>,
) -> io::Result<()> {
    let tty: File = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    enable_raw_mode()?;
    // `--height`: render inline (a viewport of N rows at the bottom, keeping the
    // scrollback) instead of taking over the whole screen with the alternate
    // buffer. `N%` is a fraction of the terminal height.
    let inline = height.as_deref().and_then(parse_height);
    let backend = CrosstermBackend::new(tty);
    let mut terminal = match inline {
        Some(rows) => Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(rows),
            },
        )?,
        None => {
            let mut t = Terminal::new(backend)?;
            execute!(t.backend_mut(), EnterAlternateScreen)?;
            t
        }
    };

    controls.lock().unwrap().fzf = fzf;
    spawn_key_handler(controls.clone());

    // Select-mode projection (`--with-nth`): the select widget's `source` pipeline
    // transforms each raw line into what's SHOWN and SEARCHED, while the original
    // line is what's EMITTED on Enter. Only per-line-streamable pipelines project
    // (a projection must map line→line(s); cross-line ops like sort/count can't).
    // The synthesized `select .sel { in }` is identity → display == original.
    let select_widget = spec.widgets.iter().find(|w| w.kind == WidgetKind::Select);
    let proj: Vec<QueryOp> = select_widget
        .and_then(|w| w.source.as_ref())
        .map(|s| s.pipeline.clone())
        .filter(|p| is_line_streamable(p))
        .unwrap_or_default();
    // Optional search-key pipeline (`search .name { … }`, fzf `--nth`): the fuzzy
    // match runs against this key (derived per raw line) while the row still shows
    // and emits the display. Empty = search the display (default). Non-streamable
    // falls back to searching the display too.
    let search_proj: Vec<QueryOp> = select_widget
        .and_then(|w| w.search.clone())
        .filter(|p| is_line_streamable(p))
        .unwrap_or_default();

    // fzf-mode incremental match state. Each candidate is scored ONCE as it
    // arrives (indices are stable — the fzf buffer never drops), not the whole
    // buffer every frame. Candidates are `(display, search_key, original)`: the
    // key drives fuzzy match, the display renders, the original is emitted. Empty
    // filter appends in stream order; a real filter accumulates scored hits,
    // re-sorted on a short debounce.
    let mut fzf_cands: Vec<(String, String, String)> = Vec::new(); // (display, key, original)
    let mut fzf_raw_done = 0usize; // raw stream lines already projected
    let mut fzf_filter = String::from("\u{0}"); // sentinel: forces initial reset
    let mut fzf_processed = 0usize; // candidates already scored
    let mut fzf_hits: Vec<(i32, String, String, String)> = Vec::new(); // (score, display, key, original)
    let mut fzf_matched: Vec<(String, String)> = Vec::new(); // display list (display, original)
    let mut fzf_last_sort = Instant::now() - Duration::from_secs(1);
    // fzf prompt/header (set once by `main` before this call).
    let (fzf_prompt, fzf_header) = {
        let c = controls.lock().unwrap();
        let p = if c.prompt.is_empty() {
            "> ".to_string()
        } else {
            c.prompt.clone()
        };
        (p, c.header.clone())
    };

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
                // Project any new raw lines into candidates (once each, indices
                // stable). Identity projection is a plain clone; a real projection
                // may map one raw line to zero (filtered) or several display rows,
                // each carrying that raw line as its emit-original. The search key
                // is derived per raw line (shared across its display rows); with no
                // `search` pipeline it defaults to the display so match == what you see.
                for i in fzf_raw_done..st.lines.len() {
                    let raw = &st.lines[i];
                    let key = if search_proj.is_empty() {
                        None
                    } else {
                        Some(project_line(&search_proj, raw).join(" "))
                    };
                    for disp in project_line(&proj, raw) {
                        let k = key.clone().unwrap_or_else(|| disp.clone());
                        fzf_cands.push((disp, k, raw.clone()));
                    }
                }
                fzf_raw_done = st.lines.len();
                let n = fzf_cands.len();
                let empty = filter.is_empty();
                if filter != fzf_filter {
                    // fzf's query-extension trick: typing another char can only
                    // narrow the current matches (fuzzy match is monotonic), so
                    // re-filter the existing hit set instead of rescanning the
                    // whole (million-line) buffer. Only a non-prefix change
                    // (backspace, new query) does a full — parallel — rescan.
                    let extends = !empty && !fzf_filter.is_empty() && filter.starts_with(&fzf_filter);
                    if empty {
                        fzf_hits.clear();
                        fzf_matched.clear();
                        fzf_processed = 0;
                    } else if extends {
                        let old = std::mem::take(&mut fzf_hits);
                        fzf_hits = old
                            .into_iter()
                            .filter_map(|(_, d, k, o)| fuzzy_score(&k, &filter).map(|s| (s, d, k, o)))
                            .collect();
                        // keep fzf_processed — new candidates scored below
                    } else {
                        // Full rescan across cores (rayon) — first char / backspace.
                        // Match on the search key `k`, carry the display `d`.
                        fzf_hits = fzf_cands
                            .par_iter()
                            .filter_map(|(d, k, o)| {
                                fuzzy_score(k, &filter).map(|s| (s, d.clone(), k.clone(), o.clone()))
                            })
                            .collect();
                        fzf_processed = n;
                    }
                    fzf_filter = filter.clone();
                    fzf_last_sort = Instant::now() - Duration::from_secs(1);
                }
                // Incorporate new candidates since the last frame.
                for (d, k, o) in fzf_cands.iter().take(n).skip(fzf_processed) {
                    if empty {
                        fzf_matched.push((d.clone(), o.clone()));
                    } else if let Some(sc) = fuzzy_score(k, &filter) {
                        fzf_hits.push((sc, d.clone(), k.clone(), o.clone()));
                    }
                }
                fzf_processed = n;
            }
            // Non-empty filter: re-sort the (narrowed) hit set into the display
            // list on a short debounce — cheap once the query has narrowed it.
            if !filter.is_empty() {
                let now = Instant::now();
                if now.duration_since(fzf_last_sort) >= Duration::from_millis(100) {
                    let mut h = fzf_hits.clone();
                    h.par_sort_by(|a, b| b.0.cmp(&a.0));
                    fzf_matched = h.into_iter().map(|(_, d, _k, o)| (d, o)).collect();
                    fzf_last_sort = now;
                }
            }
            let matched = &fzf_matched;
            let sel = cursor.min(matched.len().saturating_sub(1));

            let mut c = controls.lock().unwrap();
            // Publish the cursor's ORIGINAL line so a `--preview` thread acts on
            // what would be emitted, not the projected display.
            c.current = matched.get(sel).map(|(_, o)| o.clone()).unwrap_or_default();
            if c.toggle {
                // Tab: toggle the cursor line's original in the mark set, advance.
                c.toggle = false;
                if let Some((_, orig)) = matched.get(sel) {
                    match c.marks.iter().position(|m| m == orig) {
                        Some(pos) => {
                            c.marks.remove(pos);
                        }
                        None => c.marks.push(orig.clone()),
                    }
                }
                c.cursor = c.cursor.saturating_add(1);
            }
            if submit {
                // Emit the marks if any (multi-select), else the cursor original.
                c.result = if c.marks.is_empty() {
                    matched.get(sel).map(|(_, o)| o.clone()).into_iter().collect()
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
            let draw = terminal.draw(|f| {
                render_fzf(
                    f, matched, &filter, sel, &marks, total, err_ref, prev_ref, &fzf_prompt,
                    &fzf_header,
                )
            });
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
        // Snapshot live `input .name` values (form mode) so bound `apply .name`
        // pipelines resolve against what the user has typed, and the focused
        // field renders highlighted.
        let (inputs, focus_name): (HashMap<String, String>, Option<String>) = {
            let c = controls.lock().unwrap();
            let map = c.inputs.iter().cloned().collect();
            let focus = c.inputs.get(c.focus).map(|(n, _)| n.clone());
            (map, focus)
        };
        let st = state.lock().unwrap();
        let draw = terminal.draw(|f| {
            let down_ref = down_snap.as_ref().map(|(l, lab)| (l.as_slice(), lab.as_str()));
            render(f, spec, &st, &filter, down_ref, err_ref, &inputs, focus_name.as_deref());
        });
        drop(st);
        if let Err(e) = draw {
            break Err(e);
        }
        thread::sleep(Duration::from_millis(120));
    };

    disable_raw_mode()?;
    if inline.is_some() {
        // Inline mode never entered the alternate screen; clear the viewport
        // region so the UI doesn't linger in the scrollback.
        terminal.clear()?;
    } else {
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    }
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
    inputs: &HashMap<String, String>,
    focus: Option<&str>,
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
        // Input widgets are interactive fields, not stream views: render the live
        // value with the focused field highlighted.
        if w.kind == WidgetKind::Input {
            let name = w.path.trim_start_matches('.');
            let val = inputs.get(name).map(String::as_str).unwrap_or("");
            render_input(f, rects[i], w, val, focus == Some(name));
            continue;
        }
        // Resolve `apply .name` placeholders against the live input values before
        // evaluating, so a bound pipeline reflects what the user has typed.
        let result = w.source.as_ref().map(|s| {
            let pipeline = crate::spec::resolve_pipeline(&s.pipeline, inputs);
            eval(&pipeline, &raw, elapsed)
        });
        render_widget(f, rects[i], w, st, &raw, result);
    }
}

/// Render an `input .name` widget as an editable field: `label: value▏`, with a
/// cyan border + reversed caret when focused. `placeholder`/`title` opts supply
/// the label and dimmed empty-state hint.
fn render_input(f: &mut Frame, area: Rect, w: &Widget, val: &str, focused: bool) {
    let label = w
        .opts
        .get("title")
        .or_else(|| w.opts.get("placeholder"))
        .map(String::as_str)
        .unwrap_or_else(|| w.path.trim_start_matches('.'));
    let border = if focused { Color::Cyan } else { Color::DarkGray };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(format!(" {label} "));
    let body = if focused {
        Line::from(vec![
            Span::raw(val.to_string()),
            Span::styled("▏", Style::default().fg(Color::Cyan)),
        ])
    } else if val.is_empty() {
        Line::from(Span::styled(
            w.opts.get("placeholder").cloned().unwrap_or_default(),
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(Span::raw(val.to_string()))
    };
    f.render_widget(Paragraph::new(body).block(block), area);
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
    let inner_h = area.height.saturating_sub(2) as usize;
    let skip = lines.len().saturating_sub(inner_h);
    let items: Vec<ListItem> = lines
        .iter()
        .skip(skip)
        .map(|l| ListItem::new(ansi_line(l)))
        .collect();
    f.render_widget(List::new(items).block(block), area);
}

#[allow(clippy::too_many_arguments)]
/// Apply a per-line select projection to one raw line, yielding its display
/// row(s). Empty pipeline = identity. A filtering projection (`grep`/`reject`)
/// can yield zero rows (the line drops out of the candidate list); a transform
/// (`field`/`upper`) yields one. The caller pairs each row with the raw line as
/// its emit-original, so the display is searchable while Enter emits the source.
pub fn project_line(proj: &[QueryOp], raw: &str) -> Vec<String> {
    if proj.is_empty() {
        return vec![raw.to_string()];
    }
    let one = [raw.to_string()];
    match eval(proj, &one, 0.0) {
        QueryResult::Lines(ls) => ls,
        QueryResult::Scalar(v) => vec![format!("{v}")],
        QueryResult::Pairs(p) => p.into_iter().map(|(k, v)| format!("{k}\t{v}")).collect(),
    }
}

fn render_fzf(
    f: &mut Frame,
    matched: &[(String, String)],
    filter: &str,
    sel: usize,
    marks: &[String],
    total: u64,
    err: Option<(&[String], &str)>,
    preview: Option<(&[String], &str)>,
    prompt: &str,
    header: &str,
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
    let header_h: u16 = if header.is_empty() { 0 } else { 1 };
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(header_h),
        Constraint::Min(0),
    ])
    .split(main_top);
    let marked = if marks.is_empty() {
        String::new()
    } else {
        format!(" ({})", marks.len())
    };
    // fzf-style prompt: cyan prompt string, the query with a cursor bar, then a
    // cyan matched/total(marked) counter and dim key hints.
    let cyan = Style::default().fg(Color::Cyan);
    let prompt_line = Line::from(vec![
        Span::styled(prompt.to_string(), cyan.add_modifier(Modifier::BOLD)),
        Span::raw(format!("{filter}\u{258f}")),
        Span::styled(format!("   {}/{total}{marked}", matched.len()), cyan),
        Span::styled(
            "   Enter select · Tab mark · \u{2191}\u{2193} move · Ctrl-C abort",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(prompt_line), chunks[0]);
    if !header.is_empty() {
        f.render_widget(
            Paragraph::new(header).style(Style::default().fg(Color::DarkGray)),
            chunks[1],
        );
    }

    let inner_w = chunks[2].width as usize;
    // Only build ListItems for the VISIBLE window around the cursor — not the
    // whole (possibly million-line) match list. This is what keeps arb as fast as
    // fzf: fuzzy-highlighting and allocation happen for ~a screenful, not all rows.
    let list_h = chunks[2].height as usize;
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
        // Show the projected display; a row is marked when its ORIGINAL is marked.
        .map(|(disp, orig)| {
            ListItem::new(fzf_line(disp, filter, inner_w, mark_set.contains(orig.as_str())))
        })
        .collect();
    let mut state = ListState::default();
    if n > 0 {
        state.select(Some(sel - start));
    }
    // Cyan pointer + a subtle highlight bar on the cursor line, fzf-style.
    let list = List::new(items)
        .highlight_symbol("\u{25b6} ")
        .highlight_style(Style::default().bg(Color::Rgb(38, 38, 46)).add_modifier(Modifier::BOLD));
    f.render_stateful_widget(list, chunks[2], &mut state);
}

/// Render the captured downstream output (`arb -- CMD`) as a tailed list pane —
/// the child's stdout+stderr, hooked to a temp file and shown here so it never
/// touches the terminal.
fn render_output_pane(f: &mut Frame, area: Rect, label: &str, lines: &[String]) {
    let title = format!(" -- {label} · {} ln ", lines.len());
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner_h = area.height.saturating_sub(2) as usize;
    let skip = lines.len().saturating_sub(inner_h);
    let items: Vec<ListItem> = lines
        .iter()
        .skip(skip)
        .map(|l| ListItem::new(ansi_line(l)))
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
                .map(|l| ListItem::new(ansi_line(l)))
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
