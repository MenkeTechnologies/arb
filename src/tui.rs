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
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, size, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Map, MapResolution, Points};
use ratatui::widgets::calendar::{CalendarEventStore, Monthly};
use ratatui::widgets::{
    Axis, BarChart, Block, Borders, Cell, Chart, Dataset, Gauge, GraphType, LineGauge, List,
    ListItem, ListState, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Sparkline,
    Table, Tabs,
};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

use rayon::prelude::*;

use crate::query::{eval, is_line_streamable, QueryOp, QueryResult};
use crate::spec::{Bind, BindAction, Spec, Timeout, Widget, WidgetKind};
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
    /// Per-control metadata parallel to `inputs` (slider/facet/check bounds), so
    /// the key handler and renderer know each focused control's kind. Empty for a
    /// plain `input` form; populated when control widgets are present.
    pub control_meta: Vec<ControlMeta>,
    /// Index of the focused input in `inputs`.
    pub focus: usize,
    /// Key bindings (`bind C-<letter> …`): a matching control key runs its action
    /// (set an input → drives the megafilter/map; quit).
    pub binds: Vec<Bind>,
    /// fzf compat: exact substring match instead of fuzzy (`--exact`/`-e`).
    pub exact: bool,
    /// fzf compat: keep input order, don't sort by score (`--no-sort`).
    pub no_sort: bool,
    /// Set by a `beep` action; the run loop rings the bell after the next draw
    /// then clears it.
    pub beep_pending: bool,
    /// Active `alert` message + when it expires (shown in the status bar).
    pub alert: Option<(String, Instant)>,
    /// Active `flash` tints: widget path (no dot) -> (color name, expiry).
    pub flashes: HashMap<String, (String, Instant)>,
    /// Rendered widget rects (updated each frame), so the key-handler thread can
    /// hit-test a mouse click without the spec/area.
    pub hitmap: Vec<HitTarget>,
    /// Screen row of the first fzf list item (so a click maps to a cursor index).
    pub fzf_list_start: usize,
    /// `bind <Click> …` reactions, fired on any mouse press.
    pub mouse_binds: Vec<(crate::spec::MouseTrigger, BindAction)>,
    /// `tabs` widget selection: widget name (no dot) -> selected tab index, set by
    /// a tab-bar click and read by the Tabs render arm.
    pub tab_sel: HashMap<String, usize>,
    /// Per-widget history scrollback: widget name -> rows scrolled back from the
    /// live bottom (0 = live tail; wheel-up increments, wheel-down decrements).
    pub scroll: HashMap<String, usize>,
    /// Previous mouse-down (time, col, row) for double-click detection.
    pub last_click: Option<(Instant, u16, u16)>,
    /// Writer to a `spawn -pty` child's stdin, so the `send "…"` action can drive
    /// it (Expect). `None` unless the stream source is a PTY.
    pub pty_writer: Option<Box<dyn std::io::Write + Send>>,
    /// The actor session (SPEC §15): `spawn`/`pool` refs, driven by `tell`/`ask`
    /// bind actions. Empty unless the spec declares session refs.
    pub session: crate::actor::Session,
    /// Live color theme, initialized from the resolved spec/config theme. The `c`
    /// key cycles it through the 31 built-ins at runtime (persisted to `~/.arb`);
    /// render reads this, not `spec.theme`, so a cycle takes effect immediately.
    pub theme: Option<crate::theme::Palette>,
    /// Index into `theme::THEMES` of the current theme.
    pub theme_idx: usize,
    /// Whether the `Ctrl-G` global help overlay is showing.
    pub help_open: bool,
    /// Whether the `Ctrl-T` theme-chooser popup is open (ported from iftoprs).
    pub theme_picker_open: bool,
    /// The highlighted row in the theme chooser (live-previews as it moves).
    pub theme_picker_sel: usize,
    /// The theme index to revert to if the chooser is cancelled with Esc.
    pub theme_picker_revert: usize,
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

/// fzf `--exact`/`-e` scoring: case-insensitive substring match (smart-case),
/// `None` if `line` doesn't contain `pat`; earlier matches score higher. Used in
/// place of [`fuzzy_score`] when exact mode is on.
pub fn exact_score(line: &str, pat: &str) -> Option<i32> {
    if pat.is_empty() {
        return Some(0);
    }
    let cased = pat.chars().any(|c| c.is_uppercase());
    if cased {
        line.find(pat).map(|i| -(i as i32))
    } else {
        line.to_lowercase()
            .find(&pat.to_lowercase())
            .map(|i| -(i as i32))
    }
}

/// Score a line against the query with the active mode (exact substring or fuzzy).
fn score_line(line: &str, pat: &str, exact: bool) -> Option<i32> {
    if exact {
        exact_score(line, pat)
    } else {
        fuzzy_score(line, pat)
    }
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

/// Apply a [`BindAction`] to `Controls` — shared by key `bind`s and stream
/// `expect` reactions. `Quit` sets the flag (the run loop exits on it); `SetInput`
/// writes the named input value, driving the megafilter/map.
fn apply_bind_action(c: &mut Controls, action: &BindAction) {
    match action {
        BindAction::Quit => c.quit = true,
        BindAction::SetInput { name, value } => {
            if let Some(slot) = c.inputs.iter_mut().find(|(n, _)| n == name) {
                slot.1 = value.clone();
            }
        }
        BindAction::Beep => c.beep_pending = true,
        BindAction::Alert(msg) => {
            c.alert = Some((msg.clone(), Instant::now() + Duration::from_secs(3)));
        }
        BindAction::Flash { widget, color } => {
            c.flashes.insert(
                widget.clone(),
                (color.clone(), Instant::now() + Duration::from_secs(2)),
            );
        }
        BindAction::Exec(cmd) => {
            // Fire-and-forget: spawn, never wait — the run loop must not block.
            let _ = std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        BindAction::Send(text) => {
            // Write to the `spawn -pty` child's stdin (Expect `send`); a no-op
            // when the source isn't a PTY. Best-effort — a closed child is fine.
            if let Some(w) = c.pty_writer.as_mut() {
                use std::io::Write;
                let _ = w.write_all(text.as_bytes());
                let _ = w.flush();
            }
        }
        BindAction::ActorTell { refname, call } => {
            // Parse + evaluate the message call against live control values, then
            // tell the session ref (fire-and-forget). A bad call/unknown ref is a
            // silent no-op — the run loop must never block or panic on a keystroke.
            if let Ok((msg, argexprs)) = crate::actor::parse_call(call) {
                let inputs: HashMap<String, String> = c.inputs.iter().cloned().collect();
                let args = crate::actor::eval_args(&argexprs, &inputs);
                c.session.tell(refname, &msg, args);
            }
        }
        BindAction::ActorAsk {
            ctrl,
            refname,
            call,
        } => {
            // Ask the ref and store the reply into control `ctrl` so a widget
            // bound to it displays the result.
            if let Ok((msg, argexprs)) = crate::actor::parse_call(call) {
                let inputs: HashMap<String, String> = c.inputs.iter().cloned().collect();
                let args = crate::actor::eval_args(&argexprs, &inputs);
                if let Some(v) = c.session.ask(refname, &msg, args) {
                    let text = crate::query::fmt_scalar(v);
                    if let Some(slot) = c.inputs.iter_mut().find(|(n, _)| n == ctrl) {
                        slot.1 = text;
                    }
                }
            }
        }
        BindAction::Seq(actions) => {
            for a in actions {
                apply_bind_action(c, a);
            }
        }
    }
}

/// Advance idle-timeout state one render tick. If the stream advanced since the
/// last tick, reset the idle clock and re-arm every latch. Otherwise fire any
/// timeout whose idle span has elapsed and latch it (once until the next line).
fn tick_timeouts(
    timeouts: &[Timeout],
    now_total: u64,
    last_total: &mut u64,
    last_activity: &mut Instant,
    fired: &mut [bool],
    now: Instant,
    c: &mut Controls,
) {
    if now_total != *last_total {
        *last_total = now_total;
        *last_activity = now;
        fired.iter_mut().for_each(|f| *f = false);
        return;
    }
    let idle = now.saturating_duration_since(*last_activity);
    for (i, t) in timeouts.iter().enumerate() {
        if !fired[i] && idle >= t.dur {
            apply_bind_action(c, &t.action);
            fired[i] = true;
        }
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
                    // The focused control's kind (form mode) drives key handling.
                    let fk = if form {
                        c.control_meta
                            .get(c.focus)
                            .map(|m| m.kind)
                            .unwrap_or(ControlKind::Text)
                    } else {
                        ControlKind::Text
                    };
                    // SGR mouse report `ESC[<…M/m` — must precede the arrow branch,
                    // which would otherwise swallow the `[`. Click/scroll/drag are
                    // hit-tested + dispatched; a truncated report re-syncs on `i+=1`.
                    if b == 0x1b && buf.get(i + 1) == Some(&b'[') && buf.get(i + 2) == Some(&b'<') {
                        if let Some((ev, used)) = parse_sgr_mouse(&buf[..n], i) {
                            dispatch_mouse(&mut c, ev, fzf, Instant::now());
                            if c.quit {
                                break 'read;
                            }
                            i += used;
                            continue;
                        }
                        i += 1;
                        continue;
                    }
                    // Theme chooser (Ctrl-T popup): while open it captures all keys
                    // — arrows/j/k navigate + live-preview, Enter saves, Esc/q
                    // cancels, Ctrl-C still quits. Everything else is swallowed.
                    if c.theme_picker_open {
                        if b == 0x1b && i + 2 < n && buf[i + 1] == b'[' {
                            match buf[i + 2] {
                                b'A' => theme_picker_move(&mut c, -1),
                                b'B' => theme_picker_move(&mut c, 1),
                                _ => {}
                            }
                            i += 3;
                            continue;
                        }
                        match b {
                            b'k' | 0x10 => theme_picker_move(&mut c, -1), // k / Ctrl-P
                            b'j' | 0x0e => theme_picker_move(&mut c, 1),  // j / Ctrl-N
                            0x0d => theme_picker_accept(&mut c),          // Enter
                            0x1b | b'q' => theme_picker_cancel(&mut c),   // Esc / q
                            0x14 => theme_picker_cancel(&mut c),          // Ctrl-T toggles closed
                            0x03 => {
                                c.quit = true;
                                break 'read;
                            }
                            _ => {}
                        }
                        i += 1;
                        continue;
                    }
                    // Arrow keys: ESC [ A/B/C/D. fzf moves the cursor; a form slider
                    // adjusts on Left/Right, a facet moves its cursor on Up/Down.
                    if b == 0x1b && i + 2 < n && buf[i + 1] == b'[' {
                        match buf[i + 2] {
                            b'A' if fzf => c.cursor = c.cursor.saturating_sub(1),
                            b'B' if fzf => c.cursor = c.cursor.saturating_add(1),
                            // Facet and Sel both move a row cursor with Up/Down.
                            b'A' if matches!(fk, ControlKind::Facet | ControlKind::Sel) => {
                                let f = c.focus;
                                c.control_meta[f].cursor =
                                    c.control_meta[f].cursor.saturating_sub(1);
                            }
                            b'B' if matches!(fk, ControlKind::Facet | ControlKind::Sel) => {
                                let f = c.focus;
                                c.control_meta[f].cursor =
                                    c.control_meta[f].cursor.saturating_add(1);
                            }
                            b'C' if fk == ControlKind::Slider => slider_key(&mut c, true),
                            b'D' if fk == ControlKind::Slider => slider_key(&mut c, false),
                            _ => {}
                        }
                        i += 3;
                        continue;
                    }
                    match b {
                        // A declared `bind` wins over the hardwired editing/control
                        // keys for any control byte (C-u/C-h/Esc/…) — otherwise
                        // e.g. `bind C-u …` (documented in the README) is silently
                        // shadowed by the clear-input handler and never fires.
                        // Printable bytes (>= 0x20) still fall through so filter and
                        // input typing is never shadowed.
                        _ if b < 0x20 && c.binds.iter().any(|bd| bd.key == b) => {
                            let action = c
                                .binds
                                .iter()
                                .find(|bd| bd.key == b)
                                .map(|bd| bd.action.clone());
                            if let Some(action) = action {
                                apply_bind_action(&mut c, &action);
                                if c.quit {
                                    break 'read;
                                }
                            }
                        }
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
                            if c.help_open {
                                // Esc closes the help overlay first, before quit/clear.
                                c.help_open = false;
                            } else if form {
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
                        // Slider: `+`/`=`/`l` up, `-`/`h` down (by one step).
                        b'+' | b'=' | b'l' if fk == ControlKind::Slider => slider_key(&mut c, true),
                        b'-' | b'h' if fk == ControlKind::Slider => slider_key(&mut c, false),
                        // Check: Space/Enter toggles the boolean.
                        0x20 | 0x0d if fk == ControlKind::Check => {
                            let f = c.focus;
                            c.inputs[f].1 = toggle_check(&c.inputs[f].1);
                        }
                        // Facet: Space toggles the option under the cursor.
                        0x20 if fk == ControlKind::Facet => {
                            let f = c.focus;
                            let cur = c.control_meta[f].cursor;
                            if let Some(item) = c.control_meta[f].opts.get(cur).cloned() {
                                c.inputs[f].1 = toggle_set_member(&c.inputs[f].1, &item);
                            }
                        }
                        // Ctrl-T opens the theme chooser popup (works in EVERY
                        // mode). A bare `c` (iftop's key) can't be used — the
                        // megafilter, the fzf filter, and `input` controls all
                        // consume printable bytes as text; a control byte never
                        // types, so it's safe here, in fzf, and in a form alike.
                        0x14 => open_theme_picker(&mut c),
                        // Ctrl-G toggles the global help overlay (works everywhere).
                        0x07 => c.help_open = !c.help_open,
                        0x20..=0x7e => {
                            if form && fk == ControlKind::Text {
                                let f = c.focus;
                                c.inputs[f].1.push(b as char);
                            } else if !form {
                                c.filter.push(b as char);
                                c.cursor = 0;
                            }
                        }
                        // A declared `bind C-<letter> …` control key: run its action.
                        // (Clone the action first so the immutable `binds` borrow
                        // ends before we mutate `inputs`/`quit`.)
                        _ => {
                            if let Some(action) = c
                                .binds
                                .iter()
                                .find(|bd| bd.key == b)
                                .map(|bd| bd.action.clone())
                            {
                                apply_bind_action(&mut c, &action);
                                if c.quit {
                                    break 'read;
                                }
                            }
                        }
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
/// One fzf candidate: `(display, search_key, original)`, all `Arc<str>` so the
/// identity projection shares a single allocation across the three handles.
type FzfCand = (Arc<str>, Arc<str>, Arc<str>);
/// A scored candidate: `(score, display, search_key, original)`.
type FzfHit = (i32, Arc<str>, Arc<str>, Arc<str>);

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
    // SGR mouse reporting (button + `?1006`): clicks/scroll/drag arrive as
    // `ESC[<…M/m` in the /dev/tty byte stream, decoded by the key handler.
    execute!(terminal.backend_mut(), EnableMouseCapture)?;

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
    // Lines are shared as `Arc<str>` — for the common identity projection (no
    // `search`/projection, e.g. `find / | arb --fzf`) display == key == original
    // is ONE allocation shared by three cheap refcount handles, and the match /
    // hit / display vecs clone pointers, not strings. Without this a fast producer
    // allocated the line 6× per frame and pegged the allocator (visible lag).
    // (display, key, original) and its scored form (score, display, key, original).
    let mut fzf_cands: Vec<FzfCand> = Vec::new();
    let mut fzf_raw_done = 0usize; // raw stream lines already projected
    let mut fzf_filter = String::from("\u{0}"); // sentinel: forces initial reset
    let mut fzf_processed = 0usize; // candidates already scored
    let mut fzf_hits: Vec<FzfHit> = Vec::new();
    let mut fzf_matched: Vec<(Arc<str>, Arc<str>)> = Vec::new(); // display list (display, original)
    let mut fzf_last_sort = Instant::now() - Duration::from_secs(1);
    // `expect /re/ …` reactions: total stream lines already checked against the
    // patterns. Tracked by `total` (lines-ever), not a deque index, because the
    // dashboard ring drops old lines — we scan the newest arrivals each frame.
    let mut expect_total: u64 = 0;
    // fzf prompt/header (set once by `main` before this call).
    let (fzf_prompt, fzf_header, fzf_exact, fzf_no_sort) = {
        let c = controls.lock().unwrap();
        let p = if c.prompt.is_empty() {
            "> ".to_string()
        } else {
            c.prompt.clone()
        };
        (p, c.header.clone(), c.exact, c.no_sort)
    };

    // `timeout Ns …` idle reactions: track the last stream `total` and when it
    // last advanced; each timeout fires once per idle span, re-armed on a new line.
    let mut to_last_total: u64 = { state.lock().unwrap().total };
    let mut to_last_activity = Instant::now();
    let mut to_fired = vec![false; spec.timeouts.len()];
    // `bind <Resize>`: poll the terminal size each frame; fire on a change.
    let mut last_size = size().unwrap_or((0, 0));

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
        // `expect` reactions: scan any new stream lines against the patterns and
        // fire the matching action (set a control / quit). Snapshot the new lines
        // under the state lock, release it, THEN take the controls lock — never
        // hold both, so the reader/key threads can't deadlock against this.
        if !spec.expects.is_empty() {
            let new_lines: Vec<String> = {
                let st = state.lock().unwrap();
                // Scan the newest `total - already-seen` lines still retained in the
                // ring (older ones that scrolled past between frames are missed —
                // an honest limit on a stream faster than the redraw cadence).
                let new_count = st.total.saturating_sub(expect_total) as usize;
                let take = new_count.min(st.lines.len());
                let start = st.lines.len() - take;
                expect_total = st.total;
                st.lines.iter().skip(start).cloned().collect()
            };
            if !new_lines.is_empty() {
                let mut c = controls.lock().unwrap();
                for line in &new_lines {
                    for ex in &spec.expects {
                        if ex.pattern.is_match(line) {
                            apply_bind_action(&mut c, &ex.action);
                        }
                    }
                }
            }
        }
        // `timeout Ns …` idle reactions — same lock discipline as expect (read
        // state, drop it, then lock controls; never hold both at once).
        if !spec.timeouts.is_empty() {
            let now_total = { state.lock().unwrap().total };
            let mut c = controls.lock().unwrap();
            tick_timeouts(
                &spec.timeouts,
                now_total,
                &mut to_last_total,
                &mut to_last_activity,
                &mut to_fired,
                Instant::now(),
                &mut c,
            );
        }
        // `bind <Resize>` reactions: fire on a terminal size change.
        let cur_size = size().unwrap_or(last_size);
        if !spec.resize_binds.is_empty() && detect_resize(&mut last_size, cur_size) {
            let mut c = controls.lock().unwrap();
            let actions = spec.resize_binds.clone();
            for a in &actions {
                apply_bind_action(&mut c, a);
            }
        }
        // Snapshot the error pane (spawned producer's stderr) — a bordered strip
        // at the bottom, so upstream errors show inside arb, never on the terminal.
        let err_snap: Option<(Vec<String>, String)> = err.as_ref().map(|(es, label)| {
            let e = es.lock().unwrap();
            let n = e.lines.len();
            let tail = e
                .lines
                .iter()
                .skip(n.saturating_sub(200))
                .cloned()
                .collect();
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
                //
                // Cap the batch so a firehose producer (`find /`) can't make one
                // frame do unbounded work while holding the stream lock — the rest
                // ingests over the next frames, keeping the UI responsive.
                const MAX_INGEST_PER_FRAME: usize = 50_000;
                let ingest_end = st.lines.len().min(fzf_raw_done + MAX_INGEST_PER_FRAME);
                for i in fzf_raw_done..ingest_end {
                    let raw = &st.lines[i];
                    // Identity fast-path (no projection, no search key): the whole
                    // line is display, key and original at once — one allocation.
                    if proj.is_empty() && search_proj.is_empty() {
                        let a: Arc<str> = Arc::from(raw.as_str());
                        fzf_cands.push((a.clone(), a.clone(), a));
                        continue;
                    }
                    let orig: Arc<str> = Arc::from(raw.as_str());
                    let key: Option<Arc<str>> = if search_proj.is_empty() {
                        None
                    } else {
                        Some(Arc::from(project_line(&search_proj, raw).join(" ").as_str()))
                    };
                    for disp in project_line(&proj, raw) {
                        let d: Arc<str> = Arc::from(disp.as_str());
                        let k = key.clone().unwrap_or_else(|| d.clone());
                        fzf_cands.push((d, k, orig.clone()));
                    }
                }
                fzf_raw_done = ingest_end;
                let n = fzf_cands.len();
                let empty = filter.is_empty();
                if filter != fzf_filter {
                    // fzf's query-extension trick: typing another char can only
                    // narrow the current matches (fuzzy match is monotonic), so
                    // re-filter the existing hit set instead of rescanning the
                    // whole (million-line) buffer. Only a non-prefix change
                    // (backspace, new query) does a full — parallel — rescan.
                    let extends =
                        !empty && !fzf_filter.is_empty() && filter.starts_with(&fzf_filter);
                    if empty {
                        fzf_hits.clear();
                        fzf_matched.clear();
                        fzf_processed = 0;
                    } else if extends {
                        let old = std::mem::take(&mut fzf_hits);
                        fzf_hits = old
                            .into_iter()
                            .filter_map(|(_, d, k, o)| {
                                score_line(&k, &filter, fzf_exact).map(|s| (s, d, k, o))
                            })
                            .collect();
                        // keep fzf_processed — new candidates scored below
                    } else {
                        // Full rescan across cores (rayon) — first char / backspace.
                        // Match on the search key `k`, carry the display `d`.
                        fzf_hits = fzf_cands
                            .par_iter()
                            .filter_map(|(d, k, o)| {
                                score_line(k, &filter, fzf_exact)
                                    .map(|s| (s, d.clone(), k.clone(), o.clone()))
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
                    } else if let Some(sc) = score_line(k, &filter, fzf_exact) {
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
                    // `--no-sort` keeps the input (scan) order; else rank best-first.
                    if !fzf_no_sort {
                        h.par_sort_by(|a, b| b.0.cmp(&a.0));
                    }
                    fzf_matched = h.into_iter().map(|(_, d, _k, o)| (d, o)).collect();
                    fzf_last_sort = now;
                }
            }
            let matched = &fzf_matched;
            let sel = cursor.min(matched.len().saturating_sub(1));

            let mut c = controls.lock().unwrap();
            // Publish the cursor's ORIGINAL line so a `--preview` thread acts on
            // what would be emitted, not the projected display.
            c.current = matched.get(sel).map(|(_, o)| o.to_string()).unwrap_or_default();
            if c.toggle {
                // Tab: toggle the cursor line's original in the mark set, advance.
                c.toggle = false;
                if let Some((_, orig)) = matched.get(sel) {
                    match c.marks.iter().position(|m| m.as_str() == orig.as_ref()) {
                        Some(pos) => {
                            c.marks.remove(pos);
                        }
                        None => c.marks.push(orig.to_string()),
                    }
                }
                c.cursor = c.cursor.saturating_add(1);
            }
            if submit {
                // Emit the marks if any (multi-select), else the cursor original.
                c.result = if c.marks.is_empty() {
                    matched
                        .get(sel)
                        .map(|(_, o)| o.to_string())
                        .into_iter()
                        .collect()
                } else {
                    c.marks.clone()
                };
                break Ok(());
            }
            let marks = c.marks.clone();
            let fzf_theme = c.theme; // live theme (Ctrl-T chooser previews it)
            let fzf_help = c.help_open;
            let fzf_picker = c.theme_picker_open.then_some(c.theme_picker_sel);
            drop(c);
            // Snapshot the `--preview` pane (command output for the cursor line).
            let prev_snap: Option<(Vec<String>, String)> = down.as_ref().map(|(ds, label)| {
                let d = ds.lock().unwrap();
                (d.lines.iter().cloned().collect(), label.clone())
            });
            let prev_ref = prev_snap
                .as_ref()
                .map(|(l, lab)| (l.as_slice(), lab.as_str()));
            let mut hitmap: Vec<HitTarget> = Vec::new();
            let mut fzf_start = 0usize;
            let draw = terminal.draw(|f| {
                fzf_start = render_fzf(
                    f,
                    matched,
                    &filter,
                    sel,
                    &marks,
                    total,
                    err_ref,
                    prev_ref,
                    &fzf_prompt,
                    &fzf_header,
                    fzf_theme,
                    &mut hitmap,
                );
                // Global overlays draw on top of the picker too.
                if fzf_help {
                    draw_help_overlay(f, spec, fzf_theme);
                }
                if let Some(sel) = fzf_picker {
                    draw_theme_picker(f, sel, fzf_theme);
                }
            });
            // Publish the fzf list hit target so a click moves the cursor.
            {
                let mut c = controls.lock().unwrap();
                c.hitmap = hitmap;
                c.fzf_list_start = fzf_start;
            }
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
            let tail = d
                .lines
                .iter()
                .skip(n.saturating_sub(1000))
                .cloned()
                .collect();
            (tail, label.clone())
        });
        // Publish each `sel` widget's highlighted row into its `.<path>.sel`
        // control before the snapshot, so downstream widgets/actions resolve
        // against the live selection this frame. Snapshot the stream lines first
        // (state lock), release it, THEN take the controls lock — never both at
        // once (reader/key deadlock discipline).
        {
            let lines: Vec<String> = {
                let st = state.lock().unwrap();
                st.lines.iter().cloned().collect()
            };
            let mut c = controls.lock().unwrap();
            update_sel_controls(spec, &lines, &mut c);
        }
        // Snapshot live `input .name` values (form mode) so bound `apply .name`
        // pipelines resolve against what the user has typed, and the focused
        // field renders highlighted.
        // Snapshot form values plus the transient `alert`/`flash`/`beep` action
        // state (pruning anything expired) in one lock, so render is lock-free.
        let mut c = controls.lock().unwrap();
        let inputs: HashMap<String, String> = c.inputs.iter().cloned().collect();
        let focus_name = c.inputs.get(c.focus).map(|(n, _)| n.clone());
        let theme = c.theme; // live theme (Ctrl-T chooser previews it)
        let help_open = c.help_open; // Ctrl-G help overlay
        let theme_picker = c.theme_picker_open.then_some(c.theme_picker_sel);
        // Control metadata keyed by name (slider/facet/check bounds + facet cursor).
        let cmeta: HashMap<String, ControlMeta> = c
            .inputs
            .iter()
            .zip(c.control_meta.iter())
            .map(|((n, _), m)| (n.clone(), m.clone()))
            .collect();
        let now = Instant::now();
        let alert_msg = c
            .alert
            .as_ref()
            .filter(|(_, exp)| *exp > now)
            .map(|(m, _)| m.clone());
        c.flashes.retain(|_, (_, exp)| *exp > now);
        let flash_snap: HashMap<String, String> = c
            .flashes
            .iter()
            .map(|(k, (col, _))| (k.clone(), col.clone()))
            .collect();
        let tab_sel_snap: HashMap<String, usize> = c.tab_sel.clone();
        let scroll_snap: HashMap<String, usize> = c.scroll.clone();
        let beep = std::mem::take(&mut c.beep_pending);
        // Control names (index-aligned to inputs) so the hitmap can point a click
        // at the right control_meta slot.
        let control_names: Vec<String> = c.inputs.iter().map(|(n, _)| n.clone()).collect();
        drop(c);
        let st = state.lock().unwrap();
        let mut hitmap: Vec<HitTarget> = Vec::new();
        let draw = terminal.draw(|f| {
            let down_ref = down_snap
                .as_ref()
                .map(|(l, lab)| (l.as_slice(), lab.as_str()));
            render(
                f,
                spec,
                &st,
                &filter,
                down_ref,
                err_ref,
                &inputs,
                focus_name.as_deref(),
                alert_msg.as_deref(),
                &flash_snap,
                &cmeta,
                &mut hitmap,
                &control_names,
                &tab_sel_snap,
                &scroll_snap,
                theme,
                help_open,
                theme_picker,
            );
        });
        drop(st);
        if let Err(e) = draw {
            break Err(e);
        }
        // Publish the frame's hit targets so the key handler can hit-test clicks.
        controls.lock().unwrap().hitmap = hitmap;
        // Ring the terminal bell once after the frame if a `beep` action fired.
        if beep {
            use std::io::Write;
            let _ = terminal.backend_mut().write_all(b"\x07");
            let _ = terminal.backend_mut().flush();
        }
        thread::sleep(Duration::from_millis(120));
    };

    let _ = execute!(terminal.backend_mut(), DisableMouseCapture);
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

// Each parameter is a distinct render input (stream, filter, panes, form state);
// bundling them into a struct would obscure the call site more than it helps.
#[allow(clippy::too_many_arguments)]
fn render(
    f: &mut Frame,
    spec: &Spec,
    st: &StreamState,
    filter: &str,
    down: Option<(&[String], &str)>,
    err: Option<(&[String], &str)>,
    inputs: &HashMap<String, String>,
    focus: Option<&str>,
    alert: Option<&str>,
    flashes: &HashMap<String, String>,
    cmeta: &HashMap<String, ControlMeta>,
    hitmap: &mut Vec<HitTarget>,
    control_names: &[String],
    tab_sel: &HashMap<String, usize>,
    scroll: &HashMap<String, usize>,
    theme: Option<crate::theme::Palette>,
    help: bool,
    theme_picker: Option<usize>,
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
        let cols = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area);
        area = cols[0];
        render_output_pane(f, cols[1], label, dlines);
    }

    let matched = st
        .lines
        .iter()
        .filter(|l| filter_matches(l, filter))
        .count();
    // An active `alert` action takes over the status bar; else the filter hint.
    if let Some(msg) = alert {
        f.render_widget(
            Paragraph::new(format!("  ⚠ {msg}")).style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            bar,
        );
    } else {
        let hint = if filter.is_empty() {
            "  type to filter  ·  Ctrl-T theme  ·  Ctrl-G help  ·  Ctrl-C quit".to_string()
        } else {
            format!("  filter: {filter}▏   {matched}/{} lines", st.lines.len())
        };
        f.render_widget(Paragraph::new(hint), bar);
    }

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
    // Publish each widget's rect + identity for mouse hit-testing.
    hitmap.clear();
    for (i, w) in spec.widgets.iter().enumerate() {
        let name = w.path.trim_start_matches('.').to_string();
        let meta_index = if w.kind.is_control() {
            control_names.iter().position(|n| *n == name)
        } else {
            None
        };
        // Tabs carry their split labels so a click can map a column to an index
        // (filtered like the render arm so indices align).
        let tabs = if w.kind == WidgetKind::Tabs {
            w.opts
                .get("tabs")
                .map(|s| {
                    s.split(',')
                        .filter(|t| !t.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        hitmap.push(HitTarget {
            rect: rects[i],
            kind: w.kind,
            control_name: name,
            meta_index,
            tabs,
        });
    }
    let elapsed = st.start.elapsed().as_secs_f64();
    for (i, w) in spec.widgets.iter().enumerate() {
        // Control widgets are interactive, not stream views: render the live value
        // (slider bar / facet list / check / text) with the focused one highlighted.
        if w.kind.is_control() {
            let name = w.path.trim_start_matches('.');
            let val = inputs.get(name).map(String::as_str).unwrap_or("");
            let default_meta = ControlMeta {
                kind: control_kind(w.kind),
                ..Default::default()
            };
            let meta = cmeta.get(name).unwrap_or(&default_meta);
            render_control(f, rects[i], w, val, meta, &raw, focus == Some(name), theme);
            continue;
        }
        // Resolve `apply .name` placeholders against the live input values before
        // evaluating, so a bound pipeline reflects what the user has typed.
        let result = w.source.as_ref().map(|s| {
            let pipeline = crate::spec::resolve_pipeline(&s.pipeline, inputs);
            eval(&pipeline, &raw, elapsed)
        });
        // A live `flash` action tints this widget's border/accent.
        let flash = flashes
            .get(w.path.trim_start_matches('.'))
            .map(String::as_str);
        let tsel = tab_sel
            .get(w.path.trim_start_matches('.'))
            .copied()
            .unwrap_or(0);
        let wsc = scroll
            .get(w.path.trim_start_matches('.'))
            .copied()
            .unwrap_or(0);
        render_widget(f, rects[i], w, st, &raw, result, flash, tsel, wsc, theme);
    }
    // The Ctrl-G help overlay and the Ctrl-T theme chooser draw on top.
    if help {
        draw_help_overlay(f, spec, theme);
    }
    if let Some(sel) = theme_picker {
        draw_theme_picker(f, sel, theme);
    }
}

/// A centered, theme-accented help overlay (toggled by Ctrl-G) listing the
/// global command keys plus the spec's own `bind` keys.
fn draw_help_overlay(f: &mut Frame, spec: &Spec, theme: Option<crate::theme::Palette>) {
    let accent = theme_accent(theme);
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            "arb — keys",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  Ctrl-T    cycle color theme (saved to ~/.arb)"),
        Line::from("  Ctrl-G    toggle this help"),
        Line::from("  Ctrl-C    quit    ·    Esc  clear / close"),
        Line::from("  ↑ ↓       move a facet/sel/fzf cursor"),
        Line::from("  Tab       cycle inputs · mark an fzf row"),
        Line::from("  wheel     scroll back a tail/list/table"),
    ];
    if !spec.binds.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  spec binds:",
            Style::default().fg(accent),
        )));
        for b in &spec.binds {
            lines.push(Line::from(format!("  {}", key_label(b.key))));
        }
    }
    let w = 52u16.min(f.area().width.saturating_sub(4));
    let h = (lines.len() as u16 + 2).min(f.area().height.saturating_sub(2));
    let area = Rect {
        x: (f.area().width.saturating_sub(w)) / 2,
        y: (f.area().height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(ratatui::widgets::Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .title(" help ");
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// The `Ctrl-T` theme chooser popup (ported from iftoprs): a boxed, scrollable
/// list of the 31 themes — each with a 6-cell palette swatch, a `▸` on the active
/// theme and a highlight bar on the cursor row. The dashboard behind previews the
/// highlighted theme live; `sel` is the cursor, `theme` the (previewed) palette.
fn draw_theme_picker(f: &mut Frame, sel: usize, theme: Option<crate::theme::Palette>) {
    let accent = theme_accent(theme);
    let themes = crate::theme::THEMES;
    let area_full = f.area();
    let w = 40u16.min(area_full.width.saturating_sub(4));
    let h = (themes.len() as u16 + 4).min(area_full.height.saturating_sub(2));
    let area = Rect {
        x: (area_full.width.saturating_sub(w)) / 2,
        y: (area_full.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(ratatui::widgets::Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .title(" theme  (↑↓ preview · Enter save · Esc cancel) ");
    let inner_h = area.height.saturating_sub(2) as usize;
    // Scroll so the cursor row stays visible.
    let start = sel.saturating_sub(inner_h.saturating_sub(1));
    let rows: Vec<Line> = themes
        .iter()
        .enumerate()
        .skip(start)
        .take(inner_h)
        .map(|(i, (name, pal))| {
            let cursor = i == sel;
            let mark = if cursor { "▸ " } else { "  " };
            let name_style = if cursor {
                Style::default().fg(accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut spans = vec![Span::styled(format!("{mark}{name:<14}"), name_style)];
            // 6-cell palette swatch (each cell a 256-color background block).
            for &c in pal.iter() {
                spans.push(Span::styled("  ", Style::default().bg(Color::Indexed(c))));
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(rows).block(block), area);
}

/// A readable label for a raw control-key byte (`0x15` -> `Ctrl-U`, etc.).
fn key_label(key: u8) -> String {
    match key {
        b if (1..=26).contains(&b) => format!("Ctrl-{}", (b'A' + b - 1) as char),
        0x1b => "Esc".into(),
        b => format!("0x{b:02x}"),
    }
}

/// Render an `input .name` widget as an editable field: `label: value▏`, with a
/// cyan border + reversed caret when focused. `placeholder`/`title` opts supply
/// the label and dimmed empty-state hint.
/// The kind of a decoded SGR mouse report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Down,
    Up,
    Drag,
    ScrollUp,
    ScrollDown,
}

/// One decoded SGR mouse event. `col`/`row` are 0-based (converted from the
/// 1-based wire coords) so they index a ratatui `Rect` directly. `button` is the
/// raw SGR button byte (button number in the low 2 bits, modifiers in the high
/// bits) — decode with `mouse_button`/`mouse_shift`/`mouse_ctrl`/`mouse_alt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub kind: MouseKind,
    pub col: u16,
    pub row: u16,
    pub button: u8,
    pub press: bool,
}

/// Which physical button an SGR byte encodes (low 2 bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Other,
}

/// Decode the physical button from an SGR button byte (0=left, 1=middle,
/// 2=right; modifier/motion/wheel bits live higher). Pure.
pub fn mouse_button(b: u8) -> MouseButton {
    match b & 0b11 {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        2 => MouseButton::Right,
        _ => MouseButton::Other,
    }
}

/// SGR modifier predicates over the raw button byte (Shift=0x04, Alt=0x08,
/// Ctrl=0x10). Pure.
pub fn mouse_shift(b: u8) -> bool {
    b & 0x04 != 0
}
pub fn mouse_alt(b: u8) -> bool {
    b & 0x08 != 0
}
pub fn mouse_ctrl(b: u8) -> bool {
    b & 0x10 != 0
}

/// A rendered widget's screen rect + identity, so the key-handler thread can
/// hit-test a click without the spec/area (which live in the render loop).
#[derive(Debug, Clone)]
pub struct HitTarget {
    pub rect: Rect,
    pub kind: WidgetKind,
    /// Widget path minus the leading `.` (matches the input-registry key).
    pub control_name: String,
    /// Index into `Controls.inputs`/`control_meta` when this is a control.
    pub meta_index: Option<usize>,
    /// `-tabs {a b c}` labels for a Tabs widget (empty otherwise), so a tab-bar
    /// click can resolve a column to an index without the spec.
    pub tabs: Vec<String>,
}

/// Parse one SGR mouse report `ESC [ < b ; x ; y (M|m)` starting at byte `i`.
/// Returns the decoded event + total bytes consumed, or `None` if the slice from
/// `i` is not a complete SGR mouse sequence (e.g. truncated at the buffer tail).
/// Pure — no tty. Wire coords are 1-based; converted to 0-based (saturating).
pub fn parse_sgr_mouse(bytes: &[u8], i: usize) -> Option<(MouseEvent, usize)> {
    if bytes.get(i)? != &0x1b || bytes.get(i + 1)? != &b'[' || bytes.get(i + 2)? != &b'<' {
        return None;
    }
    let mut p = i + 3;
    // A `;`-terminated (or final) decimal field.
    let field = |p: &mut usize, want_semi: bool| -> Option<u32> {
        let start = *p;
        let mut v: u32 = 0;
        while let Some(&d) = bytes.get(*p) {
            if d.is_ascii_digit() {
                v = v.checked_mul(10)?.checked_add((d - b'0') as u32)?;
                *p += 1;
            } else {
                break;
            }
        }
        if *p == start {
            return None; // empty field
        }
        if want_semi {
            if bytes.get(*p)? != &b';' {
                return None;
            }
            *p += 1;
        }
        Some(v)
    };
    let b = field(&mut p, true)?;
    let x = field(&mut p, true)?;
    let y = field(&mut p, false)?;
    let press = match bytes.get(p)? {
        b'M' => true,
        b'm' => false,
        _ => return None,
    };
    p += 1;
    let kind = if b & 0x40 != 0 {
        // Wheel: bit0 picks direction (64 = up, 65 = down).
        if b & 0x01 == 0 {
            MouseKind::ScrollUp
        } else {
            MouseKind::ScrollDown
        }
    } else if !press {
        MouseKind::Up
    } else if b & 0x20 != 0 {
        MouseKind::Drag
    } else {
        // Any non-wheel, non-drag press is a button-down (left/middle/right);
        // the specific button is carried in `button` (read via `mouse_button`).
        MouseKind::Down
    };
    let ev = MouseEvent {
        kind,
        col: x.saturating_sub(1).min(u16::MAX as u32) as u16,
        row: y.saturating_sub(1).min(u16::MAX as u32) as u16,
        button: (b & 0xff) as u8,
        press,
    };
    Some((ev, p - i))
}

/// The topmost hit target containing `(col, row)` — last match wins, since a
/// later widget overdraws an earlier one in the layout.
pub fn hit(h: &[HitTarget], col: u16, row: u16) -> Option<&HitTarget> {
    h.iter().rev().find(|t| {
        let r = t.rect;
        col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
    })
}

/// Which facet option row was clicked (`+1` skips the top border); the caller
/// bounds-checks against the option count.
pub fn facet_row_to_index(rect_y: u16, row: u16) -> Option<usize> {
    if row <= rect_y {
        None
    } else {
        Some((row - rect_y - 1) as usize)
    }
}

/// The fzf cursor index for a clicked list row: `start` (the scroll offset of the
/// first visible row) plus the rows below `list_top`.
pub fn fzf_row_to_cursor(list_top: u16, start: usize, row: u16) -> usize {
    start + row.saturating_sub(list_top) as usize
}

/// Which widget kinds honor wheel history-scroll when the wheel is over them.
fn is_scrollable(k: WidgetKind) -> bool {
    matches!(
        k,
        WidgetKind::Tail
            | WidgetKind::List
            | WidgetKind::Text
            | WidgetKind::Table
            | WidgetKind::Block
            | WidgetKind::Frame
    )
}

/// The `skip` for a tail/list/table window ending `scroll` rows above the live
/// bottom: `len-cap` shifted up by `scroll`, clamped so over-scroll parks at the
/// oldest row. `scroll == 0` reproduces the pre-scroll `len.saturating_sub(cap)`.
pub fn scroll_skip(len: usize, cap: usize, scroll: usize) -> usize {
    len.saturating_sub(cap).saturating_sub(scroll)
}

/// Double-click window (a second press this soon after the first, on the same
/// row, is a double-click — the common ~400ms terminal default).
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Whether a press at `row` at time `now` double-clicks the prior press `last`
/// (within the window, same row — the column may drift within a list row). Takes
/// two `Instant`s so it is unit-testable off a synthetic base.
pub fn is_double_click(last: Option<(Instant, u16, u16)>, now: Instant, row: u16) -> bool {
    match last {
        Some((t, _c, r)) => r == row && now.saturating_duration_since(t) <= DOUBLE_CLICK,
        None => false,
    }
}

/// Which tab a click at `col` landed on. ratatui `Tabs` render each label as
/// ` label ` (a space each side) joined by a `|` divider, inside the block's
/// left border — so tab `i` spans `label.len()+2` cols, `+1` for the divider
/// between tabs.
pub fn tab_index_from_x(labels: &[&str], rect_x: u16, col: u16) -> Option<usize> {
    let inner_x = rect_x.saturating_add(1); // block left border
    if col < inner_x {
        return None;
    }
    let mut x = inner_x;
    for (i, label) in labels.iter().enumerate() {
        let last = i + 1 == labels.len();
        let span = label.chars().count() as u16 + if last { 2 } else { 3 };
        if col < x.saturating_add(span) {
            return Some(i);
        }
        x = x.saturating_add(span);
    }
    None
}

/// A slider value from the clicked x, snapped to `step` and clamped to `[min,max]`.
pub fn slider_value_from_x(
    rect_x: u16,
    rect_w: u16,
    col: u16,
    min: f64,
    max: f64,
    step: f64,
) -> String {
    let inner = (rect_w.saturating_sub(2)).max(1) as f64; // border on both sides
    let x = col.saturating_sub(rect_x + 1) as f64;
    let p = (x / inner).clamp(0.0, 1.0);
    let step = if step > 0.0 { step } else { 1.0 };
    let raw = min + p * (max - min);
    let snapped = (min + ((raw - min) / step).round() * step).clamp(min, max);
    crate::query::fmt_scalar(snapped)
}

/// Apply a decoded mouse event to `Controls`: the wheel moves a cursor or scrolls
/// a widget's history; a left press hit-tests to a widget and focuses/toggles/sets
/// it (double-click on an fzf row picks it); a right press resets the hit control;
/// then any `bind <Click>` reactions fire. Pure over `Controls` — `now` is passed
/// in (from the key handler) so double-click stays unit-testable.
fn dispatch_mouse(c: &mut Controls, ev: MouseEvent, fzf: bool, now: Instant) {
    match ev.kind {
        MouseKind::ScrollUp => {
            if fzf {
                c.cursor = c.cursor.saturating_sub(1);
            } else if let Some(name) = hit(&c.hitmap, ev.col, ev.row)
                .filter(|t| is_scrollable(t.kind))
                .map(|t| t.control_name.clone())
            {
                *c.scroll.entry(name).or_insert(0) += 1; // older rows
            } else if let Some(m) = c.control_meta.get_mut(c.focus) {
                if m.kind == ControlKind::Facet {
                    m.cursor = m.cursor.saturating_sub(1);
                }
            }
        }
        MouseKind::ScrollDown => {
            if fzf {
                c.cursor = c.cursor.saturating_add(1);
            } else if let Some(name) = hit(&c.hitmap, ev.col, ev.row)
                .filter(|t| is_scrollable(t.kind))
                .map(|t| t.control_name.clone())
            {
                if let Some(s) = c.scroll.get_mut(&name) {
                    *s = s.saturating_sub(1); // toward the live tail
                }
            } else if let Some(m) = c.control_meta.get_mut(c.focus) {
                if m.kind == ControlKind::Facet {
                    m.cursor = m.cursor.saturating_add(1);
                }
            }
        }
        MouseKind::Down | MouseKind::Drag => {
            let down = ev.kind == MouseKind::Down;
            let button = mouse_button(ev.button);
            let dbl = down && is_double_click(c.last_click, now, ev.row);
            if down {
                c.last_click = Some((now, ev.col, ev.row));
            }
            if let Some(t) = hit(&c.hitmap, ev.col, ev.row).cloned() {
                let mi = t
                    .meta_index
                    .or_else(|| c.inputs.iter().position(|(n, _)| *n == t.control_name));
                if let Some(mi) = mi {
                    c.focus = mi; // focus only ever indexes a real control
                }
                if down && button == MouseButton::Right {
                    // Right-click resets the hit control to its empty/default.
                    if let Some(mi) = mi {
                        let kind = c
                            .control_meta
                            .get(mi)
                            .map(|m| m.kind)
                            .unwrap_or(ControlKind::Text);
                        c.inputs[mi].1 = match kind {
                            ControlKind::Slider => crate::query::fmt_scalar(c.control_meta[mi].min),
                            ControlKind::Check => "0".to_string(),
                            ControlKind::Text | ControlKind::Facet | ControlKind::Sel => {
                                String::new()
                            }
                        };
                        // A reset `sel` also returns its cursor to the top row.
                        if kind == ControlKind::Sel {
                            c.control_meta[mi].cursor = 0;
                        }
                    }
                } else if button == MouseButton::Left {
                    match t.kind {
                        WidgetKind::Check if down => {
                            if let Some(mi) = mi {
                                c.inputs[mi].1 = toggle_check(&c.inputs[mi].1);
                            }
                        }
                        WidgetKind::Slider => {
                            if let Some(mi) = mi {
                                let m = &c.control_meta[mi];
                                let (mn, mx, sp) = (m.min, m.max, m.step);
                                c.inputs[mi].1 =
                                    slider_value_from_x(t.rect.x, t.rect.width, ev.col, mn, mx, sp);
                            }
                        }
                        WidgetKind::Facet if down => {
                            if let Some(mi) = mi {
                                if let Some(idx) = facet_row_to_index(t.rect.y, ev.row) {
                                    if let Some(item) = c.control_meta[mi].opts.get(idx).cloned() {
                                        c.inputs[mi].1 = toggle_set_member(&c.inputs[mi].1, &item);
                                        c.control_meta[mi].cursor = idx;
                                    }
                                }
                            }
                        }
                        WidgetKind::Select if down && ev.row >= t.rect.y => {
                            c.cursor = fzf_row_to_cursor(t.rect.y, c.fzf_list_start, ev.row);
                            if dbl {
                                c.submit = true; // double-click picks the row, like Enter
                            }
                        }
                        WidgetKind::Sel if down => {
                            // Click a row to move the selection cursor to it.
                            if let Some(mi) = mi {
                                if let Some(idx) = facet_row_to_index(t.rect.y, ev.row) {
                                    c.control_meta[mi].cursor = idx;
                                }
                            }
                        }
                        WidgetKind::Tabs if down => {
                            let labels: Vec<&str> = t.tabs.iter().map(String::as_str).collect();
                            if let Some(idx) = tab_index_from_x(&labels, t.rect.x, ev.col) {
                                c.tab_sel.insert(t.control_name.clone(), idx);
                            }
                        }
                        _ => {} // input/filter focus set above; view widgets no-op
                    }
                }
                // Middle button: focus only (set above), no widget action.
            }
            // `bind <Click>` reactions fire on any button press (not a drag).
            if down {
                let actions: Vec<BindAction> =
                    c.mouse_binds.iter().map(|(_, a)| a.clone()).collect();
                for a in &actions {
                    apply_bind_action(c, a);
                }
            }
        }
        MouseKind::Up => {}
    }
}

/// Update `last` to `cur` and report whether the terminal size changed — drives
/// `bind <Resize>` (polled each frame; no SIGWINCH handler needed).
pub fn detect_resize(last: &mut (u16, u16), cur: (u16, u16)) -> bool {
    let changed = *last != cur;
    *last = cur;
    changed
}

/// The interaction kind of a control widget (its value always lives in the
/// string input registry; this drives key handling + render).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ControlKind {
    /// `input`/`filter`: free text.
    #[default]
    Text,
    /// `slider`: a number in `[min, max]`, arrows/`+`/`-` adjust by `step`.
    Slider,
    /// `check`: a boolean, Space toggles ("1"/"0").
    Check,
    /// `facet`: a comma-set selected from `opts`, Up/Down move, Space toggles.
    Facet,
    /// `sel`: a single-select list over the widget's own `source`, Up/Down move
    /// the cursor; the highlighted row is the value, published as `.<path>.sel`.
    Sel,
}

/// Per-control metadata parallel to `Controls.inputs`, for key handling + render.
#[derive(Debug, Clone, Default)]
pub struct ControlMeta {
    pub kind: ControlKind,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub opts: Vec<String>,
    pub cursor: usize,
}

/// Map a widget kind to its control interaction kind.
pub fn control_kind(k: WidgetKind) -> ControlKind {
    match k {
        WidgetKind::Slider => ControlKind::Slider,
        WidgetKind::Check => ControlKind::Check,
        WidgetKind::Facet => ControlKind::Facet,
        WidgetKind::Sel => ControlKind::Sel,
        _ => ControlKind::Text, // input, filter
    }
}

/// Adjust the focused slider control's value by one step.
fn slider_key(c: &mut Controls, up: bool) {
    let f = c.focus;
    if let Some(m) = c.control_meta.get(f) {
        let (min, max, step) = (m.min, m.max, m.step);
        c.inputs[f].1 = slider_adjust(&c.inputs[f].1, min, max, step, up);
    }
}

/// Adjust a slider value by one step, clamped to `[min, max]`.
pub fn slider_adjust(cur: &str, min: f64, max: f64, step: f64, up: bool) -> String {
    let v = cur.trim().parse::<f64>().unwrap_or(min);
    let step = if step > 0.0 { step } else { 1.0 };
    let next = if up { v + step } else { v - step }.clamp(min, max);
    crate::query::fmt_scalar(next)
}

/// Toggle a boolean control value ("1" <-> "0").
pub fn toggle_check(cur: &str) -> String {
    if cur == "1" {
        "0".to_string()
    } else {
        "1".to_string()
    }
}

/// Toggle `item`'s membership in a comma-separated set; returns the new set.
pub fn toggle_set_member(set: &str, item: &str) -> String {
    let mut items: Vec<String> = set
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if let Some(pos) = items.iter().position(|x| x == item) {
        items.remove(pos);
    } else {
        items.push(item.to_string());
    }
    items.join(",")
}

/// A facet's candidate values: explicit `-opts`, else the distinct values of
/// its `-field` across the current stream (bounded).
pub fn facet_candidates(w: &Widget, raw: &[String]) -> Vec<String> {
    if let Some(opts) = w.opts.get("opts") {
        return opts
            .split(',')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
    }
    let Some(field) = w.opts.get("field") else {
        return Vec::new();
    };
    let mut seen = Vec::new();
    for line in raw {
        let v = crate::query::field_str_pub(line, field);
        if !v.is_empty() && !seen.contains(&v) {
            seen.push(v);
            if seen.len() >= 32 {
                break;
            }
        }
    }
    seen
}

/// Extract `YYYY-MM-DD` dates appearing anywhere in the stream lines, for the
/// `calendar` widget to highlight days with activity. Scans each line for an
/// 8–10 char ISO date token; malformed dates are skipped.
fn stream_event_dates(lines: &[String]) -> Vec<time::Date> {
    let mut out = Vec::new();
    for l in lines {
        for tok in l.split(|c: char| !(c.is_ascii_digit() || c == '-')) {
            let parts: Vec<&str> = tok.split('-').collect();
            if parts.len() != 3 {
                continue;
            }
            let (Ok(y), Ok(m), Ok(d)) = (
                parts[0].parse::<i32>(),
                parts[1].parse::<u8>(),
                parts[2].parse::<u8>(),
            ) else {
                continue;
            };
            if let Ok(month) = time::Month::try_from(m) {
                if let Ok(date) = time::Date::from_calendar_date(y, month, d) {
                    if !out.contains(&date) {
                        out.push(date);
                    }
                }
            }
        }
    }
    out
}

/// The candidate rows of a `sel` widget: its `source` pipeline evaluated over the
/// stream (or the raw stream when it has none). The highlighted row is the value.
pub fn sel_candidates(w: &Widget, raw: &[String]) -> Vec<String> {
    match &w.source {
        Some(s) => match crate::query::eval(&s.pipeline, raw, 0.0) {
            crate::query::QueryResult::Lines(ls) => ls,
            crate::query::QueryResult::Scalar(v) => vec![crate::query::fmt_scalar(v)],
            crate::query::QueryResult::Pairs(p) => {
                p.iter().map(|(k, v)| format!("{k}\t{v}")).collect()
            }
        },
        None => raw.to_vec(),
    }
}

/// Before each frame, publish every `sel` widget's highlighted row into its
/// `.<path>.sel` control, clamping the cursor to the live candidate count — so a
/// `where`/`apply`/`tell` that reads `.<path>.sel` sees the current selection.
/// Stream lines are snapshotted by the caller (never holds the state + controls
/// locks together — the reader/key deadlock discipline).
pub fn update_sel_controls(spec: &Spec, raw: &[String], c: &mut Controls) {
    for w in spec.widgets.iter().filter(|w| w.kind == WidgetKind::Sel) {
        let key = format!("{}.sel", w.path.trim_start_matches('.'));
        let Some(idx) = c.inputs.iter().position(|(n, _)| *n == key) else {
            continue;
        };
        let cands = sel_candidates(w, raw);
        let cursor = if cands.is_empty() {
            0
        } else {
            c.control_meta[idx].cursor.min(cands.len() - 1)
        };
        c.control_meta[idx].cursor = cursor;
        c.inputs[idx].1 = cands.get(cursor).cloned().unwrap_or_default();
    }
}

fn render_input(f: &mut Frame, area: Rect, w: &Widget, val: &str, focused: bool, accent: Color) {
    let label = w
        .opts
        .get("title")
        .or_else(|| w.opts.get("placeholder"))
        .map(String::as_str)
        .unwrap_or_else(|| w.path.trim_start_matches('.'));
    let border = if focused {
        accent
    } else {
        Color::DarkGray
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(format!(" {label} "));
    let body = if focused {
        Line::from(vec![
            Span::raw(val.to_string()),
            Span::styled("▏", Style::default().fg(accent)),
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

/// Render an interactive control (slider/check/facet); text kinds delegate to
/// [`render_input`]. `meta` is the control's parallel metadata; `raw` is the live
/// stream (for a `-field` facet's candidates).
#[allow(clippy::too_many_arguments)]
fn render_control(
    f: &mut Frame,
    area: Rect,
    w: &Widget,
    val: &str,
    meta: &ControlMeta,
    raw: &[String],
    focused: bool,
    theme: Option<crate::theme::Palette>,
) {
    let accent = theme_accent(theme);
    let label = w
        .opts
        .get("label")
        .or_else(|| w.opts.get("title"))
        .map(String::as_str)
        .unwrap_or_else(|| w.path.trim_start_matches('.'));
    let border = if focused {
        accent
    } else {
        Color::DarkGray
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(format!(" {label} "));
    match meta.kind {
        ControlKind::Text => render_input(f, area, w, val, focused, accent),
        ControlKind::Slider => {
            let v = val.trim().parse::<f64>().unwrap_or(meta.min);
            let span = (meta.max - meta.min).max(f64::MIN_POSITIVE);
            let filled = (((v - meta.min) / span) * 20.0).round().clamp(0.0, 20.0) as usize;
            let bar: String = "█".repeat(filled) + &"─".repeat(20 - filled);
            let body = Line::from(vec![
                Span::styled(bar, Style::default().fg(accent)),
                Span::raw(format!("  {}", crate::query::fmt_scalar(v))),
            ]);
            f.render_widget(Paragraph::new(body).block(block), area);
        }
        ControlKind::Check => {
            let on = val == "1";
            let body = Line::from(Span::styled(
                format!("[{}] {label}", if on { "x" } else { " " }),
                Style::default().fg(if on { accent } else { Color::Gray }),
            ));
            f.render_widget(Paragraph::new(body).block(block), area);
        }
        ControlKind::Facet => {
            let cands = facet_candidates(w, raw);
            let selected: Vec<&str> = val
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            let items: Vec<ListItem> = cands
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let mark = if selected.contains(&c.as_str()) {
                        "[x] "
                    } else {
                        "[ ] "
                    };
                    let style = if focused && i == meta.cursor {
                        Style::default()
                            .fg(accent)
                            .add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    };
                    ListItem::new(Line::from(Span::styled(format!("{mark}{c}"), style)))
                })
                .collect();
            f.render_widget(List::new(items).block(block), area);
        }
        ControlKind::Sel => {
            // Single-select list over the widget's own source; the cursor row is
            // the value (`.<path>.sel`). A `▸` marks it; when focused it reverses.
            let cands = sel_candidates(w, raw);
            let inner_h = area.height.saturating_sub(2) as usize;
            let skip = meta.cursor.saturating_sub(inner_h.saturating_sub(1));
            let items: Vec<ListItem> = cands
                .iter()
                .enumerate()
                .skip(skip)
                .take(inner_h)
                .map(|(i, row)| {
                    let cur = i == meta.cursor;
                    let mark = if cur { "▸ " } else { "  " };
                    let mut style = Style::default();
                    if cur {
                        style = style.fg(accent);
                        if focused {
                            style = style.add_modifier(Modifier::REVERSED);
                        } else {
                            style = style.add_modifier(Modifier::BOLD);
                        }
                    }
                    ListItem::new(Line::from(Span::styled(format!("{mark}{row}"), style)))
                })
                .collect();
            f.render_widget(List::new(items).block(block), area);
        }
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
fn fzf_line(
    line: &str,
    filter: &str,
    width: usize,
    marked: bool,
    base: Option<Color>,
    accent: Color,
) -> Line<'static> {
    let text: String = line.chars().take(width.saturating_sub(2)).collect();
    // Base row-text style: the theme's primary color, or terminal default.
    let base_style = base.map(|c| Style::default().fg(c)).unwrap_or_default();
    let base_span = |s: String| Span::styled(s, base_style);
    let gutter = if marked {
        Span::styled(
            "+ ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("  ")
    };
    if filter.is_empty() {
        return Line::from(vec![gutter, base_span(text)]);
    }
    let pos: std::collections::HashSet<usize> =
        match_positions(&text, filter).into_iter().collect();
    // Matched characters glow in the theme accent (was a fixed yellow).
    let hl = Style::default().fg(accent).add_modifier(Modifier::BOLD);
    let mut spans = vec![gutter];
    let mut cur = String::new();
    let mut cur_hl = false;
    for (i, ch) in text.chars().enumerate() {
        let h = pos.contains(&i);
        if h != cur_hl && !cur.is_empty() {
            let s = std::mem::take(&mut cur);
            spans.push(if cur_hl { Span::styled(s, hl) } else { base_span(s) });
        }
        cur_hl = h;
        cur.push(ch);
    }
    if !cur.is_empty() {
        spans.push(if cur_hl {
            Span::styled(cur, hl)
        } else {
            base_span(cur)
        });
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

// Distinct render inputs (matches, filter, cursor, marks, panes, prompt/header).
#[allow(clippy::too_many_arguments)]
fn render_fzf(
    f: &mut Frame,
    matched: &[(Arc<str>, Arc<str>)],
    filter: &str,
    sel: usize,
    marks: &[String],
    total: u64,
    err: Option<(&[String], &str)>,
    preview: Option<(&[String], &str)>,
    prompt: &str,
    header: &str,
    theme: Option<crate::theme::Palette>,
    hitmap: &mut Vec<HitTarget>,
) -> usize {
    // Reserve a bottom strip for the stderr pane when present.
    let (top, err_area) = match err {
        Some((lines, _)) => {
            let h = ((lines.len() as u16) + 2).clamp(3, 10);
            let rows =
                Layout::vertical([Constraint::Min(0), Constraint::Length(h)]).split(f.area());
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
    // The whole picker recolors from the active palette (not just the accent):
    // row text in `base` (primary), the prompt/matches/cursor in `accent`, the
    // header/counter/hints in `dim`, and the cursor bar in the palette `bg`. With
    // no theme these fall back to the classic cyan/default/gray look.
    let accent = theme_accent(theme);
    let base = theme.map(|p| p.primary()); // None -> terminal default text
    let dim = theme.map(|p| p.dim()).unwrap_or(Color::DarkGray);
    let bar_bg = theme.map(|p| p.bg()).unwrap_or(Color::Rgb(38, 38, 46));
    let query_style = base.map(|c| Style::default().fg(c)).unwrap_or_default();
    let acc = Style::default().fg(accent);
    let prompt_line = Line::from(vec![
        Span::styled(prompt.to_string(), acc.add_modifier(Modifier::BOLD)),
        Span::styled(format!("{filter}\u{258f}"), query_style),
        Span::styled(
            format!("   {}/{total}{marked}", matched.len()),
            Style::default().fg(dim),
        ),
        Span::styled(
            "   Enter select · Tab mark · \u{2191}\u{2193} move · Ctrl-T theme · Ctrl-C abort",
            Style::default().fg(dim),
        ),
    ]);
    f.render_widget(Paragraph::new(prompt_line), chunks[0]);
    if !header.is_empty() {
        f.render_widget(
            Paragraph::new(header).style(Style::default().fg(dim)),
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
            ListItem::new(fzf_line(
                disp,
                filter,
                inner_w,
                mark_set.contains(orig.as_ref()),
                base,
                accent,
            ))
        })
        .collect();
    let mut state = ListState::default();
    if n > 0 {
        state.select(Some(sel - start));
    }
    // Cursor line: accent text + the `▶` pointer over a bar in the palette `bg`,
    // so the selected row reads as themed even before you type a query.
    let list = List::new(items)
        .highlight_symbol("\u{25b6} ")
        .highlight_style(
            Style::default()
                .fg(accent)
                .bg(bar_bg)
                .add_modifier(Modifier::BOLD),
        );
    f.render_stateful_widget(list, chunks[2], &mut state);
    // Publish the list body so a click maps to a cursor row (see dispatch_mouse
    // Select arm). `start` is the scroll offset of the first visible row.
    hitmap.clear();
    hitmap.push(HitTarget {
        rect: chunks[2],
        kind: WidgetKind::Select,
        control_name: String::new(),
        meta_index: None,
        tabs: Vec::new(),
    });
    start
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

/// Map a [`Track`] to its ratatui `Constraint`.
fn track_to_constraint(t: crate::spec::Track) -> Constraint {
    use crate::spec::Track;
    match t {
        Track::Length(n) => Constraint::Length(n),
        Track::Percentage(n) => Constraint::Percentage(n),
        Track::Fill(w) => Constraint::Fill(w),
    }
}

/// Build `n` track constraints: index `i` uses `tracks[i]` when given, else an
/// equal-weight `Fill(1)`. A shorter spec sizes the leading tracks and lets the
/// rest fill; a longer one is ignored past `n`.
fn track_cons(tracks: Option<&Vec<crate::spec::Track>>, n: usize) -> Vec<Constraint> {
    (0..n)
        .map(|i| {
            tracks
                .and_then(|t| t.get(i))
                .map(|&t| track_to_constraint(t))
                .unwrap_or(Constraint::Fill(1))
        })
        .collect()
}

pub fn compute_rects(area: Rect, spec: &Spec) -> Vec<Rect> {
    use crate::spec::Flow;
    let ws = &spec.widgets;
    let grid_mode = ws.iter().any(|w| w.grid.is_some());
    if !grid_mode {
        // Auto-tile in the `layout` direction with `gap` spacing; the flow-axis
        // track spec (`rows` for vertical, `cols` for horizontal) sizes the tiles
        // when given, else they split evenly.
        let n = ws.len().max(1);
        let lay = match spec.flow {
            Flow::Vertical => Layout::vertical(track_cons(spec.row_tracks.as_ref(), n)),
            Flow::Horizontal => Layout::horizontal(track_cons(spec.col_tracks.as_ref(), n)),
        };
        return lay.spacing(spec.gap).split(area).to_vec();
    }
    // Each widget occupies `(row, col)` and spans `(rowspan, colspan)` cells.
    let cells: Vec<(usize, usize, usize, usize)> = ws
        .iter()
        .map(|w| {
            let (r, c) = w.grid.unwrap_or((0, 0));
            let (rs, cs) = w.span;
            (r, c, rs.max(1), cs.max(1))
        })
        .collect();
    let rows = cells
        .iter()
        .map(|(r, _, rs, _)| r + rs)
        .max()
        .unwrap_or(1)
        .max(1)
        .max(spec.row_tracks.as_ref().map_or(0, |t| t.len()));
    let cols = cells
        .iter()
        .map(|(_, c, _, cs)| c + cs)
        .max()
        .unwrap_or(1)
        .max(1)
        .max(spec.col_tracks.as_ref().map_or(0, |t| t.len()));
    // Proportional row/column tracks (`rows`/`cols`), with `gap` cells between.
    let row_chunks = Layout::vertical(track_cons(spec.row_tracks.as_ref(), rows))
        .spacing(spec.gap)
        .split(area);
    let col_cons = track_cons(spec.col_tracks.as_ref(), cols);
    cells
        .iter()
        .map(|&(r, c, rs, cs)| {
            // Vertical extent: rows r .. r+rs; horizontal: cols c .. c+cs.
            let top = row_chunks[r.min(rows - 1)];
            let bottom = row_chunks[(r + rs - 1).min(rows - 1)];
            let y = top.y;
            let height = bottom.y + bottom.height - top.y;
            let band = Rect {
                x: area.x,
                y,
                width: area.width,
                height,
            };
            let col_chunks = Layout::horizontal(col_cons.clone())
                .spacing(spec.gap)
                .split(band);
            let left = col_chunks[c.min(cols - 1)];
            let right = col_chunks[(c + cs - 1).min(cols - 1)];
            Rect {
                x: left.x,
                y,
                width: right.x + right.width - left.x,
                height,
            }
        })
        .collect()
}

/// A widget's row cap from `-limit N` (alias `-lines N`), if any — how many rows
/// a `list`/`tail` shows at most. Shared with the web dashboard so both agree.
pub fn widget_limit(w: &Widget) -> Option<usize> {
    w.opts
        .get("limit")
        .or_else(|| w.opts.get("lines"))
        .and_then(|s| s.parse::<usize>().ok())
}

/// Parse a `#rrggbb` hex string (from [`crate::spec::color_hex`]) into a ratatui
/// RGB color; falls back to cyan on any malformed input.
fn hex_color(hex: &str) -> Color {
    let h = hex.trim_start_matches('#');
    if h.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&h[0..2], 16),
            u8::from_str_radix(&h[2..4], 16),
            u8::from_str_radix(&h[4..6], 16),
        ) {
            return Color::Rgb(r, g, b);
        }
    }
    Color::Cyan
}

/// Resolve a widget's accent color, theme-aware. Precedence: an explicit theme
/// palette slot (`-color accent|primary|alt|mid|dim|bg`) → a fixed semantic name
/// (`-color green`, theme-independent) → otherwise the theme accent when a theme
/// is active, else the classic cyan default. Backward-compatible: with no theme
/// and a semantic/absent name this is exactly the old `hex_color(color_hex(..))`.
fn resolve_accent(name: Option<&str>, theme: Option<crate::theme::Palette>) -> Color {
    if let Some(n) = name {
        let nl = n.trim().to_ascii_lowercase();
        if let Some(p) = theme {
            if let Some(col) = p.slot(&nl) {
                return col;
            }
        }
        if crate::spec::is_named_color(&nl) {
            return hex_color(crate::spec::color_hex(Some(&nl)));
        }
        // Unknown name: fall through to the theme accent / cyan default.
    }
    match theme {
        Some(p) => p.accent(),
        None => hex_color(crate::spec::color_hex(None)),
    }
}

/// The theme-aware focus/highlight accent (focused control borders, fzf cursor,
/// prompt) — the theme accent when set, else cyan.
fn theme_accent(theme: Option<crate::theme::Palette>) -> Color {
    theme.map(|p| p.accent()).unwrap_or(Color::Cyan)
}

/// Set the live theme to `idx` of the 31 built-ins (used for the chooser's live
/// preview — the dashboard behind the popup recolors as the cursor moves).
fn set_live_theme(c: &mut Controls, idx: usize) {
    let n = crate::theme::THEMES.len();
    c.theme_idx = idx % n;
    c.theme = Some(crate::theme::Palette {
        c: crate::theme::THEMES[c.theme_idx].1,
    });
}

/// Open the `Ctrl-T` theme chooser, remembering the current theme to revert to on
/// cancel (ported from iftoprs' theme picker).
fn open_theme_picker(c: &mut Controls) {
    c.theme_picker_open = true;
    c.theme_picker_sel = c.theme_idx;
    c.theme_picker_revert = c.theme_idx;
}

/// Move the chooser cursor by `delta` (wrapping) and live-preview that theme.
fn theme_picker_move(c: &mut Controls, delta: isize) {
    let n = crate::theme::THEMES.len() as isize;
    c.theme_picker_sel = (((c.theme_picker_sel as isize + delta) % n + n) % n) as usize;
    set_live_theme(c, c.theme_picker_sel);
}

/// Accept the highlighted theme: persist it to `~/.arb`, flash the name, close.
fn theme_picker_accept(c: &mut Controls) {
    set_live_theme(c, c.theme_picker_sel);
    let name = crate::theme::THEMES[c.theme_idx].0;
    let _ = crate::theme::set_config_default(name);
    c.alert = Some((format!("theme: {name}"), Instant::now() + Duration::from_secs(2)));
    c.theme_picker_open = false;
}

/// Cancel the chooser: revert to the theme active when it was opened, close.
fn theme_picker_cancel(c: &mut Controls) {
    set_live_theme(c, c.theme_picker_revert);
    c.theme_picker_open = false;
}

/// The default palette slot for a widget with no explicit `-color`, chosen by
/// kind so a themed dashboard is multi-colored (like the iftop/htop HUD) instead
/// of monochrome-accent — value gauges in the accent, bars in the alt hue,
/// series/plots in the mid tone, text/containers in the primary. Used only when a
/// theme is active; without one every widget stays the classic cyan default.
fn default_slot_for_kind(kind: WidgetKind, p: crate::theme::Palette) -> Color {
    use WidgetKind::*;
    match kind {
        Gauge | LineGauge => p.accent(),
        Bars | Histo => p.alt(),
        Spark | Sparkline | Scatter | Chart | Map => p.mid(),
        Calendar => p.mid(),
        Text | Tail | List | Table | Tabs | Block | Frame => p.primary(),
        // Controls carry their own focus accent elsewhere.
        _ => p.accent(),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_widget(
    f: &mut Frame,
    area: Rect,
    w: &Widget,
    st: &StreamState,
    lines: &[String],
    result: Option<QueryResult>,
    flash: Option<&str>,
    tab_sel: usize,
    scroll: usize,
    theme: Option<crate::theme::Palette>,
) {
    // `-label`/`-title` overrides the widget's display name (the dot-path).
    let name = w
        .opts
        .get("label")
        .or_else(|| w.opts.get("title"))
        .map(String::as_str)
        .unwrap_or(&w.path);
    let title = format!(
        " {} · {} · {} ln {:.0}/s ",
        name,
        w.kind.label(),
        st.total,
        st.rate()
    );
    // Per-widget accent (`-color NAME`): tints the border and each kind's accent
    // element (gauge/bar fill, spark, chart line, table header). Default cyan. A
    // live `flash` action temporarily overrides the color.
    let color_name = flash.or_else(|| w.opts.get("color").map(String::as_str));
    // Explicit `-color` (slot or semantic) resolves as given; with none, a themed
    // dashboard picks a palette slot by widget kind (multi-color HUD), else cyan.
    let accent = match color_name {
        Some(_) => resolve_accent(color_name, theme),
        None => match theme {
            Some(p) => default_slot_for_kind(w.kind, p),
            None => resolve_accent(None, None),
        },
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .title(title);
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
                Some(QueryResult::Pairs(p)) => p.iter().map(|(k, v)| format!("{k}  {v}")).collect(),
                None => lines.to_vec(),
            };
            // `-limit N` (alias `-lines N`) caps the rows shown to the last N,
            // even when more would fit; unset fills the pane.
            let inner_h = area.height.saturating_sub(2) as usize;
            let cap = widget_limit(w).map_or(inner_h, |n| inner_h.min(n));
            // Wheel scrollback shifts the window up from the live tail.
            let skip = scroll_skip(owned.len(), cap, scroll);
            let items: Vec<ListItem> = owned
                .iter()
                .skip(skip)
                .take(cap)
                .map(|l| ListItem::new(ansi_line(l)))
                .collect();
            f.render_widget(List::new(items).block(block), area);
            render_scrollbar(f, area, owned.len(), cap, skip, accent);
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
            let ratio = if max > 0.0 {
                (val / max).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let g = Gauge::default()
                .block(block)
                .gauge_style(Style::default().fg(accent))
                .ratio(ratio)
                .label(format!("{val:.0}/{max:.0}"));
            f.render_widget(g, area);
        }
        WidgetKind::LineGauge => {
            // A thin one-line progress bar — same scalar source + `-max` as gauge,
            // for tight cells where a full gauge is too tall.
            let val = match &result {
                Some(QueryResult::Scalar(v)) => *v,
                _ => 0.0,
            };
            let max = w
                .opts
                .get("max")
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(100.0);
            let ratio = if max > 0.0 {
                (val / max).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let g = LineGauge::default()
                .block(block)
                .filled_style(Style::default().fg(accent))
                .line_set(symbols::line::THICK)
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
            let shown: Vec<(&str, u64)> = pairs
                .iter()
                .take(top)
                .map(|(k, v)| (k.as_str(), *v))
                .collect();
            let n = shown.len().max(1);
            let inner_w = area.width.saturating_sub(2) as usize;
            let bw = ((inner_w / n).saturating_sub(1)).clamp(1, 12) as u16;
            let chart = BarChart::default()
                .block(block)
                .bar_style(Style::default().fg(accent))
                .bar_width(bw)
                .bar_gap(1)
                .data(&shown[..]);
            f.render_widget(chart, area);
        }
        WidgetKind::Chart => {
            let series: Vec<f64> = match &result {
                Some(QueryResult::Pairs(p)) => p.iter().map(|(_, v)| *v as f64).collect(),
                Some(QueryResult::Lines(ls)) => crate::query::numeric_series(ls),
                Some(QueryResult::Scalar(v)) => vec![*v],
                None => crate::query::numeric_series(lines),
            };
            let points: Vec<(f64, f64)> = series
                .iter()
                .enumerate()
                .map(|(i, v)| (i as f64, *v))
                .collect();
            let min = series.iter().cloned().fold(f64::INFINITY, f64::min);
            let max = series.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            // Pad a flat/empty range so the line has somewhere to sit.
            let (lo, hi) = if !min.is_finite() || min == max {
                (min.min(0.0), max.max(min + 1.0))
            } else {
                (min, max)
            };
            let xmax = (series.len().saturating_sub(1)).max(1) as f64;
            let datasets = vec![Dataset::default()
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(accent))
                .data(&points)];
            let chart = Chart::new(datasets)
                .block(block)
                .x_axis(Axis::default().bounds([0.0, xmax]))
                .y_axis(Axis::default().bounds([lo, hi]));
            f.render_widget(chart, area);
        }
        WidgetKind::Scatter => {
            // A braille scatter of a numeric series (higher spatial resolution than
            // `spark`, no axes chrome). Each value plots at (index, value); the
            // canvas bounds auto-fit the data.
            let series: Vec<f64> = match &result {
                Some(QueryResult::Pairs(p)) => p.iter().map(|(_, v)| *v as f64).collect(),
                Some(QueryResult::Lines(ls)) => crate::query::numeric_series(ls),
                Some(QueryResult::Scalar(v)) => vec![*v],
                None => crate::query::numeric_series(lines),
            };
            let coords: Vec<(f64, f64)> = series
                .iter()
                .enumerate()
                .map(|(i, v)| (i as f64, *v))
                .collect();
            let min = series.iter().cloned().fold(f64::INFINITY, f64::min);
            let max = series.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let (lo, hi) = if !min.is_finite() || min == max {
                (min.min(0.0), max.max(min + 1.0))
            } else {
                (min, max)
            };
            let xmax = (series.len().saturating_sub(1)).max(1) as f64;
            let canvas = Canvas::default()
                .block(block)
                .marker(symbols::Marker::Braille)
                .x_bounds([0.0, xmax])
                .y_bounds([lo, hi])
                .paint(move |ctx| {
                    ctx.draw(&Points {
                        coords: &coords,
                        color: accent,
                    });
                });
            f.render_widget(canvas, area);
        }
        WidgetKind::Sparkline => {
            // The classic block-bar sparkline (ratatui `Sparkline`) — fixed-height
            // bars scaled to the series max. Distinct from `spark`'s braille line.
            let series: Vec<f64> = match &result {
                Some(QueryResult::Pairs(p)) => p.iter().map(|(_, v)| *v as f64).collect(),
                Some(QueryResult::Lines(ls)) => crate::query::numeric_series(ls),
                Some(QueryResult::Scalar(v)) => vec![*v],
                None => crate::query::numeric_series(lines),
            };
            // Newest points that fit the width; non-finite/negative clamp to 0.
            let cap = inner_w.max(1);
            let start = series.len().saturating_sub(cap);
            let data: Vec<u64> = series[start..]
                .iter()
                .map(|v| if v.is_finite() && *v > 0.0 { *v as u64 } else { 0 })
                .collect();
            let sp = Sparkline::default()
                .block(block)
                .style(Style::default().fg(accent))
                .data(&data);
            f.render_widget(sp, area);
        }
        WidgetKind::Map => {
            // A braille world map (ratatui `Canvas` + `Map`) with `lon lat` points
            // from the stream — the first two numeric fields of each line. `-res
            // high|low` picks the coastline resolution.
            let res = match w.opts.get("res").map(String::as_str) {
                Some("low") => MapResolution::Low,
                _ => MapResolution::High,
            };
            let pts: Vec<(f64, f64)> = lines
                .iter()
                .filter_map(|l| {
                    let mut it = l.split_whitespace();
                    let lon = it.next()?.parse::<f64>().ok()?;
                    let lat = it.next()?.parse::<f64>().ok()?;
                    (lon.abs() <= 180.0 && lat.abs() <= 90.0).then_some((lon, lat))
                })
                .collect();
            let canvas = Canvas::default()
                .block(block)
                .marker(symbols::Marker::Braille)
                .x_bounds([-180.0, 180.0])
                .y_bounds([-90.0, 90.0])
                .paint(move |ctx| {
                    ctx.draw(&Map {
                        resolution: res,
                        color: Color::DarkGray,
                    });
                    ctx.layer();
                    ctx.draw(&Points {
                        coords: &pts,
                        color: accent,
                    });
                });
            f.render_widget(canvas, area);
        }
        WidgetKind::Calendar => {
            // The current month (ratatui `Monthly`); days that appear as
            // `YYYY-MM-DD` anywhere in a stream line are highlighted (activity).
            let today = time::OffsetDateTime::now_utc().date();
            let mut events = CalendarEventStore::default();
            events.add(
                today,
                Style::default().fg(Color::Black).bg(accent),
            );
            for d in stream_event_dates(lines) {
                if d != today {
                    events.add(d, Style::default().fg(accent).add_modifier(Modifier::BOLD));
                }
            }
            let cal = Monthly::new(today, events)
                .block(block)
                .show_month_header(Style::default().fg(accent))
                .show_weekdays_header(Style::default().fg(Color::DarkGray));
            f.render_widget(cal, area);
        }
        WidgetKind::Spark => {
            let series: Vec<f64> = match &result {
                Some(QueryResult::Pairs(p)) => p.iter().map(|(_, v)| *v as f64).collect(),
                Some(QueryResult::Lines(ls)) => crate::query::numeric_series(ls),
                Some(QueryResult::Scalar(v)) => vec![*v],
                None => crate::query::numeric_series(lines),
            };
            // Keep only the newest points that fit the width.
            let cap = (area.width as usize).saturating_sub(2).max(1);
            let start = series.len().saturating_sub(cap);
            let spark = crate::query::sparkline(&series[start..]);
            f.render_widget(
                Paragraph::new(spark)
                    .style(Style::default().fg(accent))
                    .block(block),
                area,
            );
        }
        WidgetKind::Table => {
            let src_lines: Vec<String> = match &result {
                Some(QueryResult::Lines(ls)) => ls.clone(),
                Some(QueryResult::Pairs(p)) => p.iter().map(|(k, v)| format!("{k} {v}")).collect(),
                _ => lines.to_vec(),
            };
            let (headers, rows) =
                crate::query::table_data(&src_lines, w.opts.get("cols").map(String::as_str));
            let ncols = crate::query::table_ncols(&headers, &rows);
            let widths: Vec<Constraint> = (0..ncols)
                .map(|_| Constraint::Ratio(1, ncols as u32))
                .collect();
            // Keep the newest rows that fit (leave room for borders + header).
            let reserve = if headers.is_empty() { 2 } else { 3 };
            let inner_h = area.height.saturating_sub(reserve) as usize;
            let skip = scroll_skip(rows.len(), inner_h, scroll);
            let body: Vec<Row> = rows
                .iter()
                .skip(skip)
                .take(inner_h)
                .map(|r| {
                    Row::new(
                        (0..ncols)
                            .map(|i| Cell::from(r.get(i).cloned().unwrap_or_default()))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();
            let mut table = Table::new(body, widths).block(block);
            if !headers.is_empty() {
                table = table.header(
                    Row::new(
                        headers
                            .iter()
                            .map(|h| Cell::from(h.clone()))
                            .collect::<Vec<_>>(),
                    )
                    .style(Style::default().fg(accent).add_modifier(Modifier::BOLD)),
                );
            }
            f.render_widget(table, area);
        }
        WidgetKind::Tabs => {
            // `-tabs {a b}` -> a tab bar; `tab_sel` (set by a tab-bar click) marks
            // the selected label (a labelled selector — no per-tab content yet).
            let titles: Vec<Line> = w
                .opts
                .get("tabs")
                .map(|s| {
                    s.split(',')
                        .filter(|t| !t.is_empty())
                        .map(|t| Line::from(t.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let sel = tab_sel.min(titles.len().saturating_sub(1));
            let tabs = Tabs::new(titles)
                .block(block)
                .style(Style::default().fg(accent))
                .highlight_style(
                    Style::default()
                        .fg(accent)
                        .add_modifier(Modifier::BOLD | Modifier::REVERSED),
                )
                .select(sel);
            f.render_widget(tabs, area);
        }
        // Containers (`block`/`frame`) and any remaining kind render their bound
        // stream content inside the bordered box — never a placeholder string.
        _ => {
            let owned: Vec<String> = match &result {
                Some(QueryResult::Lines(ls)) => ls.clone(),
                Some(QueryResult::Scalar(v)) => vec![format!("{v}")],
                Some(QueryResult::Pairs(p)) => p.iter().map(|(k, v)| format!("{k}  {v}")).collect(),
                None => lines.to_vec(),
            };
            let inner_h = area.height.saturating_sub(2) as usize;
            let skip = scroll_skip(owned.len(), inner_h, scroll);
            let items: Vec<ListItem> = owned
                .iter()
                .skip(skip)
                .take(inner_h)
                .map(|l| ListItem::new(ansi_line(l)))
                .collect();
            f.render_widget(List::new(items).block(block), area);
            render_scrollbar(f, area, owned.len(), inner_h, skip, accent);
        }
    }
}

/// Draw a vertical scrollbar (ratatui `Scrollbar`) on the right border of a
/// scrollable list widget — only when the content overflows the viewport. `pos`
/// is the topmost visible row; the thumb tracks it. A no-op when everything fits.
fn render_scrollbar(f: &mut Frame, area: Rect, content: usize, visible: usize, pos: usize, accent: Color) {
    if content <= visible || area.height < 3 {
        return;
    }
    let mut state = ScrollbarState::new(content.saturating_sub(visible)).position(pos);
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .thumb_style(Style::default().fg(accent))
        .track_style(Style::default().fg(Color::DarkGray));
    f.render_stateful_widget(bar, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::compute_rects;
    use crate::parser::parse;
    use crate::spec::build;
    use ratatui::layout::Rect;

    #[test]
    fn grid_span_widget_covers_multiple_cells() {
        // .main spans both columns of the top row; .a/.b split the bottom row.
        let spec = build(
            &parse(
                "chart .main\ngauge .a\ngauge .b\n\
                 grid .main -row 0 -col 0 -span 2\ngrid .a -row 1 -col 0\ngrid .b -row 1 -col 1",
            )
            .unwrap(),
        )
        .unwrap();
        let rects = compute_rects(
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            &spec,
        );
        assert_eq!(rects.len(), 3);
        // .main: full width, top half.
        assert_eq!(
            (rects[0].x, rects[0].y, rects[0].width, rects[0].height),
            (0, 0, 100, 50)
        );
        // .a: bottom-left, .b: bottom-right.
        assert_eq!((rects[1].x, rects[1].y, rects[1].width), (0, 50, 50));
        assert_eq!((rects[2].x, rects[2].y, rects[2].width), (50, 50, 50));
    }

    #[test]
    fn exact_score_is_substring_smartcase() {
        use super::exact_score;
        // Substring present → Some; earlier position scores higher (less negative).
        assert!(exact_score("hello world", "world").is_some());
        assert!(exact_score("abc", "xyz").is_none());
        assert!(exact_score("axbxc", "abc").is_none()); // not contiguous → no exact match
                                                        // Smart case: lowercase query is case-insensitive; uppercase is exact.
        assert!(exact_score("Hello", "hello").is_some());
        assert!(exact_score("hello", "Hello").is_none());
        // Earlier match ranks above a later one.
        assert!(exact_score("xa", "a") < exact_score("a", "a"));
    }

    #[test]
    fn no_grid_auto_stacks_vertically() {
        let spec = build(&parse("gauge .a\ngauge .b").unwrap()).unwrap();
        let rects = compute_rects(
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 40,
            },
            &spec,
        );
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].height, 20);
        assert_eq!(rects[1].y, 20);
    }

    fn rects_of(src: &str, w: u16, h: u16) -> Vec<Rect> {
        let spec = build(&parse(src).unwrap()).unwrap();
        compute_rects(
            Rect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            },
            &spec,
        )
    }

    #[test]
    fn parse_tracks_lengths_percents_weights() {
        use crate::spec::{parse_tracks, Track};
        assert_eq!(
            parse_tracks("20 * 2* 30%").unwrap(),
            vec![Track::Length(20), Track::Fill(1), Track::Fill(2), Track::Percentage(30)]
        );
        assert!(parse_tracks("").is_err());
        assert!(parse_tracks("bogus").is_err());
    }

    #[test]
    fn cols_weighted_split_proportionally() {
        // `cols "1* 2*"` → two columns in a 1:2 ratio across width 90 = 30 / 60.
        let r = rects_of(
            "gauge .a\ngauge .b\ngrid .a -row 0 -col 0\ngrid .b -row 0 -col 1\ncols \"1* 2*\"",
            90,
            20,
        );
        assert_eq!(r[0].width, 30);
        assert_eq!(r[1].width, 60);
        assert_eq!(r[1].x, 30);
    }

    #[test]
    fn cols_fixed_length_then_fill() {
        // `cols "20 *"` → first column a fixed 20 cells, second fills the rest.
        let r = rects_of(
            "gauge .a\ngauge .b\ngrid .a -row 0 -col 0\ngrid .b -row 0 -col 1\ncols \"20 *\"",
            80,
            20,
        );
        assert_eq!(r[0].width, 20);
        assert_eq!(r[1].width, 60);
    }

    #[test]
    fn gap_inserts_spacing_between_cells() {
        // `gap 2` puts 2 blank cells between the two columns: (92-2)/2 = 45 each.
        let r = rects_of(
            "gauge .a\ngauge .b\ngrid .a -row 0 -col 0\ngrid .b -row 0 -col 1\ngap 2",
            92,
            20,
        );
        assert_eq!(r[0].width, 45);
        assert_eq!(r[1].x, 47); // 45 + 2 gap
    }

    #[test]
    fn layout_horizontal_tiles_side_by_side() {
        // `layout horizontal` lays the auto (no-grid) widgets in a row.
        let r = rects_of("gauge .a\ngauge .b\nlayout horizontal", 80, 20);
        assert_eq!(r[0].width, 40);
        assert_eq!(r[1].x, 40);
        assert_eq!(r[0].height, 20); // full height, not stacked
    }

    // Renders one widget into a TestBackend and returns its cell text.
    fn render_text(spec_src: &str) -> String {
        use super::render_widget;
        use crate::stream::StreamState;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let spec = build(&parse(spec_src).unwrap()).unwrap();
        let w = &spec.widgets[0];
        let st = StreamState::new();
        let data = vec!["one".to_string(), "two".to_string()];
        let mut term = Terminal::new(TestBackend::new(40, 6)).unwrap();
        term.draw(|f| render_widget(f, f.area(), w, &st, &data, None, None, 0, 0, None))
            .unwrap();
        term.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn tick_timeouts_fires_once_per_idle_span() {
        use super::{tick_timeouts, Controls};
        use crate::spec::{BindAction, Timeout};
        use std::time::{Duration, Instant};
        let mut c = Controls {
            inputs: vec![("x".into(), String::new())],
            ..Default::default()
        };
        let timeouts = vec![Timeout {
            dur: Duration::from_millis(10),
            action: BindAction::SetInput {
                name: "x".into(),
                value: "hot".into(),
            },
        }];
        let base = Instant::now();
        let (mut last_total, mut last_activity, mut fired) = (0u64, base, vec![false]);
        // Idle 20ms past the 10ms threshold -> fires.
        tick_timeouts(
            &timeouts,
            0,
            &mut last_total,
            &mut last_activity,
            &mut fired,
            base + Duration::from_millis(20),
            &mut c,
        );
        assert_eq!(c.inputs[0].1, "hot");
        assert!(fired[0]);
        // Same idle span -> latched, does not re-fire (clear the value, confirm it stays clear).
        c.inputs[0].1.clear();
        tick_timeouts(
            &timeouts,
            0,
            &mut last_total,
            &mut last_activity,
            &mut fired,
            base + Duration::from_millis(40),
            &mut c,
        );
        assert_eq!(c.inputs[0].1, "");
        // Stream advances (new line) -> re-arms and resets the idle clock; an
        // immediate tick does not fire.
        tick_timeouts(
            &timeouts,
            1,
            &mut last_total,
            &mut last_activity,
            &mut fired,
            base + Duration::from_millis(41),
            &mut c,
        );
        assert!(!fired[0]);
        assert_eq!(c.inputs[0].1, "");
    }

    #[test]
    fn actor_tell_then_ask_updates_control() {
        // End-to-end runtime path: a `tell` mutates a session actor's state, an
        // `ask` reads it back and writes the reply into a control input — exactly
        // what a `bind C-t tell w add(5)` / `bind C-a ask .out w add(10)` fires.
        use super::{apply_bind_action, Controls};
        use crate::spec::BindAction;
        let cmds = crate::parser::parse(
            "actor acc(state) { on add(n) { state = state + n; reply state } }",
        )
        .unwrap();
        let mut defs = std::collections::BTreeMap::new();
        let d = std::sync::Arc::new(crate::actor::parse_actor(&cmds[0].args).unwrap());
        defs.insert("acc".to_string(), d);
        let decls = vec![crate::actor::RefDecl {
            name: "w".into(),
            actor: "acc".into(),
            init: 100.0,
            pool: None,
            restart: true,
        }];
        let mut c = Controls {
            inputs: vec![("out".into(), String::new())],
            session: crate::actor::Session::build(&defs, &decls).unwrap(),
            ..Default::default()
        };
        apply_bind_action(
            &mut c,
            &BindAction::ActorTell {
                refname: "w".into(),
                call: "add(5)".into(),
            },
        ); // state 100 -> 105
        apply_bind_action(
            &mut c,
            &BindAction::ActorAsk {
                ctrl: "out".into(),
                refname: "w".into(),
                call: "add(10)".into(),
            },
        ); // state 105 -> 115, written to `.out`
        assert_eq!(c.inputs[0].1, "115");
    }

    #[test]
    fn parse_sgr_mouse_decodes_reports() {
        use super::{parse_sgr_mouse, MouseKind};
        // Left click at 1-based (5,3) -> 0-based (4,2), 'M' press, 9 bytes.
        let (ev, n) = parse_sgr_mouse(b"\x1b[<0;5;3M", 0).unwrap();
        assert_eq!(
            (ev.kind, ev.col, ev.row, ev.press),
            (MouseKind::Down, 4, 2, true)
        );
        assert_eq!(n, 9);
        // Release ('m').
        let (ev, _) = parse_sgr_mouse(b"\x1b[<0;5;3m", 0).unwrap();
        assert_eq!((ev.kind, ev.press), (MouseKind::Up, false));
        // Scroll up (button 64) / down (65).
        assert_eq!(
            parse_sgr_mouse(b"\x1b[<64;10;20M", 0).unwrap().0.kind,
            MouseKind::ScrollUp
        );
        assert_eq!(
            parse_sgr_mouse(b"\x1b[<65;1;1M", 0).unwrap().0.kind,
            MouseKind::ScrollDown
        );
        // Drag (bit 32 set, press).
        assert_eq!(
            parse_sgr_mouse(b"\x1b[<32;7;8M", 0).unwrap().0.kind,
            MouseKind::Drag
        );
        // Mid-buffer offset.
        let (ev, n) = parse_sgr_mouse(b"xy\x1b[<0;3;4M", 2).unwrap();
        assert_eq!((ev.col, ev.row), (2, 3));
        assert_eq!(n, 9);
        // Truncated / non-mouse -> None.
        assert!(parse_sgr_mouse(b"\x1b[<0;5;", 0).is_none());
        assert!(parse_sgr_mouse(b"\x1b[A", 0).is_none());
    }

    #[test]
    fn mouse_hit_and_geometry_helpers() {
        use super::{detect_resize, facet_row_to_index, hit, slider_value_from_x, HitTarget};
        use crate::spec::WidgetKind;
        use ratatui::layout::Rect;
        let hm = vec![
            HitTarget {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 10,
                    height: 5,
                },
                kind: WidgetKind::Filter,
                control_name: "q".into(),
                meta_index: Some(0),
                tabs: Vec::new(),
            },
            HitTarget {
                rect: Rect {
                    x: 0,
                    y: 2,
                    width: 10,
                    height: 3,
                },
                kind: WidgetKind::Check,
                control_name: "c".into(),
                meta_index: Some(1),
                tabs: Vec::new(),
            },
        ];
        // Overlap at (3,3): the later (topmost) target wins.
        assert_eq!(hit(&hm, 3, 3).unwrap().control_name, "c");
        assert_eq!(hit(&hm, 3, 1).unwrap().control_name, "q"); // only the first covers y=1
        assert!(hit(&hm, 20, 20).is_none()); // outside
                                             // Facet row -> option index (skip the top border).
        assert_eq!(facet_row_to_index(0, 1), Some(0));
        assert_eq!(facet_row_to_index(0, 3), Some(2));
        assert_eq!(facet_row_to_index(5, 5), None);
        // Slider value from click x (inner width = w-2), clamped + snapped.
        assert_eq!(slider_value_from_x(0, 12, 6, 0.0, 10.0, 1.0), "5"); // mid
        assert_eq!(slider_value_from_x(0, 12, 0, 0.0, 10.0, 1.0), "0"); // far left
        assert_eq!(slider_value_from_x(0, 12, 99, 0.0, 10.0, 1.0), "10"); // clamp right
                                                                          // Resize detector.
        let mut last = (80, 24);
        assert!(!detect_resize(&mut last, (80, 24)));
        assert!(detect_resize(&mut last, (100, 30)));
        assert_eq!(last, (100, 30));
    }

    #[test]
    fn dispatch_mouse_clicks_and_scrolls() {
        use super::{
            dispatch_mouse, ControlKind, ControlMeta, Controls, HitTarget, MouseEvent, MouseKind,
        };
        use crate::spec::WidgetKind;
        use ratatui::layout::Rect;
        let mut c = Controls {
            inputs: vec![("chk".into(), "0".into()), ("sl".into(), "0".into())],
            control_meta: vec![
                ControlMeta {
                    kind: ControlKind::Check,
                    ..Default::default()
                },
                ControlMeta {
                    kind: ControlKind::Slider,
                    min: 0.0,
                    max: 10.0,
                    step: 1.0,
                    ..Default::default()
                },
            ],
            hitmap: vec![
                HitTarget {
                    rect: Rect {
                        x: 0,
                        y: 0,
                        width: 10,
                        height: 3,
                    },
                    kind: WidgetKind::Check,
                    control_name: "chk".into(),
                    meta_index: Some(0),
                    tabs: Vec::new(),
                },
                HitTarget {
                    rect: Rect {
                        x: 0,
                        y: 3,
                        width: 12,
                        height: 3,
                    },
                    kind: WidgetKind::Slider,
                    control_name: "sl".into(),
                    meta_index: Some(1),
                    tabs: Vec::new(),
                },
            ],
            ..Default::default()
        };
        let ev = |kind, col, row| MouseEvent {
            kind,
            col,
            row,
            button: 0,
            press: kind == MouseKind::Down,
        };
        // Click the checkbox -> toggles + focuses it.
        dispatch_mouse(
            &mut c,
            ev(MouseKind::Down, 2, 1),
            false,
            std::time::Instant::now(),
        );
        assert_eq!(c.inputs[0].1, "1");
        assert_eq!(c.focus, 0);
        // Click mid the slider (inner width 10, x=6 -> ~mid).
        dispatch_mouse(
            &mut c,
            ev(MouseKind::Down, 6, 4),
            false,
            std::time::Instant::now(),
        );
        assert_eq!(c.inputs[1].1, "5");
        assert_eq!(c.focus, 1);
        // Scroll in fzf mode moves the cursor.
        c.cursor = 5;
        dispatch_mouse(
            &mut c,
            ev(MouseKind::ScrollUp, 0, 0),
            true,
            std::time::Instant::now(),
        );
        assert_eq!(c.cursor, 4);
    }

    #[test]
    fn fzf_row_and_tab_geometry() {
        use super::{fzf_row_to_cursor, tab_index_from_x};
        // fzf row -> cursor = scroll offset + rows below the list top.
        assert_eq!(fzf_row_to_cursor(2, 10, 2), 10);
        assert_eq!(fzf_row_to_cursor(2, 10, 4), 12);
        assert_eq!(fzf_row_to_cursor(0, 0, 5), 5);
        assert_eq!(fzf_row_to_cursor(2, 10, 1), 10); // saturates above the body
                                                     // Tab bar: ` a | bb | ccc ` inside the left border at rect_x.
        let labels = ["a", "bb", "ccc"];
        assert_eq!(tab_index_from_x(&labels, 0, 0), None); // on the border
        assert_eq!(tab_index_from_x(&labels, 0, 1), Some(0)); // ` a `
        assert_eq!(tab_index_from_x(&labels, 0, 5), Some(1)); // ` bb `
        assert_eq!(tab_index_from_x(&labels, 0, 10), Some(2)); // ` ccc `
        assert_eq!(tab_index_from_x(&labels, 0, 99), None); // past the last tab
    }

    #[test]
    fn dispatch_mouse_fzf_row_and_tab_click() {
        use super::{dispatch_mouse, Controls, HitTarget, MouseEvent, MouseKind};
        use crate::spec::WidgetKind;
        use ratatui::layout::Rect;
        // fzf: clicking a list row sets the cursor via the scroll offset.
        let mut c = Controls {
            fzf_list_start: 10,
            hitmap: vec![HitTarget {
                rect: Rect {
                    x: 0,
                    y: 2,
                    width: 20,
                    height: 10,
                },
                kind: WidgetKind::Select,
                control_name: String::new(),
                meta_index: None,
                tabs: Vec::new(),
            }],
            ..Default::default()
        };
        dispatch_mouse(
            &mut c,
            MouseEvent {
                kind: MouseKind::Down,
                col: 3,
                row: 4,
                button: 0,
                press: true,
            },
            true,
            std::time::Instant::now(),
        );
        assert_eq!(c.cursor, 12); // 10 + (4 - 2)
                                  // tabs: clicking a label selects it.
        let mut c = Controls {
            hitmap: vec![HitTarget {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 20,
                    height: 3,
                },
                kind: WidgetKind::Tabs,
                control_name: "t".into(),
                meta_index: None,
                tabs: vec!["a".into(), "bb".into(), "ccc".into()],
            }],
            ..Default::default()
        };
        dispatch_mouse(
            &mut c,
            MouseEvent {
                kind: MouseKind::Down,
                col: 5,
                row: 0,
                button: 0,
                press: true,
            },
            false,
            std::time::Instant::now(),
        );
        assert_eq!(c.tab_sel.get("t"), Some(&1));
    }

    #[test]
    fn control_helpers_slider_check_facet() {
        use super::{slider_adjust, toggle_check, toggle_set_member};
        // Slider: clamps to [min, max], steps by `step`.
        assert_eq!(slider_adjust("5", 0.0, 10.0, 2.0, true), "7");
        assert_eq!(slider_adjust("9", 0.0, 10.0, 2.0, true), "10"); // clamp high
        assert_eq!(slider_adjust("1", 0.0, 10.0, 2.0, false), "0"); // clamp low
                                                                    // Check: boolean flip.
        assert_eq!(toggle_check("0"), "1");
        assert_eq!(toggle_check("1"), "0");
        // Facet set: add then remove, order preserved.
        assert_eq!(toggle_set_member("", "warn"), "warn");
        assert_eq!(toggle_set_member("warn", "error"), "warn,error");
        assert_eq!(toggle_set_member("warn,error", "warn"), "error");
    }

    #[test]
    fn mouse_button_and_modifiers() {
        use super::{mouse_alt, mouse_button, mouse_ctrl, mouse_shift, MouseButton};
        // Low two bits pick the button; wheel codes (64/65) keep bits 0/1 clear.
        assert_eq!(mouse_button(0), MouseButton::Left);
        assert_eq!(mouse_button(1), MouseButton::Middle);
        assert_eq!(mouse_button(2), MouseButton::Right);
        // Modifiers ride the high bits and don't disturb the button decode.
        assert_eq!(mouse_button(0x04 | 0x10), MouseButton::Left);
        assert_eq!(mouse_button(2 | 0x08), MouseButton::Right);
        assert!(mouse_shift(0x04) && !mouse_alt(0x04) && !mouse_ctrl(0x04));
        assert!(mouse_alt(0x08) && !mouse_shift(0x08));
        assert!(mouse_ctrl(0x10) && !mouse_alt(0x10));
        assert!(!mouse_shift(0) && !mouse_alt(0) && !mouse_ctrl(0));
    }

    #[test]
    fn scroll_skip_windows_the_tail() {
        use super::scroll_skip;
        // No scrollback: skip everything above the last `cap` rows.
        assert_eq!(scroll_skip(100, 10, 0), 90);
        // Scroll back N: window ends N rows above the live tail.
        assert_eq!(scroll_skip(100, 10, 5), 85);
        // Clamp: can't skip past the top of the buffer.
        assert_eq!(scroll_skip(100, 10, 200), 0);
        // Buffer fits the pane: nothing to skip regardless of scroll.
        assert_eq!(scroll_skip(5, 10, 3), 0);
    }

    #[test]
    fn double_click_window() {
        use super::{is_double_click, DOUBLE_CLICK};
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        // No prior click -> never a double-click.
        assert!(!is_double_click(None, t0, 4));
        // Same row, inside the window -> double-click.
        let last = Some((t0, 3u16, 4u16));
        assert!(is_double_click(last, t0 + Duration::from_millis(100), 4));
        // Same row but past the window -> single click.
        assert!(!is_double_click(
            last,
            t0 + DOUBLE_CLICK + Duration::from_millis(1),
            4
        ));
        // Different row inside the window -> not a double-click.
        assert!(!is_double_click(last, t0 + Duration::from_millis(100), 5));
    }

    #[test]
    fn dispatch_mouse_right_click_resets_and_double_click_submits() {
        use super::{
            dispatch_mouse, ControlKind, ControlMeta, Controls, HitTarget, MouseEvent, MouseKind,
        };
        use crate::spec::WidgetKind;
        use ratatui::layout::Rect;
        use std::time::{Duration, Instant};
        let mk = |button, row| MouseEvent {
            kind: MouseKind::Down,
            col: 2,
            row,
            button,
            press: true,
        };
        let mut c = Controls {
            inputs: vec![
                ("chk".into(), "1".into()),
                ("sl".into(), "7".into()),
                ("txt".into(), "hi".into()),
            ],
            control_meta: vec![
                ControlMeta {
                    kind: ControlKind::Check,
                    ..Default::default()
                },
                ControlMeta {
                    kind: ControlKind::Slider,
                    min: 2.0,
                    max: 10.0,
                    step: 1.0,
                    ..Default::default()
                },
                ControlMeta {
                    kind: ControlKind::Text,
                    ..Default::default()
                },
            ],
            hitmap: vec![
                HitTarget {
                    rect: Rect {
                        x: 0,
                        y: 0,
                        width: 10,
                        height: 3,
                    },
                    kind: WidgetKind::Check,
                    control_name: "chk".into(),
                    meta_index: Some(0),
                    tabs: Vec::new(),
                },
                HitTarget {
                    rect: Rect {
                        x: 0,
                        y: 3,
                        width: 12,
                        height: 3,
                    },
                    kind: WidgetKind::Slider,
                    control_name: "sl".into(),
                    meta_index: Some(1),
                    tabs: Vec::new(),
                },
                HitTarget {
                    rect: Rect {
                        x: 0,
                        y: 6,
                        width: 12,
                        height: 3,
                    },
                    kind: WidgetKind::Filter,
                    control_name: "txt".into(),
                    meta_index: Some(2),
                    tabs: Vec::new(),
                },
            ],
            ..Default::default()
        };
        let now = Instant::now();
        // Right-click (button 2) resets each control to its empty/default.
        dispatch_mouse(&mut c, mk(2, 1), false, now); // Check -> "0"
        assert_eq!(c.inputs[0].1, "0");
        dispatch_mouse(&mut c, mk(2, 4), false, now); // Slider -> min (2)
        assert_eq!(c.inputs[1].1, "2");
        dispatch_mouse(&mut c, mk(2, 7), false, now); // Text -> ""
        assert_eq!(c.inputs[2].1, "");

        // Double-click a Select row within the window sets submit.
        let mut c = Controls {
            fzf_list_start: 0,
            hitmap: vec![HitTarget {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 20,
                    height: 10,
                },
                kind: WidgetKind::Select,
                control_name: String::new(),
                meta_index: None,
                tabs: Vec::new(),
            }],
            ..Default::default()
        };
        dispatch_mouse(
            &mut c,
            MouseEvent {
                kind: MouseKind::Down,
                col: 3,
                row: 4,
                button: 0,
                press: true,
            },
            false,
            now,
        );
        assert!(!c.submit); // first click only positions the cursor
        dispatch_mouse(
            &mut c,
            MouseEvent {
                kind: MouseKind::Down,
                col: 3,
                row: 4,
                button: 0,
                press: true,
            },
            false,
            now + Duration::from_millis(100),
        );
        assert!(c.submit); // second click on the same row within the window submits
    }

    #[test]
    fn dispatch_mouse_wheel_scrolls_widget() {
        use super::{dispatch_mouse, Controls, HitTarget, MouseEvent, MouseKind};
        use crate::spec::WidgetKind;
        use ratatui::layout::Rect;
        use std::time::Instant;
        let mut c = Controls {
            hitmap: vec![HitTarget {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 20,
                    height: 10,
                },
                kind: WidgetKind::Tail,
                control_name: "log".into(),
                meta_index: None,
                tabs: Vec::new(),
            }],
            ..Default::default()
        };
        let ev = |kind| MouseEvent {
            kind,
            col: 5,
            row: 5,
            button: 0,
            press: false,
        };
        let now = Instant::now();
        // Wheel up over a scrollable widget banks older rows.
        dispatch_mouse(&mut c, ev(MouseKind::ScrollUp), false, now);
        dispatch_mouse(&mut c, ev(MouseKind::ScrollUp), false, now);
        assert_eq!(c.scroll.get("log"), Some(&2));
        // Wheel down walks back toward the live tail, saturating at 0.
        dispatch_mouse(&mut c, ev(MouseKind::ScrollDown), false, now);
        assert_eq!(c.scroll.get("log"), Some(&1));
        dispatch_mouse(&mut c, ev(MouseKind::ScrollDown), false, now);
        dispatch_mouse(&mut c, ev(MouseKind::ScrollDown), false, now);
        assert_eq!(c.scroll.get("log"), Some(&0));
    }

    #[test]
    fn facet_candidates_from_opts_and_field() {
        use super::facet_candidates;
        // Explicit -opts.
        let w = &build(&parse("facet .lv -opts {info warn error}").unwrap())
            .unwrap()
            .widgets[0];
        assert_eq!(facet_candidates(w, &[]), vec!["info", "warn", "error"]);
        // Distinct -field values from the stream.
        let w2 = &build(&parse("facet .lv -field level").unwrap())
            .unwrap()
            .widgets[0];
        let raw = vec![
            r#"{"level":"info"}"#.to_string(),
            r#"{"level":"error"}"#.to_string(),
            r#"{"level":"info"}"#.to_string(),
        ];
        assert_eq!(facet_candidates(w2, &raw), vec!["info", "error"]);
    }

    #[test]
    fn tick_timeouts_quit_action() {
        use super::{tick_timeouts, Controls};
        use crate::spec::{BindAction, Timeout};
        use std::time::{Duration, Instant};
        let mut c = Controls::default();
        let timeouts = vec![Timeout {
            dur: Duration::from_millis(5),
            action: BindAction::Quit,
        }];
        let base = Instant::now();
        let (mut lt, mut la, mut fired) = (0u64, base, vec![false]);
        tick_timeouts(
            &timeouts,
            0,
            &mut lt,
            &mut la,
            &mut fired,
            base + Duration::from_millis(10),
            &mut c,
        );
        assert!(c.quit);
    }

    #[test]
    fn tabs_block_frame_render_without_placeholder() {
        // The old fallback printed "<kind> — not yet rendered"; these kinds now
        // render real widgets, so that string must never appear.
        for src in [
            "tabs .t -tabs {alpha beta}",
            "block .b -title Box",
            "frame .f",
        ] {
            let text = render_text(src);
            assert!(
                !text.contains("not yet rendered"),
                "placeholder leaked for `{src}`: {text}"
            );
        }
        // Tab labels captured from the `{alpha beta}` block reach the tab bar.
        let tabs = render_text("tabs .t -tabs {alpha beta}");
        assert!(
            tabs.contains("alpha") && tabs.contains("beta"),
            "tab labels missing: {tabs}"
        );
        // A container shows its bound stream content, not an apology string.
        assert!(render_text("block .b").contains("two"));
    }

    #[test]
    fn theme_directive_sets_palette_and_resolve_accent() {
        use super::resolve_accent;
        use ratatui::style::Color;
        // `theme neon-noir` sets the palette; accent (c2) = index 231.
        let sp = build(&parse("theme neon-noir\ntext .t <- in").unwrap()).unwrap();
        let th = sp.theme;
        assert_eq!(th.map(|p| p.accent()), Some(Color::Indexed(231)));
        // No -color, theme active -> theme accent.
        assert_eq!(resolve_accent(None, th), Color::Indexed(231));
        // A palette slot resolves through the theme.
        assert_eq!(resolve_accent(Some("dim"), th), Color::Indexed(57)); // c5
        // A fixed semantic name is theme-independent (green hex, not a slot).
        assert_eq!(resolve_accent(Some("green"), th), super::hex_color("#00e676"));
        // No theme, no color -> classic cyan default (backward compatible).
        assert_eq!(resolve_accent(None, None), super::hex_color(crate::spec::color_hex(None)));
    }

    #[test]
    fn key_label_formats_control_bytes() {
        use super::key_label;
        assert_eq!(key_label(0x14), "Ctrl-T");
        assert_eq!(key_label(0x07), "Ctrl-G");
        assert_eq!(key_label(0x15), "Ctrl-U");
        assert_eq!(key_label(0x1b), "Esc");
    }

    #[test]
    fn default_slot_varies_by_widget_kind() {
        use super::default_slot_for_kind;
        use crate::spec::WidgetKind;
        // neon-noir = [201(primary), 231(accent), 93(alt), 219(mid), 57, 53].
        let p = crate::theme::by_name("neon-noir").unwrap();
        assert_eq!(default_slot_for_kind(WidgetKind::Gauge, p), p.accent());
        assert_eq!(default_slot_for_kind(WidgetKind::Bars, p), p.alt());
        assert_eq!(default_slot_for_kind(WidgetKind::Chart, p), p.mid());
        assert_eq!(default_slot_for_kind(WidgetKind::Tail, p), p.primary());
        // Distinct slots -> a multi-color dashboard, not monochrome.
        assert_ne!(
            default_slot_for_kind(WidgetKind::Gauge, p),
            default_slot_for_kind(WidgetKind::Bars, p)
        );
    }

    #[test]
    fn theme_custom_and_unknown() {
        // `theme custom c1..c6` builds a palette from six indices.
        let sp = build(&parse("theme custom 1 2 3 4 5 6\ntext .t <- in").unwrap()).unwrap();
        assert_eq!(sp.theme.map(|p| p.accent()), Some(ratatui::style::Color::Indexed(2)));
        // An unknown theme name is a build error.
        assert!(build(&parse("theme bogus\ntext .t <- in").unwrap()).is_err());
        // `theme custom` with the wrong count of indices errors.
        assert!(build(&parse("theme custom 1 2 3\ntext .t <- in").unwrap()).is_err());
    }

    #[test]
    fn linegauge_and_scatter_render_without_panic() {
        // Both new display widgets render into a real backend; their titles carry
        // the kind label, and neither panics on empty/no-source data.
        let lg = render_text("linegauge .load -max 8");
        assert!(lg.contains("linegauge"), "linegauge title missing: {lg}");
        let sc = render_text("scatter .lat");
        assert!(sc.contains("scatter"), "scatter title missing: {sc}");
    }

    #[test]
    fn sparkline_map_calendar_render_without_panic() {
        // Each ratatui-widget wrapper renders without panicking on plain data.
        // (Canvas/Monthly draw into the buffer; the block title carries the kind.)
        assert!(render_text("sparkline .s").contains("sparkline"));
        // Map + calendar draw shapes that don't leave the kind label in the top
        // border cells, so just assert the render completes (non-empty buffer).
        assert!(!render_text("map .m").is_empty());
        assert!(!render_text("calendar .c").is_empty());
    }

    #[test]
    fn stream_event_dates_parses_iso_dates() {
        use super::stream_event_dates;
        let lines = vec![
            "2026-07-19 login ok".to_string(),
            "no date here".to_string(),
            "[2026-07-20] event".to_string(),
            "2026-13-40 bad".to_string(), // invalid month/day -> skipped
            "2026-07-19 dup".to_string(), // duplicate -> deduped
        ];
        let dates = stream_event_dates(&lines);
        assert_eq!(dates.len(), 2);
        assert!(dates.contains(&time::Date::from_calendar_date(2026, time::Month::July, 19).unwrap()));
        assert!(dates.contains(&time::Date::from_calendar_date(2026, time::Month::July, 20).unwrap()));
    }

    #[test]
    fn sel_publishes_highlighted_row_as_control() {
        use super::{sel_candidates, update_sel_controls, ControlKind, ControlMeta, Controls};
        // A `sel` widget over `field 1` yields the first column of each row; the
        // cursor row is published into the `.ps.sel` control.
        let spec = build(&parse("sel .ps\nsource .ps { in; fields 1 }").unwrap()).unwrap();
        let raw = vec!["alice 30".to_string(), "bob 25".to_string(), "carol 40".to_string()];
        assert_eq!(sel_candidates(&spec.widgets[0], &raw), vec!["alice", "bob", "carol"]);

        let mut c = Controls {
            inputs: vec![("ps.sel".into(), String::new())],
            control_meta: vec![ControlMeta {
                kind: ControlKind::Sel,
                cursor: 1,
                ..Default::default()
            }],
            ..Default::default()
        };
        update_sel_controls(&spec, &raw, &mut c);
        assert_eq!(c.inputs[0].1, "bob"); // cursor row 1

        // Cursor past the end clamps to the last row, not out-of-bounds.
        c.control_meta[0].cursor = 99;
        update_sel_controls(&spec, &raw, &mut c);
        assert_eq!(c.inputs[0].1, "carol");
        assert_eq!(c.control_meta[0].cursor, 2);
    }
}
