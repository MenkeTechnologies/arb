//! ratatui render loop. Widgets are auto-tiled vertically (explicit `pack`/`grid`
//! geometry arrives later). Each widget's `source` pipeline is evaluated against
//! the shared stream every frame; `text`/`tail`/`list` render the resulting
//! lines and `gauge` renders a scalar against `-max`. Widget kinds without a
//! renderer yet show an honest placeholder rather than faking output.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{
    cursor::MoveTo,
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, size, Clear as TermClear, ClearType,
        EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::calendar::{CalendarEventStore, Monthly};
use ratatui::widgets::canvas::{Canvas, Map, MapResolution, Points};
use ratatui::widgets::{
    Axis, BarChart, Block, Borders, Cell, Chart, Clear, Dataset, Gauge, GraphType, LineGauge, List,
    ListItem, Paragraph, RatatuiLogo, Row, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Sparkline, Table, Tabs,
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

/// Parse a `--height` spec into inline viewport rows plus whether it was a
/// PERCENTAGE — `--min-height` applies to the percentage form only, as in fzf.
/// `None` (doesn't parse) → full-screen.
fn parse_height(spec: &str) -> Option<(u16, bool)> {
    let s = spec.trim();
    if let Some(p) = s.strip_suffix('%') {
        let pct: u32 = p.trim().parse().ok()?;
        // `--height 100%` is fzf's way of saying FULL SCREEN, and fzf runs that
        // on the alternate screen — which is why quitting restores whatever was
        // on the terminal before. An inline viewport of the same size would wipe
        // it instead. (An absolute height equal to the row count stays inline,
        // as it does in fzf.)
        if pct >= 100 {
            return None;
        }
        let rows = size().map(|(_, h)| h).unwrap_or(24) as u32;
        Some(((rows * pct / 100).clamp(3, rows) as u16, true))
    } else {
        s.parse::<u16>().ok().map(|n| (n.max(3), false))
    }
}

/// fzf's `--min-height` floor for a percentage `--height`. The default `10+`
/// means "10 rows of LIST", so the chrome around it (prompt, info rule, header,
/// border) is added on top; a bare number is the total instead. Never taller
/// than the terminal.
pub fn min_height_rows(min: u16, plus: bool, chrome: u16, total: u16) -> u16 {
    let want = if plus {
        min.saturating_add(chrome)
    } else {
        min
    };
    want.min(total.max(1))
}

/// Parse a cursor-position report (`ESC [ row ; col R`) into a 0-based row.
/// The LAST report in the buffer wins — a keystroke typed before the terminal
/// answered can sit in front of it.
pub fn parse_cursor_report(buf: &[u8]) -> Option<u16> {
    let start = buf.windows(2).rposition(|w| w == b"\x1b[").map(|i| i + 2)?;
    let end = start + buf[start..].iter().position(|b| *b == b'R')?;
    let body = std::str::from_utf8(&buf[start..end]).ok()?;
    let row: u16 = body.split(';').next()?.parse().ok()?;
    Some(row.saturating_sub(1))
}

/// Ask the terminal where the cursor is, over the `/dev/tty` handle arb already
/// owns. crossterm's `position()` can't be used for this: it writes the query to
/// STDOUT — which is arb's DATA channel, and a pipe whenever a consumer is
/// attached, so the request never reaches the terminal — and then loops forever
/// when no reply arrives. That is what made `--height` (and therefore any
/// `$FZF_DEFAULT_OPTS` containing it) hang before the first frame.
fn tty_cursor_row(tty: &mut File) -> Option<u16> {
    let fd = tty.as_raw_fd();
    // Non-blocking for the duration of the query: a terminal that answers
    // nothing (or answers something else) must cost one blink, never a blocked
    // read — `poll` alone isn't enough, since it can report readable for a
    // condition that yields no bytes.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return None;
    }
    let row = read_cursor_report(tty, fd, Duration::from_millis(300));
    unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
    row
}

/// The query/read half of [`tty_cursor_row`], with the fd already non-blocking.
fn read_cursor_report(tty: &mut File, fd: i32, budget: Duration) -> Option<u16> {
    tty.write_all(b"\x1b[6n").ok()?;
    tty.flush().ok()?;
    let deadline = Instant::now() + budget;
    let mut buf = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, (left.as_millis() as i32).max(1)) } <= 0 {
            break;
        }
        let mut chunk = [0u8; 32];
        match tty.read(&mut chunk) {
            Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
            // Readable but empty (EAGAIN / EOF): keep waiting out the budget.
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
        if let Some(row) = parse_cursor_report(&buf) {
            return Some(row);
        }
    }
    None
}

/// The inline (`--height`) viewport, given the row the cursor sits on AFTER the
/// room has been reserved — that row is the picker's last line, so the viewport
/// is the `rows` lines ending there. Asking the terminal where it ended up beats
/// predicting whether it scrolled, which is also why fzf re-queries.
pub fn inline_rect(bottom_row: u16, rows: u16, cols: u16, total: u16) -> Rect {
    let h = rows.clamp(1, total.max(1));
    let bottom = bottom_row.min(total.saturating_sub(1));
    Rect {
        x: 0,
        y: bottom.saturating_sub(h - 1),
        width: cols,
        height: h,
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
    /// Tab pressed — the run loop toggles a line in `marks`, since only it knows
    /// the current filtered list.
    /// Rows with a pending `toggle`, in press order. The INDEX is recorded when
    /// the key is pressed for two reasons: fzf's `toggle+down` marks the row you
    /// were on and only then moves, and several Tabs can land inside one render
    /// tick — a plain flag would collapse them into a single mark.
    pub toggles: Vec<usize>,
    /// Whether marking is allowed at all. fzf ignores `toggle` without
    /// `-m`/`--multi`, so `arb --fzf` does too; arb's own `select` widget keeps
    /// marking on unconditionally.
    pub multi: bool,
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
    /// The resolved fzf presentation (`$FZF_DEFAULT_OPTS` + the command line):
    /// layout, border, info style, pointer/marker glyphs, palette and the
    /// `--bind` table. The key handler consults the bindings before its own
    /// defaults, so `tab:toggle+down` behaves the way fzf would.
    pub look: crate::fzf::Look,
    /// Rows of list currently on screen, published by the renderer so a
    /// `page-up`/`page-down` binding moves by a real screenful.
    pub fzf_page: usize,
    /// Matches on screen this frame, so `--cycle` can wrap and `last` can land
    /// on the final row.
    pub fzf_count: usize,
    /// A `toggle-all` binding fired; the run loop marks (or clears) every match,
    /// since only it holds the filtered list.
    pub toggle_all: bool,
    /// Which `--expect` key accepted the selection, if one did. `main` prints it
    /// ahead of the result, as fzf does.
    pub expect_key: Option<String>,
}

/// The megafilter predicate: a line is kept iff it matches the interactive
/// filter (case-insensitive substring); an empty filter keeps everything. The
/// SAME test narrows the on-screen dashboard and the passthrough to a downstream
/// consumer, so what you type reshapes both live.
pub fn filter_matches(line: &str, filter: &str) -> bool {
    filter.is_empty() || line.to_lowercase().contains(&filter.to_lowercase())
}

/// Fuzzy match, scored by the port of fzf's own `FuzzyMatchV2`
/// ([`crate::algo`]) — same ranking as `fzf` for the same query, smart-case
/// included. An empty pattern matches everything with score 0.
pub fn fuzzy_score(line: &str, pat: &str) -> Option<i32> {
    let (p, cased) = crate::algo::prepare_pattern(pat);
    crate::algo::fuzzy_match_v2(cased, &crate::algo::Text::new(line), &p, false)
        .map(|(m, _)| m.score)
}

/// fzf `--exact`/`-e`: the best substring occurrence, scored on the same scale
/// as a fuzzy match (fzf's `ExactMatchNaive`).
pub fn exact_score(line: &str, pat: &str) -> Option<i32> {
    let (p, cased) = crate::algo::prepare_pattern(pat);
    crate::algo::exact_match_naive(cased, &crate::algo::Text::new(line), &p, false)
        .map(|(m, _)| m.score)
}

/// The length fzf's `--tiebreak=length` compares: character count with the
/// surrounding whitespace trimmed (fzf's `Chars.TrimLength`). Trailing
/// whitespace is real in file lists — macOS `Icon\r` entries tie with their
/// neighbours and land in a different order if it is counted.
pub fn trim_length(s: &str) -> usize {
    s.trim().chars().count()
}

/// What a line IS once fzf has read it: the text with the SGR codes removed
/// under `--ansi` (they become colour, not content), the line itself otherwise.
/// This is what a selection emits and what `--filter` prints.
pub fn item_text<'a>(line: &'a str, look: &crate::fzf::Look) -> std::borrow::Cow<'a, str> {
    match look.ansi {
        true => std::borrow::Cow::Owned(crate::fzf::strip_ansi(line)),
        false => std::borrow::Cow::Borrowed(line),
    }
}

/// What a query matches against for one line: the whole line by default, the
/// `--nth` fields when asked, with the SGR codes removed under `--ansi`.
pub fn search_key(line: &str, look: &crate::fzf::Look) -> String {
    let plain = |s: &str| match look.ansi {
        true => crate::fzf::strip_ansi(s),
        false => s.to_string(),
    };
    if look.nth.is_empty() && look.with_nth.is_empty() {
        return plain(line);
    }
    let delim = look.delimiter.as_deref();
    let tokens = crate::fzf::tokenize(line, delim);
    let ranges = match look.nth.is_empty() {
        true => &look.with_nth,
        false => &look.nth,
    };
    plain(&crate::fzf::transform(&tokens, ranges, delim))
}

/// Rank a whole corpus against one query the way the picker does: score across
/// cores, then order by score, then fzf's `--tiebreak=length`, with the stable
/// sort leaving anything still equal in input order (fzf's `index` criterion).
/// Returns the matching line indices, best first. Shared by `--filter` so the
/// two surfaces can't drift apart.
pub fn rank(
    lines: &[&str],
    pat: &str,
    exact: bool,
    no_sort: bool,
    tiebreak_length: bool,
    tac: bool,
    look: &crate::fzf::Look,
) -> Vec<usize> {
    let mut hits: Vec<(i32, usize)> = lines
        .par_iter()
        .enumerate()
        .filter_map(|(i, line)| score_line(&search_key(line, look), pat, exact).map(|s| (s, i)))
        .collect();
    hits.par_sort_by_key(|(_, i)| *i);
    // `--tac` reversed the input, so every order derived from it reverses too.
    if tac {
        hits.reverse();
    }
    if !no_sort {
        hits.sort_by(|a, b| {
            b.0.cmp(&a.0).then_with(|| match tiebreak_length {
                true => trim_length(lines[a.1]).cmp(&trim_length(lines[b.1])),
                false => std::cmp::Ordering::Equal,
            })
        });
    }
    hits.into_iter().map(|(_, i)| i).collect()
}

/// Score a line against the query with the active mode (exact substring or fuzzy).
pub fn score_line(line: &str, pat: &str, exact: bool) -> Option<i32> {
    if pat.is_empty() {
        return Some(0);
    }
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

/// Map one keystroke to the fzf key name a `--bind` could have used. Escape
/// sequences (arrows, PgUp/PgDn, Home/End, Shift-Tab, Alt-<char>) return the
/// bytes they consumed; a single byte consumes one.
fn fzf_key(buf: &[u8], i: usize) -> Option<(crate::fzf::Key, usize)> {
    use crate::fzf::Key;
    let b = *buf.get(i)?;
    if b == 0x1b {
        // CSI: ESC [ …
        if buf.get(i + 1) == Some(&b'[') {
            let k = match buf.get(i + 2)? {
                b'A' => Key::Up,
                b'B' => Key::Down,
                b'C' => Key::Right,
                b'D' => Key::Left,
                b'H' => Key::Home,
                b'F' => Key::End,
                b'Z' => Key::BTab,
                // ESC [ N ~ — the numbered navigation keys.
                d @ (b'1' | b'3' | b'4' | b'5' | b'6') if buf.get(i + 3) == Some(&b'~') => {
                    let k = match d {
                        b'1' => Key::Home,
                        b'3' => Key::Delete,
                        b'4' => Key::End,
                        b'5' => Key::PageUp,
                        _ => Key::PageDown,
                    };
                    return Some((k, 4));
                }
                _ => return None,
            };
            return Some((k, 3));
        }
        // Alt-<char> arrives as Esc followed by the character.
        if let Some(&c) = buf.get(i + 1) {
            if (0x20..0x7f).contains(&c) {
                return Some((Key::Alt(c.to_ascii_lowercase() as char), 2));
            }
        }
        return Some((Key::Esc, 1));
    }
    let k = match b {
        0x09 => Key::Tab,
        0x0d | 0x0a => Key::Enter,
        0x08 | 0x7f => Key::Backspace,
        0x20 => Key::Space,
        0x01..=0x1a => Key::Ctrl((b'a' + b - 1) as char),
        0x21..=0x7e => Key::Char(b as char),
        _ => return None,
    };
    Some((k, 1))
}

/// Run an fzf `--bind` action chain against the picker state. Returns true when
/// the action ends the session (`accept`/`abort`), so the reader stops.
fn fzf_action(c: &mut Controls, acts: &[crate::fzf::Action]) -> bool {
    use crate::fzf::Action;
    // Never zero: a page move must advance even before the first frame lands.
    let page = c.fzf_page.max(1);
    let count = c.fzf_count;
    let cycle = c.look.cycle;
    // fzf's movement actions are SCREEN directions. In the bottom-up default
    // layout "down" walks toward the prompt — i.e. toward the better match — so
    // every move flips against the ranked index arb stores in `cursor`.
    let inv = c.look.layout == crate::fzf::Layout::Default;
    for a in acts {
        let a = &if inv {
            match a {
                Action::Up => Action::Down,
                Action::Down => Action::Up,
                Action::PageUp => Action::PageDown,
                Action::PageDown => Action::PageUp,
                Action::HalfPageUp => Action::HalfPageDown,
                Action::HalfPageDown => Action::HalfPageUp,
                other => *other,
            }
        } else {
            *a
        };
        match a {
            Action::Up => {
                c.cursor = match (c.cursor, cycle, count) {
                    (0, true, n) if n > 0 => n - 1,
                    (cur, _, _) => cur.saturating_sub(1),
                }
            }
            Action::Down => {
                let next = c.cursor.saturating_add(1);
                c.cursor = if cycle && count > 0 && next >= count {
                    0
                } else {
                    next
                };
            }
            Action::PageUp => c.cursor = c.cursor.saturating_sub(page),
            Action::PageDown => c.cursor = c.cursor.saturating_add(page),
            Action::HalfPageUp => c.cursor = c.cursor.saturating_sub(page / 2 + 1),
            Action::HalfPageDown => c.cursor = c.cursor.saturating_add(page / 2 + 1),
            Action::First => c.cursor = 0,
            // The renderer clamps to the last row, so "past the end" IS last.
            Action::Last => c.cursor = usize::MAX,
            Action::Toggle => {
                let at = c.cursor;
                c.toggles.push(at);
            }
            Action::ToggleAll => c.toggle_all = true,
            Action::Accept => {
                c.submit = true;
                return true;
            }
            Action::Abort => {
                c.quit = true;
                return true;
            }
            Action::ClearQuery => {
                c.filter.clear();
                c.cursor = 0;
            }
            Action::BackwardDeleteChar => {
                c.filter.pop();
                c.cursor = 0;
            }
            Action::Ignore => {}
        }
    }
    false
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
///
/// Reads are chunked and the terminal splits a burst at an arbitrary byte — a
/// trackpad-momentum scroll emits `ESC[<64;…M` wheel reports faster than the
/// reader drains them — so a truncated tail is carried into the next read
/// instead of being decoded (see `feed_keys`/`partial_escape`). Without the
/// carry, the remainder of a split report typed itself into the filter as
/// `[<64;33;10M`.
fn spawn_key_handler(controls: Arc<Mutex<Controls>>) {
    if let Ok(mut tty) = OpenOptions::new().read(true).open("/dev/tty") {
        thread::spawn(move || {
            let fd = tty.as_raw_fd();
            // Big enough that a whole wheel burst normally lands in one read;
            // the carry covers the bursts that still straddle a boundary.
            let mut buf = [0u8; 4096];
            let mut pend: Vec<u8> = Vec::new();
            loop {
                let n = match tty.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    // A signal (SIGWINCH on resize, SIGCHLD from a `spawn`
                    // source) interrupts the read. Retry: bailing kills the only
                    // key reader and wedges the UI with no working keys at all,
                    // not even Esc/Ctrl-C (raw mode already disabled SIGINT).
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                pend.extend_from_slice(&buf[..n]);
                if drain_keys(&controls, &mut pend, true) {
                    break;
                }
                // Leftover bytes = a truncated escape at the tail. Give the rest
                // of the sequence a moment to arrive; if nothing comes it was a
                // real Esc keypress, so replay the tail literally rather than
                // swallowing the key.
                if !pend.is_empty()
                    && !tty_readable(fd, ESC_WAIT_MS)
                    && drain_keys(&controls, &mut pend, false)
                {
                    break;
                }
            }
        });
    }
}

/// How long a truncated escape tail waits for the rest of its sequence before it
/// is treated as a literal Esc keypress — the same ~25-50ms window every
/// terminal app uses to tell `Esc` from the head of an escape sequence.
const ESC_WAIT_MS: i32 = 30;

/// Whether `fd` has bytes readable within `ms` — a wait, never a read.
///
/// `select(2)`, not `poll(2)`: on Darwin, polling a tty returns `POLLNVAL`
/// (verified: `rc=1 revents=32` on a pty), so a `rc > 0` test reads as "data
/// waiting" and the reader blocks in `read` — Esc then does nothing until the
/// next keypress. `select` reports ttys correctly on both Darwin and Linux.
fn tty_readable(fd: RawFd, ms: i32) -> bool {
    if fd < 0 || fd as usize >= libc::FD_SETSIZE {
        return false; // unrepresentable in an fd_set: don't wait, decode now
    }
    unsafe {
        let mut set: libc::fd_set = std::mem::zeroed();
        libc::FD_SET(fd, &mut set);
        let mut tv = libc::timeval {
            tv_sec: (ms / 1000) as libc::time_t,
            tv_usec: (ms % 1000 * 1000) as libc::suseconds_t,
        };
        libc::select(
            fd + 1,
            &mut set,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut tv,
        ) > 0
    }
}

/// Feed `pend` through `feed_keys` and drop what it consumed. Returns `true`
/// when the reader should stop (quit/submit); whatever is left in `pend` is a
/// truncated escape sequence to carry into the next read.
fn drain_keys(controls: &Arc<Mutex<Controls>>, pend: &mut Vec<u8>, defer: bool) -> bool {
    let (used, stop) = feed_keys(controls, pend, defer);
    pend.drain(..used);
    stop
}

/// Decode one chunk of tty bytes into `Controls`. Returns `(consumed, stop)`:
/// under `defer` a truncated escape sequence at the tail is left unconsumed for
/// the caller to carry; without it every byte is decoded (so a tail that is just
/// `ESC` is the Esc key). `stop` means quit/submit — the reader thread is done.
fn feed_keys(controls: &Arc<Mutex<Controls>>, buf: &[u8], defer: bool) -> (usize, bool) {
    let n = buf.len();
    let mut c = controls.lock().unwrap();
    let fzf = c.fzf;
    // Form mode: `input` widgets present (and not fzf) — typing edits
    // the focused input, Tab cycles focus.
    let form = !fzf && !c.inputs.is_empty();
    let mut i = 0;
    while i < n {
        // The terminal split an escape sequence across reads: stop
        // here and let the caller carry the tail into the next
        // chunk. Decoding it now would type the remainder into the
        // filter as literal text.
        if defer && partial_escape(buf, i) {
            break;
        }
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
        // SGR mouse report `ESC[<…M/m` — must precede the CSI branch
        // below, which would otherwise consume it without
        // dispatching. Click/scroll/drag are hit-tested +
        // dispatched; a report this parser rejects falls through to
        // the CSI branch, which swallows it whole rather than
        // letting its bytes type themselves.
        if b == 0x1b && buf.get(i + 1) == Some(&b'[') && buf.get(i + 2) == Some(&b'<') {
            if let Some((ev, used)) = parse_sgr_mouse(&buf[..n], i) {
                dispatch_mouse(&mut c, ev, fzf, Instant::now());
                if c.quit {
                    return (i, true);
                }
                i += used;
                continue;
            }
        }
        // Theme chooser (Ctrl-T popup): while open it captures all keys
        // — arrows/j/k navigate + live-preview, Enter saves, Esc/q
        // cancels, Ctrl-C still quits. Everything else is swallowed.
        if c.theme_picker_open {
            if let Some(len) = csi_len(buf, i) {
                match buf[i + len - 1] {
                    b'A' => theme_picker_move(&mut c, -1),
                    b'B' => theme_picker_move(&mut c, 1),
                    _ => {}
                }
                i += len;
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
                    return (i, true);
                }
                _ => {}
            }
            i += 1;
            continue;
        }
        // `--expect` keys accept the selection and report themselves.
        if fzf && !c.look.expect.is_empty() {
            if let Some((key, used)) = fzf_key(&buf[..n], i) {
                if let Some(name) = c
                    .look
                    .expect
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, name)| name.clone())
                {
                    c.expect_key = Some(name);
                    c.submit = true;
                    let _ = used;
                    return (i, true);
                }
            }
        }
        // fzf `--bind` wins over arb's built-in picker keys, so the
        // bindings in a user's `$FZF_DEFAULT_OPTS` (`tab:toggle+down`,
        // `ctrl-n:page-down`, …) behave exactly as they do under fzf.
        if fzf && !c.look.binds.is_empty() {
            if let Some((key, used)) = fzf_key(&buf[..n], i) {
                if let Some(acts) = c.look.bound(key).map(<[_]>::to_vec) {
                    if fzf_action(&mut c, &acts) {
                        return (i, true);
                    }
                    i += used;
                    continue;
                }
            }
        }
        // Any other CSI: dispatch on its final byte (arrows
        // `ESC[A/B/C/D` move the cursor; a form slider adjusts on
        // Left/Right, a facet moves its cursor on Up/Down) and
        // consume the WHOLE sequence. Consuming a fixed 3 bytes left
        // the tail of a longer report (`ESC[1;5A` for Ctrl-Up,
        // `ESC[15~` for F5, `ESC[200~` bracketed paste) to type
        // itself into the filter as `;5A` / `5~`.
        if let Some(len) = csi_len(buf, i) {
            match buf[i + len - 1] {
                // Movement goes through the fzf action path so the
                // bottom-up layout flips it exactly once.
                b'A' if fzf => {
                    fzf_action(&mut c, &[crate::fzf::Action::Up]);
                }
                b'B' if fzf => {
                    fzf_action(&mut c, &[crate::fzf::Action::Down]);
                }
                // Facet and Sel both move a row cursor with Up/Down.
                b'A' if matches!(fk, ControlKind::Facet | ControlKind::Sel) => {
                    let f = c.focus;
                    c.control_meta[f].cursor = c.control_meta[f].cursor.saturating_sub(1);
                }
                b'B' if matches!(fk, ControlKind::Facet | ControlKind::Sel) => {
                    let f = c.focus;
                    c.control_meta[f].cursor = c.control_meta[f].cursor.saturating_add(1);
                }
                b'C' if fk == ControlKind::Slider => slider_key(&mut c, true),
                b'D' if fk == ControlKind::Slider => slider_key(&mut c, false),
                _ => {}
            }
            i += len;
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
                        return (i, true);
                    }
                }
            }
            0x03 => {
                c.quit = true;
                return (i, true);
            }
            // fzf select: Enter submits; Ctrl-N/J = down, Ctrl-P/K = up.
            0x0d if fzf => {
                c.submit = true;
                return (i, true);
            }
            0x0e | 0x0a if fzf => {
                fzf_action(&mut c, &[crate::fzf::Action::Down]);
            }
            0x10 | 0x0b if fzf => {
                fzf_action(&mut c, &[crate::fzf::Action::Up]);
            }
            // Tab: fzf's own default binding is `toggle+down` —
            // mark the row, then move to the next one.
            0x09 if fzf => {
                fzf_action(
                    &mut c,
                    &[crate::fzf::Action::Toggle, crate::fzf::Action::Down],
                );
            }
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
                    return (i, true);
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
                        return (i, true);
                    }
                }
            }
        }
        i += 1;
    }
    (i, false)
}

/// Length of the complete CSI sequence starting at `i` — `ESC [`, parameter
/// bytes (`0x30..=0x3f`: digits, `;`, and the `<` of an SGR mouse report),
/// intermediate bytes (`0x20..=0x2f`), then a final byte (`0x40..=0x7e`).
/// `None` when the slice holds no complete sequence (truncated or malformed).
/// Pure — no tty. This is the ECMA-48 CSI shape, so one rule covers arrows,
/// modified arrows, F-keys, `~`-keys, mouse reports and bracketed paste alike.
pub fn csi_len(bytes: &[u8], i: usize) -> Option<usize> {
    if bytes.get(i)? != &0x1b || bytes.get(i + 1)? != &b'[' {
        return None;
    }
    let mut p = i + 2;
    while matches!(bytes.get(p), Some(0x30..=0x3f)) {
        p += 1;
    }
    while matches!(bytes.get(p), Some(0x20..=0x2f)) {
        p += 1;
    }
    match bytes.get(p) {
        Some(0x40..=0x7e) => Some(p + 1 - i),
        _ => None,
    }
}

/// Whether `bytes[i..]` is the truncated head of an escape sequence — the
/// terminal split the report across reads, so the tail has to be carried into
/// the next read instead of being decoded now. A bare `ESC` at the very end
/// counts (it may be the head of a CSI); the caller resolves that ambiguity with
/// the escape-wait, so a real Esc keypress still lands. Pure — no tty.
pub fn partial_escape(bytes: &[u8], i: usize) -> bool {
    match bytes.get(i) {
        Some(0x1b) => match bytes.get(i + 1) {
            None => true,
            Some(b'[') => csi_len(bytes, i).is_none(),
            Some(_) => false, // ESC + non-CSI (`ESC O …`, Alt-key): decode now
        },
        _ => false,
    }
}

/// Run the TUI until the user quits (`q`/Esc/Ctrl-C). Renders to `/dev/tty` (the
/// terminal), NOT stdout — like fzf — so stdout stays a clean data channel for a
/// downstream consumer (`find / | arb | consumer`). Unlike fzf, arb never blocks
/// the pipeline: the caller tees stdin→stdout live in a separate thread while
/// this loop draws. The terminal is always restored (raw mode off, alternate
/// screen left, cursor shown) before returning, even on a draw error.
/// One fzf candidate. Identity — no `--with-nth` projection, no `search` key,
/// i.e. `find / | arb --fzf` — is the overwhelmingly common case and keeps ONE
/// handle to the line the stream already allocated: display, key and original
/// are the same string. A projection stores the three separately, boxed, so the
/// common case doesn't pay for it (24 bytes a candidate instead of 48).
#[derive(Clone)]
pub enum FzfCand {
    Ident(Arc<str>),
    Proj(Box<[Arc<str>; 3]>),
}

impl FzfCand {
    /// What the row shows.
    fn disp(&self) -> &Arc<str> {
        match self {
            FzfCand::Ident(a) => a,
            FzfCand::Proj(p) => &p[0],
        }
    }
    /// What the query matches against (`search`/`--nth`).
    fn key(&self) -> &Arc<str> {
        match self {
            FzfCand::Ident(a) => a,
            FzfCand::Proj(p) => &p[1],
        }
    }
    /// What Enter emits.
    fn orig(&self) -> &Arc<str> {
        match self {
            FzfCand::Ident(a) => a,
            FzfCand::Proj(p) => &p[2],
        }
    }
}
/// A scored candidate: `(score, candidate index)`. The match and hit lists hold
/// INDICES, not clones: at a million lines a `(score, Arc, Arc, Arc)` hit was
/// 56 bytes against 8, and the ranking sort moved all of it.
type FzfHit = (i32, u32);

pub fn run(
    spec: &Spec,
    state: Arc<Mutex<StreamState>>,
    controls: Arc<Mutex<Controls>>,
    down: Option<(Arc<Mutex<StreamState>>, String)>,
    err: Option<(Arc<Mutex<StreamState>>, String)>,
    fzf: bool,
    height: Option<String>,
) -> io::Result<()> {
    let mut tty: File = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    enable_raw_mode()?;
    // `--height`: render inline (a viewport of N rows below the cursor, keeping
    // the scrollback) instead of taking over the whole screen with the alternate
    // buffer. `N%` is a fraction of the terminal height. The viewport is placed
    // by hand — ratatui's `Viewport::Inline` would ask CROSSTERM for the cursor
    // position, and that query goes to stdout (arb's data channel) and never
    // returns when stdout is a pipe.
    // The look is fixed before the first frame (env + argv), so reading it once
    // here is enough to size the viewport the way fzf sizes it.
    let (look0, header0) = {
        let c = controls.lock().unwrap();
        (c.look.clone(), c.header.clone())
    };
    let inline = height.as_deref().and_then(parse_height).map(|(rows, pct)| {
        let (cols, total) = size().unwrap_or((80, 24));
        // fzf's `--min-height` floor, percentage heights only. `10+` counts LIST
        // rows, so the prompt, info rule, header and border are added on top.
        let rows = match pct {
            true => {
                let chrome =
                    1 + u16::from(!matches!(
                        look0.info,
                        crate::fzf::Info::Inline(_) | crate::fzf::Info::InlineRight(_)
                    )) + u16::from(!header0.is_empty())
                        + look0.border.map_or(0, |_| 2);
                rows.max(min_height_rows(
                    look0.min_height,
                    look0.min_height_plus,
                    chrome,
                    total,
                ))
            }
            false => rows,
        };
        // Reserve the room exactly as fzf does: wipe from the cursor down, then
        // print one newline per picker row after the first — the terminal
        // scrolls by itself when the picker would run off the bottom. Then ask
        // where the cursor ended up; that row is the picker's LAST line, so no
        // arithmetic has to guess whether a scroll happened.
        let _ = tty.write_all(b"\x1b[J");
        let _ = tty.write_all("\n".repeat(rows.saturating_sub(1) as usize).as_bytes());
        let _ = tty.flush();
        let bottom = tty_cursor_row(&mut tty).unwrap_or(total.saturating_sub(1));
        inline_rect(bottom, rows, cols, total)
    });
    let backend = CrosstermBackend::new(tty);
    let mut terminal = match inline {
        Some(area) => {
            let mut t = Terminal::with_options(
                backend,
                TerminalOptions {
                    viewport: Viewport::Fixed(area),
                },
            )?;
            // Wipe the region before the first frame. A fixed viewport starts
            // out believing the screen is already blank, so it emits nothing for
            // the cells it draws blank — leaving whatever the shell left there
            // showing through the picker.
            t.clear()?;
            t
        }
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
    let mut fzf_matched: Vec<u32> = Vec::new(); // display order, as candidate indices
    let mut fzf_last_sort = Instant::now() - Duration::from_secs(1);
    // `expect /re/ …` reactions: total stream lines already checked against the
    // patterns. Tracked by `total` (lines-ever), not a deque index, because the
    // dashboard ring drops old lines — we scan the newest arrivals each frame.
    let mut expect_total: u64 = 0;
    // fzf prompt/header/look (set once by `main` before this call — the look is
    // resolved from `$FZF_DEFAULT_OPTS` plus the command line and never changes
    // mid-run, so one snapshot serves every frame).
    let (fzf_prompt, fzf_header, fzf_exact, fzf_no_sort, fzf_multi, fzf_look) = {
        let c = controls.lock().unwrap();
        let p = if c.prompt.is_empty() {
            "> ".to_string()
        } else {
            c.prompt.clone()
        };
        (
            p,
            c.header.clone(),
            c.exact,
            c.no_sort,
            c.multi,
            c.look.clone(),
        )
    };

    // `timeout Ns …` idle reactions: track the last stream `total` and when it
    // last advanced; each timeout fires once per idle span, re-armed on a new line.
    let mut to_last_total: u64 = { state.lock().unwrap().total };
    let mut to_last_activity = Instant::now();
    let mut to_fired = vec![false; spec.timeouts.len()];
    // `bind <Resize>`: poll the terminal size each frame; fire on a change.
    let mut last_size = size().unwrap_or((0, 0));
    // fzf-mode scroll offset, carried between frames (see `render_fzf`).
    let mut fzf_start = 0usize;

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
                st.lines.iter().skip(start).map(|l| l.to_string()).collect()
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
                .map(|l| l.to_string())
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
                // Time-box the batch so a firehose producer (`find /`) can't make
                // one frame do unbounded work while holding the stream lock. A
                // budget rather than a line count: the point is to bound the
                // frame, and a fixed count also capped THROUGHPUT (50k per 20ms
                // frame put a 2.5M lines/s ceiling on ingest, which a million-line
                // stream noticed).
                const INGEST_BUDGET: Duration = Duration::from_millis(8);
                let ingest_started = Instant::now();
                let mut ingest_end = st.lines.len();
                for i in fzf_raw_done..st.lines.len() {
                    // Check the clock every 4k lines — `Instant::now()` per line
                    // would cost more than the work it guards.
                    if i % 4096 == 0 && ingest_started.elapsed() >= INGEST_BUDGET {
                        ingest_end = i;
                        break;
                    }
                    let raw = &st.lines[i];
                    // `--header-lines N`: the first N lines are a header, not
                    // candidates (fzf reads them off the same stream).
                    if i < fzf_look.header_lines {
                        continue;
                    }
                    // `--with-nth` reshapes what the row SHOWS, `--nth` what the
                    // query matches, and `--ansi` matches the text with the SGR
                    // codes removed. Each is a per-line derivation, so it lives
                    // beside the projection pipeline rather than inside it.
                    if !fzf_look.nth.is_empty() || !fzf_look.with_nth.is_empty() || fzf_look.ansi {
                        let delim = fzf_look.delimiter.as_deref();
                        let tokens = crate::fzf::tokenize(raw, delim);
                        let disp: Arc<str> = match fzf_look.with_nth.is_empty() {
                            true => Arc::clone(raw),
                            false => Arc::from(
                                crate::fzf::transform(&tokens, &fzf_look.with_nth, delim).as_str(),
                            ),
                        };
                        let key_src = match fzf_look.nth.is_empty() {
                            true => disp.to_string(),
                            false => crate::fzf::transform(&tokens, &fzf_look.nth, delim),
                        };
                        let key: Arc<str> = match fzf_look.ansi {
                            true => Arc::from(crate::fzf::strip_ansi(&key_src).as_str()),
                            false => Arc::from(key_src.as_str()),
                        };
                        // Under `--ansi` the codes are METADATA: fzf's item text
                        // — what it matches, prints on Enter and writes under
                        // `--filter` — is the line without them.
                        let orig: Arc<str> = match fzf_look.ansi {
                            true => Arc::from(crate::fzf::strip_ansi(raw).as_str()),
                            false => Arc::clone(raw),
                        };
                        fzf_cands.push(FzfCand::Proj(Box::new([disp, key, orig])));
                        continue;
                    }
                    // Identity fast-path: display, key and original are all the
                    // line the stream already holds — one shared handle.
                    if proj.is_empty() && search_proj.is_empty() {
                        fzf_cands.push(FzfCand::Ident(Arc::clone(raw)));
                        continue;
                    }
                    let orig: Arc<str> = Arc::clone(raw);
                    let key: Option<Arc<str>> = if search_proj.is_empty() {
                        None
                    } else {
                        Some(Arc::from(
                            project_line(&search_proj, raw).join(" ").as_str(),
                        ))
                    };
                    for disp in project_line(&proj, raw) {
                        let d: Arc<str> = Arc::from(disp.as_str());
                        let k = key.clone().unwrap_or_else(|| d.clone());
                        fzf_cands.push(FzfCand::Proj(Box::new([d, k, orig.clone()])));
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
                            .filter_map(|(_, i)| {
                                score_line(fzf_cands[i as usize].key(), &filter, fzf_exact)
                                    .map(|s| (s, i))
                            })
                            .collect();
                        // keep fzf_processed — new candidates scored below
                    } else {
                        // Full rescan across cores (rayon) — first char / backspace.
                        // Match on the search key; the index carries everything else.
                        fzf_hits = fzf_cands
                            .par_iter()
                            .enumerate()
                            .filter_map(|(i, cand)| {
                                score_line(cand.key(), &filter, fzf_exact).map(|s| (s, i as u32))
                            })
                            .collect();
                        fzf_processed = n;
                    }
                    fzf_filter = filter.clone();
                    fzf_last_sort = Instant::now() - Duration::from_secs(1);
                }
                // Incorporate new candidates since the last frame.
                for (i, cand) in fzf_cands.iter().enumerate().take(n).skip(fzf_processed) {
                    if empty {
                        fzf_matched.push(i as u32);
                    } else if let Some(sc) = score_line(cand.key(), &filter, fzf_exact) {
                        fzf_hits.push((sc, i as u32));
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
                    // `--tac` reversed the input, so every order derived from it
                    // reverses too: the whole list under `--no-sort`, and the
                    // `index` tie-break under a normal ranking.
                    if fzf_look.tac {
                        h.reverse();
                    }
                    // `--no-sort` keeps the input (scan) order; else rank
                    // best-first, breaking ties fzf's way: `--tiebreak=length`
                    // (its default) puts the shorter match first, and the sort
                    // is stable so anything still equal keeps input order —
                    // which is fzf's final `index` criterion.
                    if !fzf_no_sort {
                        h.par_sort_by(|a, b| {
                            b.0.cmp(&a.0).then_with(|| {
                                if fzf_look.tiebreak_length {
                                    trim_length(fzf_cands[a.1 as usize].disp())
                                        .cmp(&trim_length(fzf_cands[b.1 as usize].disp()))
                                } else {
                                    std::cmp::Ordering::Equal
                                }
                            })
                        });
                    }
                    fzf_matched = h.into_iter().map(|(_, i)| i).collect();
                    fzf_last_sort = now;
                }
            }
            let matched = &fzf_matched;
            let sel = cursor.min(matched.len().saturating_sub(1));
            // The cursor is a DISPLAY position; `--tac` flips it to a rank.
            // `--tac` reverses the INPUT order, so it only flips the view while
            // the list is still in input order (no query). Once a query ranks
            // the matches, it survives as the tie-break direction instead — the
            // hit set is reversed before the stable sort below.
            let tac_view = fzf_look.tac && filter.is_empty();
            let rank = |pos: usize| tac_index(pos, matched.len(), tac_view);

            // A display row's ORIGINAL line, resolved through the candidate list.
            let original = |pos: usize| -> Option<&str> {
                matched
                    .get(pos)
                    .map(|i| fzf_cands[*i as usize].orig().as_ref())
            };

            let mut c = controls.lock().unwrap();
            // Publish the cursor's ORIGINAL line so a `--preview` thread acts on
            // what would be emitted, not the projected display.
            c.current = original(rank(sel)).unwrap_or_default().to_string();
            // `toggle`: flip each pending row's original in the mark set. Every
            // press queued its own row, so a burst of Tabs marks every one of
            // them, and the cursor move fzf pairs with `toggle+down` has already
            // been applied by the key handler.
            for at in std::mem::take(&mut c.toggles) {
                let at = rank(at.min(matched.len().saturating_sub(1)));
                let orig = original(at).filter(|_| c.multi).map(str::to_string);
                if let Some(orig) = orig {
                    match c.marks.iter().position(|m| *m == orig) {
                        Some(pos) => {
                            c.marks.remove(pos);
                        }
                        None => c.marks.push(orig),
                    }
                }
            }
            if c.toggle_all {
                // `toggle-all`: clear the marks if everything is already marked,
                // else mark every current match.
                c.toggle_all = false;
                if !c.multi {
                    // Single-select: `toggle-all` is a no-op, like fzf.
                } else if c.marks.len() == matched.len() {
                    c.marks.clear();
                } else {
                    c.marks = matched
                        .iter()
                        .map(|i| fzf_cands[*i as usize].orig().to_string())
                        .collect();
                }
            }
            // Publish the match count so `--cycle` wraps against a real length.
            c.fzf_count = matched.len();
            if submit {
                // Emit the marks if any (multi-select), else the cursor original.
                c.result = if c.marks.is_empty() {
                    original(rank(sel))
                        .map(str::to_string)
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
                (
                    d.lines.iter().map(|l| l.to_string()).collect(),
                    label.clone(),
                )
            });
            let prev_ref = prev_snap
                .as_ref()
                .map(|(l, lab)| (l.as_slice(), lab.as_str()));
            let mut hitmap: Vec<HitTarget> = Vec::new();
            let mut fzf_rows = 0usize;
            let draw = terminal.draw(|f| {
                (fzf_start, fzf_rows) = render_fzf(
                    f,
                    matched,
                    &fzf_cands,
                    &filter,
                    sel,
                    fzf_start,
                    &marks,
                    fzf_multi,
                    tac_view,
                    total,
                    err_ref,
                    prev_ref,
                    &fzf_prompt,
                    &fzf_header,
                    fzf_theme,
                    &fzf_look,
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
                // A `page-up`/`page-down` binding moves by what is on screen.
                c.fzf_page = fzf_rows;
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
                .map(|l| l.to_string())
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
                st.lines.iter().map(|l| l.to_string()).collect()
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

    // Hand the candidate/match lists to the OS instead of walking a million
    // `Arc` drops on the way out: the picker is exiting, and freeing one line at
    // a time is time the user spends staring at a dead terminal. (The stream
    // buffer still owns the text; this only skips the refcount teardown.)
    std::mem::forget(fzf_cands);
    std::mem::forget(fzf_hits);
    std::mem::forget(fzf_matched);

    let _ = execute!(terminal.backend_mut(), DisableMouseCapture);
    disable_raw_mode()?;
    if let Some(area) = inline {
        // Inline mode never entered the alternate screen. Put the cursor back at
        // the picker's top-left and clear from there to the end of the screen —
        // one erase, the way fzf does it, so the next shell prompt lands exactly
        // where the picker started and nothing above it is touched.
        let _ = execute!(
            terminal.backend_mut(),
            MoveTo(0, area.y),
            TermClear(ClearType::FromCursorDown)
        );
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
        .map(|l| l.to_string())
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

/// Where the visible list window starts, given the previous frame's offset.
/// Scrolling picks up where it left off and slides only as far as the cursor
/// plus fzf's `--scroll-off` margin demands — recomputing from the cursor alone
/// would scroll at different moments than fzf does. The two margins are applied
/// in this order on purpose: on a window too short to honor both, the trailing
/// one wins, which is where fzf settles too.
pub fn fzf_window_start(
    prev: usize,
    sel: usize,
    list_h: usize,
    n: usize,
    scroll_off: usize,
) -> usize {
    let so = scroll_off.min(list_h.saturating_sub(1));
    let max_start = n.saturating_sub(list_h);
    let mut start = prev.min(max_start);
    if sel < start + so {
        start = sel.saturating_sub(so);
    }
    if sel + so >= start + list_h {
        start = (sel + so + 1).saturating_sub(list_h);
    }
    start.min(max_start)
}

/// Map a display position to its index in the ranked match list. `--tac` shows
/// the list in reverse input order, so display row 0 is the LAST match.
pub fn tac_index(pos: usize, n: usize, tac: bool) -> usize {
    if tac {
        n.saturating_sub(1).saturating_sub(pos)
    } else {
        pos
    }
}

/// The fzf cursor index for a clicked list row: `start` (the scroll offset of the
/// first visible row) plus the rows below `list_top`.
pub fn fzf_row_to_cursor(list_top: u16, start: usize, row: u16) -> usize {
    start + row.saturating_sub(list_top) as usize
}

/// The same, for fzf's `--layout=default`, where the list grows UPWARD from the
/// prompt: the top screen row is the last match, so the click mirrors within the
/// `rows` on screen.
pub fn fzf_row_to_cursor_rev(list_top: u16, start: usize, rows: usize, row: u16) -> usize {
    let off = row.saturating_sub(list_top) as usize;
    start + rows.saturating_sub(1).saturating_sub(off)
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
                            // `--layout=default` draws the list bottom-up, so a
                            // click maps to the mirrored row.
                            c.cursor = if fzf && c.look.layout == crate::fzf::Layout::Default {
                                fzf_row_to_cursor_rev(
                                    t.rect.y,
                                    c.fzf_list_start,
                                    c.fzf_page,
                                    ev.row,
                                )
                            } else {
                                fzf_row_to_cursor(t.rect.y, c.fzf_list_start, ev.row)
                            };
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
    let border = if focused { accent } else { Color::DarkGray };
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
    let border = if focused { accent } else { Color::DarkGray };
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
                        Style::default().fg(accent).add_modifier(Modifier::REVERSED)
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
    // The positions come from the SAME alignment that produced the score
    // (fzf's backtrace), so the highlight always marks the characters the
    // ranking was based on — a greedy left-to-right scan can mark others.
    let (p, cased) = crate::algo::prepare_pattern(pat);
    let text = crate::algo::Text::new(line);
    let hit = crate::algo::fuzzy_match_v2(cased, &text, &p, true)
        .or_else(|| crate::algo::exact_match_naive(cased, &text, &p, true));
    match hit {
        Some((_, Some(mut pos))) => {
            pos.sort_unstable();
            pos
        }
        _ => Vec::new(),
    }
}

/// Build one styled picker row in fzf's exact shape — `[pointer][marker][text]`,
/// with the fuzzy-matched characters highlighted. The pointer column carries the
/// accent on the current row and the gutter color on every other one (that dim
/// bar down the left edge is fzf's, not a decoration arb invented), and the mark
/// column shows the `--marker` glyph for a Tab-marked row.
fn fzf_line(
    line: &str,
    filter: &str,
    width: usize,
    marked: bool,
    current: bool,
    look: &crate::fzf::Look,
    colors: &crate::fzf::Colors,
) -> Line<'static> {
    let ptr_w = look.pointer.chars().count();
    let mark_w = look.marker.chars().count();
    let avail = width.saturating_sub(ptr_w + mark_w);
    // `--ansi`: the line's own SGR colours are part of the row. Decode them
    // once, then work with the plain text — matching, truncation and the
    // highlight all count CHARACTERS, and an escape sequence isn't one.
    let ansi_spans = look
        .ansi
        .then(|| ansi_line(line))
        .filter(|l| l.spans.len() > 1 || line.contains('\u{1b}'));
    let line = &match &ansi_spans {
        Some(l) => l
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>(),
        None => line.to_string(),
    };
    // Overlong rows end in the `--ellipsis` glyphs, like fzf's `··`.
    let text: String = if line.chars().count() > avail {
        let ell = look.ellipsis.chars().count();
        line.chars()
            .take(avail.saturating_sub(ell))
            .chain(look.ellipsis.chars())
            .collect()
    } else {
        line.chars().collect()
    };
    // The row background rides on the SPANS, not the whole row: fzf's `bg+` bar
    // ends where the text ends, it doesn't run to the right edge.
    let row_bg = if current { colors.bg_plus } else { colors.bg }.bg();
    let base_style = if current { colors.fg_plus } else { colors.fg }
        .fg()
        .patch(row_bg);
    // Each row character's own colour (from `--ansi`), by character index.
    let ansi_at: Vec<Style> = match &ansi_spans {
        Some(l) => l
            .spans
            .iter()
            .flat_map(|sp| {
                // A `\x1b[0m` reset decodes as an explicit `Reset` colour; that
                // means "no colour of its own", so the row's own `fg`/`fg+`
                // must show through rather than being overridden by it.
                let keep = match sp.style.fg {
                    Some(Color::Reset) | None => Style::default(),
                    Some(c) => Style::default().fg(c),
                };
                let keep = keep.add_modifier(sp.style.add_modifier);
                std::iter::repeat_n(keep, sp.content.chars().count())
            })
            .collect(),
        None => Vec::new(),
    };
    // A run of text keeps the line's own colour where `--ansi` gave it one; the
    // current row's `fg+`/`bg+` still wins, as it does in fzf.
    let styled_run = |from: usize, s: String, over: Style| -> Vec<Span<'static>> {
        if ansi_at.is_empty() {
            return vec![Span::styled(s, over)];
        }
        let mut out: Vec<Span<'static>> = Vec::new();
        let mut cur = String::new();
        let mut cur_style: Option<Style> = None;
        for (k, ch) in s.chars().enumerate() {
            let st = over.patch(*ansi_at.get(from + k).unwrap_or(&Style::default()));
            if cur_style != Some(st) && !cur.is_empty() {
                out.push(Span::styled(
                    std::mem::take(&mut cur),
                    cur_style.unwrap_or(over),
                ));
            }
            cur_style = Some(st);
            cur.push(ch);
        }
        if !cur.is_empty() {
            out.push(Span::styled(cur, cur_style.unwrap_or(over)));
        }
        out
    };
    // Current row: the `--pointer` glyph. Every other row: fzf's fixed gutter
    // block — it does NOT follow `--pointer`, it is the bar down the left edge.
    let pointer = if current {
        Span::styled(look.pointer.clone(), colors.pointer.fg().patch(row_bg))
    } else {
        Span::styled(
            "\u{258c}".repeat(ptr_w.max(1)),
            colors.gutter.fg().patch(row_bg),
        )
    };
    let gutter = if marked {
        Span::styled(look.marker.clone(), colors.marker.fg().patch(row_bg))
    } else {
        Span::styled(" ".repeat(mark_w), row_bg)
    };
    if filter.is_empty() {
        let mut spans = vec![pointer, gutter];
        spans.extend(styled_run(0, text, base_style));
        return Line::from(spans);
    }
    let pos: std::collections::HashSet<usize> =
        match_positions(&text, filter).into_iter().collect();
    // Matched characters take fzf's `hl` / `hl+` slot.
    let hl = if current { colors.hl_plus } else { colors.hl }
        .fg()
        .patch(row_bg);
    let mut spans = vec![pointer, gutter];
    let mut cur = String::new();
    let mut cur_hl = false;
    let mut run_start = 0usize;
    for (i, ch) in text.chars().enumerate() {
        let h = pos.contains(&i);
        if h != cur_hl && !cur.is_empty() {
            let s = std::mem::take(&mut cur);
            match cur_hl {
                // A matched run always takes the `hl` colour; an unmatched one
                // keeps whatever `--ansi` painted it.
                true => spans.push(Span::styled(s, hl)),
                false => spans.extend(styled_run(run_start, s, base_style)),
            }
            run_start = i;
        }
        cur_hl = h;
        cur.push(ch);
    }
    if !cur.is_empty() {
        match cur_hl {
            true => spans.push(Span::styled(cur, hl)),
            false => spans.extend(styled_run(run_start, cur, base_style)),
        }
    }
    Line::from(spans)
}

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

/// The fzf select view: prompt, match counter and list, laid out and colored the
/// way the `fzf` binary would for the same options (see [`crate::fzf`]). Returns
/// `(scroll offset of the first visible row, rows of list on screen)` — the
/// first maps a click to a cursor index, the second sizes a `page-up`/
/// `page-down` binding.
// Distinct render inputs (matches, filter, cursor, marks, panes, prompt/header).
#[allow(clippy::too_many_arguments)]
fn render_fzf(
    f: &mut Frame,
    // The display order, as indices into `cands` — the picker never holds a
    // second copy of the lines it is showing.
    matched: &[u32],
    cands: &[FzfCand],
    filter: &str,
    sel: usize,
    // `prev_start`: the previous frame's scroll offset, so the window slides
    // like fzf's instead of being recomputed from the cursor every frame.
    prev_start: usize,
    marks: &[String],
    multi: bool,
    // `tac`: whether the display is the ranked list read backwards. Not taken
    // from `look` — it only applies while the list is still in input order.
    tac: bool,
    total: u64,
    err: Option<(&[String], &str)>,
    preview: Option<(&[String], &str)>,
    prompt: &str,
    header: &str,
    theme: Option<crate::theme::Palette>,
    look: &crate::fzf::Look,
    hitmap: &mut Vec<HitTarget>,
) -> (usize, usize) {
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
    // fzf's `--border` wraps the WHOLE picker — list and preview together — so
    // the box is drawn first and everything else is laid out inside it.
    let main_top = top;
    // fzf's `--border`: the whole picker lives inside the box, so everything
    // below measures against the block's inner area.
    let body = match look.border {
        Some((btype, sides)) => {
            let block = Block::default()
                .borders(sides)
                .border_type(btype)
                .border_style(look.colors.border.fg());
            let inner = block.inner(main_top);
            f.render_widget(block, main_top);
            // fzf pads the boxed content by one column on the LEFT only; the
            // right-hand column is the scrollbar lane, bordered or not.
            Rect {
                x: inner.x + 1,
                width: inner.width.saturating_sub(1),
                ..inner
            }
        }
        None => main_top,
    };
    // Palette: fzf's own by default — that is what makes the drop-in
    // indistinguishable from `fzf`. An explicitly requested arb theme (or a
    // live Ctrl-T pick) re-maps the same slots onto that palette instead.
    let colors = match theme {
        Some(p) => {
            let acc = crate::fzf::Ent {
                color: Some(p.accent()),
                attrs: Modifier::BOLD,
            };
            let dim = crate::fzf::Ent {
                color: Some(p.dim()),
                attrs: Modifier::empty(),
            };
            crate::fzf::Colors {
                fg: crate::fzf::Ent {
                    color: Some(p.primary()),
                    attrs: Modifier::empty(),
                },
                bg: crate::fzf::Ent::default(),
                hl: acc,
                fg_plus: acc,
                bg_plus: crate::fzf::Ent {
                    color: Some(p.bg()),
                    attrs: Modifier::empty(),
                },
                hl_plus: acc,
                gutter: crate::fzf::Ent {
                    color: Some(p.bg()),
                    attrs: Modifier::empty(),
                },
                pointer: acc,
                marker: acc,
                prompt: acc,
                query: crate::fzf::Ent {
                    color: Some(p.primary()),
                    attrs: Modifier::empty(),
                },
                info: dim,
                spinner: acc,
                header: dim,
                border: dim,
                separator: dim,
                scrollbar: dim,
            }
        }
        None => look.colors,
    };

    // With `--preview`, the body splits: the select list on the left, the
    // preview box (command output for the cursor line) on the right. It takes
    // the same palette as the picker — fzf draws it in the `border` colour, not
    // a widget accent.
    let body = match preview {
        Some((lines, _)) => {
            // Inside a `--border` box fzf keeps a padding column to the RIGHT of
            // the preview as well, mirroring the one on the left; unboxed, the
            // preview runs to the last column.
            let area = match look.border.is_some() {
                true => Rect {
                    width: body.width.saturating_sub(1),
                    ..body
                },
                false => body,
            };
            let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);
            render_preview_pane(f, cols[1], lines, &colors);
            cols[0]
        }
        None => body,
    };

    let header_h: u16 = u16::from(!header.is_empty());
    // The counter gets its own row only in fzf's separator styles; the inline
    // styles ride on the prompt line and `hidden` takes no row at all.
    // `hidden` drops the counter but KEEPS the separator row (fzf 0.74 draws the
    // bare rule); only the inline styles give the row back to the list.
    let info_h: u16 = u16::from(!matches!(
        look.info,
        crate::fzf::Info::Inline(_) | crate::fzf::Info::InlineRight(_)
    ));
    // `--layout`: reverse puts the prompt on top; default and reverse-list put it
    // at the bottom (default additionally grows the list upward from it).
    let reverse = look.layout == crate::fzf::Layout::Reverse;
    let rows = if reverse {
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(info_h),
            Constraint::Length(header_h),
            Constraint::Min(0),
        ])
        .split(body)
    } else {
        Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(header_h),
            Constraint::Length(info_h),
            Constraint::Length(1),
        ])
        .split(body)
    };
    let (prompt_area, info_area, header_area, list_area) = if reverse {
        (rows[0], rows[1], rows[2], rows[3])
    } else {
        (rows[3], rows[2], rows[1], rows[0])
    };
    // The mark count is part of the counter whenever multi-select is on — fzf
    // shows `(0)` too, it isn't hidden until something is marked.
    let marked = if multi {
        format!(" ({})", marks.len())
    } else {
        String::new()
    };
    let counter = format!("{}/{total}{marked}", matched.len());
    // The rules stop one column short: that lane belongs to the scrollbar. With
    // `--no-scrollbar` the list (only) gets the column back.
    let rule = "\u{2500}";
    let text_w = |a: Rect| (a.width as usize).saturating_sub(1);
    let list_w = |a: Rect| (a.width as usize).saturating_sub(usize::from(look.scrollbar));

    // Prompt line: `prompt` slot, then the query in the `query` slot, then the
    // counter when an inline `--info` style asked for it there.
    let mut prompt_spans = vec![
        Span::styled(prompt.to_string(), colors.prompt.fg()),
        Span::styled(filter.to_string(), colors.query.fg()),
    ];
    let used = prompt.chars().count() + filter.chars().count();
    match &look.info {
        crate::fzf::Info::Inline(prefix) => {
            // The blank after the query is the cursor cell fzf keeps free; the
            // separator then runs from the counter to the scrollbar lane.
            let head = format!(" {prefix}{counter} ");
            let fill = text_w(prompt_area).saturating_sub(used + head.chars().count());
            prompt_spans.push(Span::styled(head, colors.info.fg()));
            prompt_spans.push(Span::styled(rule.repeat(fill), colors.separator.fg()));
        }
        crate::fzf::Info::InlineRight(prefix) => {
            let text = format!("{prefix}{counter}");
            let pad = text_w(prompt_area).saturating_sub(used + text.chars().count());
            prompt_spans.push(Span::raw(" ".repeat(pad)));
            prompt_spans.push(Span::styled(text, colors.info.fg()));
        }
        _ => {}
    }
    f.render_widget(Paragraph::new(Line::from(prompt_spans)), prompt_area);
    // A real terminal cursor after the query, like fzf — not a drawn glyph.
    let cx = prompt_area.x + used as u16;
    f.set_cursor_position((cx.min(prompt_area.right().saturating_sub(1)), prompt_area.y));

    // Info row: the counter on one end of a horizontal separator that runs to
    // the scrollbar lane. `hidden` keeps the rule and drops the counter.
    if info_h == 1 {
        let w = text_w(info_area);
        let cw = counter.chars().count();
        let spans = match look.info {
            crate::fzf::Info::Hidden => {
                vec![Span::styled(rule.repeat(w), colors.separator.fg())]
            }
            crate::fzf::Info::Right => {
                let fill = w.saturating_sub(cw + 1);
                vec![
                    Span::styled(format!("{} ", rule.repeat(fill)), colors.separator.fg()),
                    Span::styled(counter.clone(), colors.info.fg()),
                ]
            }
            _ => {
                let fill = w.saturating_sub(cw + 3);
                vec![
                    Span::raw("  "),
                    Span::styled(counter.clone(), colors.info.fg()),
                    Span::styled(format!(" {}", rule.repeat(fill)), colors.separator.fg()),
                ]
            }
        };
        f.render_widget(Paragraph::new(Line::from(spans)), info_area);
    }
    if !header.is_empty() {
        // fzf indents the header to clear the pointer + marker columns.
        f.render_widget(
            Paragraph::new(format!("  {header}")).style(colors.header.fg()),
            header_area,
        );
    }

    let inner_w = list_w(list_area);
    // Only build ListItems for the VISIBLE window around the cursor — not the
    // whole (possibly million-line) match list. This is what keeps arb as fast as
    // fzf: fuzzy-highlighting and allocation happen for ~a screenful, not all rows.
    let list_h = list_area.height as usize;
    let n = matched.len();
    let sel = sel.min(n.saturating_sub(1));
    let max_start = n.saturating_sub(list_h);
    let start = fzf_window_start(prev_start, sel, list_h, n, look.scroll_off);
    let end = (start + list_h.max(1)).min(n);
    let mark_set: std::collections::HashSet<&str> = marks.iter().map(String::as_str).collect();
    let mut items: Vec<ListItem> = (start..end)
        // `--tac` walks the ranked list backwards; without it this is `[start..end]`.
        .filter_map(|pos| {
            matched
                .get(tac_index(pos, n, tac))
                .and_then(|i| cands.get(*i as usize))
                .map(|cand| (pos, (cand.disp(), cand.orig())))
        })
        // Show the projected display; a row is marked when its ORIGINAL is marked.
        .map(|(pos, (disp, orig))| {
            let current = n > 0 && pos == sel;
            let row = fzf_line(
                disp,
                filter,
                inner_w,
                mark_set.contains(orig.as_ref()),
                current,
                look,
                &colors,
            );
            ListItem::new(row).style(colors.bg.bg())
        })
        .collect();
    // `--layout=default`: the best match sits at the BOTTOM, next to the prompt,
    // and a short list stays anchored there — so pad the top, not the bottom.
    if look.layout == crate::fzf::Layout::Default {
        items.reverse();
        let pad = list_h.saturating_sub(items.len());
        let blanks = std::iter::repeat_with(|| ListItem::new("")).take(pad);
        items = blanks.chain(items).collect();
    }
    f.render_widget(
        List::new(items),
        Rect {
            width: inner_w as u16,
            ..list_area
        },
    );
    // Scrollbar: fzf keeps the last column of the list for it and draws a thumb
    // sized and positioned by the visible window — nothing when everything fits.
    if look.scrollbar && n > list_h && list_h > 0 {
        let thumb = (list_h * list_h / n).max(1);
        // Thumb position is proportional to how far the WINDOW has scrolled, so
        // it parks on the last row at the end of the list (as fzf's does).
        let top = match max_start {
            0 => 0,
            m => start * (list_h - thumb) / m,
        };
        // The bottom-up layout mirrors the list, so the thumb mirrors with it.
        let top = match look.layout {
            crate::fzf::Layout::Default => list_h - thumb - top,
            _ => top,
        };
        let x = list_area.right().saturating_sub(1);
        for row in top..top + thumb {
            let cell = Rect {
                x,
                y: list_area.y + row as u16,
                width: 1,
                height: 1,
            };
            f.render_widget(
                Paragraph::new("\u{2502}").style(colors.scrollbar.fg()),
                cell,
            );
        }
    }
    // Publish the list body so a click maps to a cursor row (see dispatch_mouse
    // Select arm). `start` is the scroll offset of the first visible row.
    hitmap.clear();
    hitmap.push(HitTarget {
        rect: list_area,
        kind: WidgetKind::Select,
        control_name: String::new(),
        meta_index: None,
        tabs: Vec::new(),
    });
    (start, list_h)
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

/// The `--preview` pane as fzf draws it: a rounded box in the palette's border
/// colour with no title, its text inset one column. arb's own `-- CMD` pane
/// (the DSL's downstream view) keeps its label; this one has to look like fzf's,
/// which labels nothing unless `--preview-label` says so.
fn render_preview_pane(f: &mut Frame, area: Rect, lines: &[String], colors: &crate::fzf::Colors) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(colors.border.fg());
    let inner = block.inner(area);
    f.render_widget(block, area);
    // fzf leaves the box's top-left corner blank when the box touches the top of
    // the screen — with an outer `--border` above it, the corner is drawn. Match
    // both, or the two pickers differ by exactly one glyph.
    if area.y == 0 {
        f.render_widget(
            Paragraph::new(" "),
            Rect {
                x: area.x,
                y: area.y,
                width: 1,
                height: 1,
            },
        );
    }
    let inner = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(1),
        ..inner
    };
    let items: Vec<ListItem> = lines
        .iter()
        .take(inner.height as usize)
        .map(|l| ListItem::new(ansi_line(l)))
        .collect();
    f.render_widget(List::new(items), inner);
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
    c.alert = Some((
        format!("theme: {name}"),
        Instant::now() + Duration::from_secs(2),
    ));
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
                .map(|v| {
                    if v.is_finite() && *v > 0.0 {
                        *v as u64
                    } else {
                        0
                    }
                })
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
            events.add(today, Style::default().fg(Color::Black).bg(accent));
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
        WidgetKind::LogView => {
            // A `tail` whose rows are tinted by detected log level.
            let owned = result_lines(&result, lines);
            let inner_h = area.height.saturating_sub(2) as usize;
            let cap = widget_limit(w).map_or(inner_h, |n| inner_h.min(n));
            let skip = scroll_skip(owned.len(), cap, scroll);
            let items: Vec<ListItem> = owned
                .iter()
                .skip(skip)
                .take(cap)
                .map(|l| {
                    ListItem::new(Line::from(Span::styled(
                        clip(l, inner_w),
                        log_level_style(l, theme),
                    )))
                })
                .collect();
            f.render_widget(List::new(items).block(block), area);
            render_scrollbar(f, area, owned.len(), cap, skip, accent);
        }
        WidgetKind::Diff => {
            // Each line tinted by its leading diff char (+/-/@).
            let owned = result_lines(&result, lines);
            let inner_h = area.height.saturating_sub(2) as usize;
            let skip = scroll_skip(owned.len(), inner_h, scroll);
            let items: Vec<ListItem> = owned
                .iter()
                .skip(skip)
                .take(inner_h)
                .map(|l| {
                    let style = match l.chars().next() {
                        Some('+') => Style::default().fg(Color::Green),
                        Some('-') => Style::default().fg(Color::Red),
                        Some('@') => Style::default().fg(accent).add_modifier(Modifier::BOLD),
                        _ => Style::default().fg(Color::DarkGray),
                    };
                    ListItem::new(Line::from(Span::styled(clip(l, inner_w), style)))
                })
                .collect();
            f.render_widget(List::new(items).block(block), area);
            render_scrollbar(f, area, owned.len(), inner_h, skip, accent);
        }
        WidgetKind::Heatmap => {
            // The numeric series as a grid of shade-scaled cells (each 2 wide).
            let series: Vec<f64> = match &result {
                Some(QueryResult::Pairs(p)) => p.iter().map(|(_, v)| *v as f64).collect(),
                Some(QueryResult::Lines(ls)) => crate::query::numeric_series(ls),
                Some(QueryResult::Scalar(v)) => vec![*v],
                None => crate::query::numeric_series(lines),
            };
            let (min, max) = series
                .iter()
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &v| {
                    (a.min(v), b.max(v))
                });
            let span = (max - min).max(f64::MIN_POSITIVE);
            let cols = (inner_w / 2).max(1);
            let rows: Vec<Line> = series
                .chunks(cols)
                .map(|chunk| {
                    Line::from(
                        chunk
                            .iter()
                            .map(|&v| {
                                let t = if min.is_finite() {
                                    (v - min) / span
                                } else {
                                    0.0
                                };
                                Span::styled("  ", Style::default().bg(heat_color(t, theme)))
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();
            f.render_widget(Paragraph::new(rows).block(block), area);
        }
        WidgetKind::Treemap => {
            // Slice-and-dice treemap of tally/count pairs (proportional labeled
            // rects, colored across the palette).
            let pairs: Vec<(String, u64)> = match &result {
                Some(QueryResult::Pairs(p)) => p.clone(),
                _ => Vec::new(),
            };
            f.render_widget(block, area);
            let inner = Rect {
                x: area.x + 1,
                y: area.y + 1,
                width: area.width.saturating_sub(2),
                height: area.height.saturating_sub(2),
            };
            for (i, (rect, (label, val))) in treemap_rects(inner, &pairs)
                .into_iter()
                .zip(pairs.iter())
                .enumerate()
            {
                let col = slot_by_index(i, theme, accent);
                let body = format!("{label} {val}");
                f.render_widget(
                    Paragraph::new(clip(&body, rect.width.saturating_sub(1) as usize))
                        .style(Style::default().bg(col).fg(Color::Black)),
                    rect,
                );
            }
        }
        WidgetKind::Gantt => {
            // `label start end` lines as bars on a shared time axis.
            let owned = result_lines(&result, lines);
            let bars: Vec<(String, f64, f64)> = owned
                .iter()
                .filter_map(|l| {
                    let mut it = l.split_whitespace();
                    let label = it.next()?.to_string();
                    let s = it.next()?.parse::<f64>().ok()?;
                    let e = it.next()?.parse::<f64>().ok()?;
                    Some((label, s, e.max(s)))
                })
                .collect();
            let lo = bars
                .iter()
                .map(|(_, s, _)| *s)
                .fold(f64::INFINITY, f64::min);
            let hi = bars
                .iter()
                .map(|(_, _, e)| *e)
                .fold(f64::NEG_INFINITY, f64::max);
            let span = (hi - lo).max(f64::MIN_POSITIVE);
            let label_w = 12usize.min(inner_w / 2);
            let track = inner_w.saturating_sub(label_w + 1);
            let inner_h = area.height.saturating_sub(2) as usize;
            let rows: Vec<Line> = bars
                .iter()
                .take(inner_h)
                .enumerate()
                .map(|(i, (label, s, e))| {
                    let off = if lo.is_finite() {
                        (((s - lo) / span) * track as f64) as usize
                    } else {
                        0
                    };
                    let len = ((((e - s) / span) * track as f64) as usize).max(1);
                    let bar = " ".repeat(off) + &"█".repeat(len.min(track.saturating_sub(off)));
                    Line::from(vec![
                        Span::styled(
                            format!("{:<label_w$} ", clip(label, label_w)),
                            Style::default().fg(Color::Gray),
                        ),
                        Span::styled(bar, Style::default().fg(slot_by_index(i, theme, accent))),
                    ])
                })
                .collect();
            f.render_widget(Paragraph::new(rows).block(block), area);
        }
        WidgetKind::Logo => {
            f.render_widget(block, area);
            // Center the braille wordmark in the inner area.
            let logo_area = Rect {
                x: area.x + 1,
                y: area.y + area.height / 2,
                width: area.width.saturating_sub(2),
                height: 1,
            };
            f.render_widget(RatatuiLogo::default(), logo_area);
        }
        WidgetKind::Blank => {
            // A spacer: clear the cell, draw nothing.
            f.render_widget(Clear, area);
        }
        WidgetKind::Rule => {
            // A divider line in the accent (horizontal, or `-dir vertical`).
            let vertical = w.opts.get("dir").map(String::as_str) == Some("vertical");
            let body = if vertical {
                "│\n".repeat(area.height as usize)
            } else {
                "─".repeat(area.width as usize)
            };
            f.render_widget(
                Paragraph::new(body).style(Style::default().fg(accent)),
                area,
            );
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
fn render_scrollbar(
    f: &mut Frame,
    area: Rect,
    content: usize,
    visible: usize,
    pos: usize,
    accent: Color,
) {
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

/// Flatten a widget's evaluated result to display lines (the shared body of the
/// list-family render arms).
fn result_lines(result: &Option<QueryResult>, lines: &[String]) -> Vec<String> {
    match result {
        Some(QueryResult::Lines(ls)) => ls.clone(),
        Some(QueryResult::Scalar(v)) => vec![crate::query::fmt_scalar(*v)],
        Some(QueryResult::Pairs(p)) => p.iter().map(|(k, v)| format!("{k}  {v}")).collect(),
        None => lines.to_vec(),
    }
}

/// The style for a `logview` row, by the log level detected in the line. Error /
/// warn stay semantic (red / yellow — meaning wins); info / debug follow the
/// theme (accent / dim), and everything else is the theme's primary text color.
fn log_level_style(line: &str, theme: Option<crate::theme::Palette>) -> Style {
    let u = line.to_ascii_uppercase();
    let has = |p: &str| u.contains(p);
    if has("ERROR") || has("FATAL") || has("CRIT") || has("PANIC") || has("[E]") {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if has("WARN") {
        Style::default().fg(Color::Yellow)
    } else if has("DEBUG") || has("TRACE") {
        Style::default().fg(theme.map(|p| p.dim()).unwrap_or(Color::DarkGray))
    } else if has("INFO") {
        Style::default().fg(theme.map(|p| p.accent()).unwrap_or(Color::Cyan))
    } else {
        theme
            .map(|p| Style::default().fg(p.primary()))
            .unwrap_or_default()
    }
}

/// A `heatmap` cell color for a normalized value `t` in `[0, 1]`: a 5-step ramp
/// from the theme's `bg` up to its `accent` when themed, else a fixed
/// blue→cyan→green→yellow→red heat scale.
fn heat_color(t: f64, theme: Option<crate::theme::Palette>) -> Color {
    let idx = ((t.clamp(0.0, 1.0) * 4.0).round() as usize).min(4);
    let ramp: [u8; 5] = match theme {
        Some(p) => [p.c[5], p.c[4], p.c[3], p.c[2], p.c[1]], // bg,dim,mid,alt,accent
        None => [17, 39, 46, 226, 196],
    };
    Color::Indexed(ramp[idx])
}

/// A distinct palette slot per index (for multi-colored `treemap`/`gantt` rows):
/// cycles accent → alt → mid → primary → dim; the plain accent with no theme.
fn slot_by_index(i: usize, theme: Option<crate::theme::Palette>, accent: Color) -> Color {
    match theme {
        Some(p) => [p.accent(), p.alt(), p.mid(), p.primary(), p.dim()][i % 5],
        None => accent,
    }
}

/// Slice-and-dice treemap layout: assign each `(label, value)` a rectangle whose
/// area is proportional to its value, alternating the split axis to keep rects
/// from getting too thin. Returns one `Rect` per pair, in order.
fn treemap_rects(area: Rect, pairs: &[(String, u64)]) -> Vec<Rect> {
    let total: u64 = pairs.iter().map(|(_, v)| *v).sum();
    if total == 0 || pairs.is_empty() || area.width == 0 || area.height == 0 {
        return pairs.iter().map(|_| area).collect();
    }
    let mut out = Vec::with_capacity(pairs.len());
    let mut rem = area;
    let mut rem_total = total;
    for (idx, (_, v)) in pairs.iter().enumerate() {
        if idx + 1 == pairs.len() {
            out.push(rem);
            break;
        }
        let frac = *v as f64 / rem_total.max(1) as f64;
        if rem.width >= rem.height {
            let cw = ((rem.width as f64 * frac).round() as u16).clamp(1, rem.width);
            out.push(Rect { width: cw, ..rem });
            rem = Rect {
                x: rem.x + cw,
                width: rem.width - cw,
                ..rem
            };
        } else {
            let ch = ((rem.height as f64 * frac).round() as u16).clamp(1, rem.height);
            out.push(Rect { height: ch, ..rem });
            rem = Rect {
                y: rem.y + ch,
                height: rem.height - ch,
                ..rem
            };
        }
        rem_total = rem_total.saturating_sub(*v);
    }
    out
}

#[cfg(test)]
mod fzf_view_tests {
    use super::{fzf_key, fzf_row_to_cursor_rev, fzf_window_start, tac_index};
    use crate::fzf::Key;

    // The expectations below were read off fzf 0.74.2 itself: it was run under a
    // pty with 20 items, the cursor driven with ctrl-n/ctrl-p, and the top row
    // of the list recorded at each position.
    #[test]
    fn window_scrolls_where_fzf_scrolls() {
        let (h, n, so) = (6, 20, 3);
        // Walking down from the top: the window holds until the cursor comes
        // within the scroll-off margin of the bottom, then follows it.
        let mut start = 0;
        let tops: Vec<usize> = (0..8)
            .map(|sel| {
                start = fzf_window_start(start, sel, h, n, so);
                start
            })
            .collect();
        assert_eq!(tops, vec![0, 0, 0, 1, 2, 3, 4, 5]);
        // At the very end the window parks on the last full screen…
        let end = fzf_window_start(5, 19, h, n, so);
        assert_eq!(end, 14);
        // …and walking back up it stays put until the margin bites (fzf keeps
        // the cursor on the same row rather than scrolling one line early).
        assert_eq!(fzf_window_start(14, 18, h, n, so), 14);
        assert_eq!(fzf_window_start(14, 16, h, n, so), 14);
        assert_eq!(fzf_window_start(14, 15, h, n, so), 13);
    }

    #[test]
    fn window_with_a_taller_list_keeps_the_same_margin() {
        // list_h = 10: fzf held the top until the cursor hit row 6 (3 rows of
        // margin below), then scrolled one line per step.
        assert_eq!(fzf_window_start(0, 5, 10, 20, 3), 0);
        assert_eq!(fzf_window_start(0, 8, 10, 20, 3), 2);
        assert_eq!(fzf_window_start(2, 12, 10, 20, 3), 6);
        // A list that fits never scrolls.
        assert_eq!(fzf_window_start(0, 4, 10, 5, 3), 0);
    }

    #[test]
    fn tac_flips_display_positions_to_ranks() {
        assert_eq!(tac_index(0, 10, false), 0);
        assert_eq!(tac_index(0, 10, true), 9);
        assert_eq!(tac_index(9, 10, true), 0);
        // An empty list must not underflow.
        assert_eq!(tac_index(0, 0, true), 0);
    }

    #[test]
    fn bottom_up_click_mirrors_the_row() {
        // 6 visible rows starting at offset 10, list drawn bottom-up: the top
        // screen row is the LAST of the window.
        assert_eq!(fzf_row_to_cursor_rev(2, 10, 6, 2), 15);
        assert_eq!(fzf_row_to_cursor_rev(2, 10, 6, 7), 10);
    }

    #[test]
    fn inline_viewport_ends_on_the_row_the_terminal_reported() {
        use super::inline_rect;
        // The reported row is the picker's last line: 6 rows ending at row 9.
        let area = inline_rect(9, 6, 80, 24);
        assert_eq!((area.y, area.height), (4, 6));
        // Reserved room ran to the bottom of the screen.
        let area = inline_rect(23, 6, 80, 24);
        assert_eq!((area.y, area.height), (18, 6));
        // A picker taller than the terminal is capped, and never starts above 0.
        let area = inline_rect(23, 40, 80, 24);
        assert_eq!((area.y, area.height), (0, 24));
    }

    #[test]
    fn min_height_counts_list_rows_when_it_ends_in_plus() {
        use super::min_height_rows;
        // fzf's default `10+`: ten LIST rows plus the chrome around them —
        // prompt + info rule + border = 4 here, so 14 rows total.
        assert_eq!(min_height_rows(10, true, 4, 40), 14);
        // A bare number is the whole picker.
        assert_eq!(min_height_rows(10, false, 4, 40), 10);
        // Never taller than the terminal.
        assert_eq!(min_height_rows(10, true, 4, 12), 12);
    }

    #[test]
    fn cursor_report_parses_the_row() {
        use super::parse_cursor_report;
        assert_eq!(parse_cursor_report(b"\x1b[12;40R"), Some(11));
        assert_eq!(parse_cursor_report(b"\x1b[1;1R"), Some(0));
        // A keystroke can arrive before the reply; the last report wins.
        assert_eq!(parse_cursor_report(b"q\x1b[7;3R"), Some(6));
        // Nothing usable yet.
        assert_eq!(parse_cursor_report(b"\x1b[7;3"), None);
        assert_eq!(parse_cursor_report(b""), None);
    }

    #[test]
    fn keystrokes_map_to_fzf_key_names() {
        // Control bytes, escape sequences and Alt-<char>, as `--bind` names them.
        assert_eq!(fzf_key(b"\x0e", 0), Some((Key::Ctrl('n'), 1)));
        assert_eq!(fzf_key(b"\t", 0), Some((Key::Tab, 1)));
        assert_eq!(fzf_key(b"\r", 0), Some((Key::Enter, 1)));
        assert_eq!(fzf_key(b"\x1b[A", 0), Some((Key::Up, 3)));
        assert_eq!(fzf_key(b"\x1b[5~", 0), Some((Key::PageUp, 4)));
        assert_eq!(fzf_key(b"\x1b[Z", 0), Some((Key::BTab, 3)));
        assert_eq!(fzf_key(b"\x1bx", 0), Some((Key::Alt('x'), 2)));
        assert_eq!(fzf_key(b"\x1b", 0), Some((Key::Esc, 1)));
        assert_eq!(fzf_key(b"?", 0), Some((Key::Char('?'), 1)));
    }
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
            parse_tracks("20c * 2* 30%").unwrap(),
            vec![
                Track::Length(20),
                Track::Fill(1),
                Track::Fill(2),
                Track::Percentage(30)
            ]
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
    fn bare_integers_are_weights() {
        // `cols "1 2 1"` is a 1:2:1 proportional split (bare ints = weights), so
        // the middle column is 2× the outer ones. Width 80 -> 20 / 40 / 20.
        let r = rects_of(
            "gauge .a\ngauge .b\ngauge .c\ngrid .a -row 0 -col 0\ngrid .b -row 0 -col 1\ngrid .c -row 0 -col 2\ncols \"1 2 1\"",
            80,
            20,
        );
        assert_eq!((r[0].width, r[1].width, r[2].width), (20, 40, 20));
    }

    #[test]
    fn cols_fixed_length_then_fill() {
        // `cols "20c *"` → first column a fixed 20 cells, second fills the rest.
        let r = rects_of(
            "gauge .a\ngauge .b\ngrid .a -row 0 -col 0\ngrid .b -row 0 -col 1\ncols \"20c *\"",
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
        // fzf: clicking a list row sets the cursor via the scroll offset. This
        // is the top-down list (`--reverse`, and arb's own `select` widget);
        // the bottom-up default layout is asserted below.
        let mut c = Controls {
            fzf_list_start: 10,
            look: crate::fzf::Look {
                layout: crate::fzf::Layout::Reverse,
                ..crate::fzf::Look::default()
            },
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
                                  // Bottom-up (fzf's default layout): the same
                                  // click lands on the mirrored row.
        let mut c = Controls {
            fzf_list_start: 10,
            fzf_page: 10,
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
        assert_eq!(c.cursor, 17); // 10 + (10 - 1 - 2)
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
        assert_eq!(
            resolve_accent(Some("green"), th),
            super::hex_color("#00e676")
        );
        // No theme, no color -> classic cyan default (backward compatible).
        assert_eq!(
            resolve_accent(None, None),
            super::hex_color(crate::spec::color_hex(None))
        );
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
        assert_eq!(
            sp.theme.map(|p| p.accent()),
            Some(ratatui::style::Color::Indexed(2))
        );
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
    fn logview_heatmap_treemap_gantt_diff_render_without_panic() {
        // The bordered new widgets carry their kind in the title; render into a
        // real backend and none panic on plain/empty data.
        for (src, kind) in [
            ("logview .l", "logview"),
            ("heatmap .h", "heatmap"),
            ("treemap .t", "treemap"),
            ("gantt .g", "gantt"),
            ("diff .d", "diff"),
        ] {
            let text = render_text(src);
            assert!(text.contains(kind), "{kind} title missing: {text}");
        }
        // logo/clear/rule render without a titled border; just no panic + output.
        assert!(!render_text("logo .b").is_empty());
        assert!(!render_text("clear .s").is_empty());
        assert!(!render_text("rule .r").is_empty());
    }

    #[test]
    fn treemap_rects_are_proportional() {
        use super::treemap_rects;
        use ratatui::layout::Rect;
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 10,
        };
        // Two equal values -> two rects covering the area, roughly equal.
        let rects = treemap_rects(area, &[("a".into(), 1), ("b".into(), 1)]);
        assert_eq!(rects.len(), 2);
        // First split is along the long axis (width 100 >= height 10) => ~50/50.
        assert!(
            (rects[0].width as i32 - 50).abs() <= 1,
            "got {}",
            rects[0].width
        );
        // Every rect stays inside the area.
        for r in &rects {
            assert!(r.x + r.width <= area.width && r.y + r.height <= area.height);
        }
    }

    #[test]
    fn log_level_style_colors_by_level() {
        use super::log_level_style;
        use ratatui::style::Color;
        assert_eq!(
            log_level_style("2026 ERROR boom", None).fg,
            Some(Color::Red)
        );
        assert_eq!(
            log_level_style("2026 WARN slow", None).fg,
            Some(Color::Yellow)
        );
        assert_eq!(log_level_style("2026 INFO ok", None).fg, Some(Color::Cyan));
        assert_eq!(
            log_level_style("2026 DEBUG x", None).fg,
            Some(Color::DarkGray)
        );
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
        assert!(
            dates.contains(&time::Date::from_calendar_date(2026, time::Month::July, 19).unwrap())
        );
        assert!(
            dates.contains(&time::Date::from_calendar_date(2026, time::Month::July, 20).unwrap())
        );
    }

    #[test]
    fn sel_publishes_highlighted_row_as_control() {
        use super::{sel_candidates, update_sel_controls, ControlKind, ControlMeta, Controls};
        // A `sel` widget over `field 1` yields the first column of each row; the
        // cursor row is published into the `.ps.sel` control.
        let spec = build(&parse("sel .ps\nsource .ps { in; fields 1 }").unwrap()).unwrap();
        let raw = vec![
            "alice 30".to_string(),
            "bob 25".to_string(),
            "carol 40".to_string(),
        ];
        assert_eq!(
            sel_candidates(&spec.widgets[0], &raw),
            vec!["alice", "bob", "carol"]
        );

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
    /// A wheel burst chopped at arbitrary byte boundaries (a trackpad-momentum
    /// scroll outruns the reader) must decode as wheel events, never as typed
    /// text. The pre-carry reader dropped the `ESC` of a split report and typed
    /// the remainder into the fzf filter as `[<64;33;10M`.
    #[test]
    fn split_wheel_reports_scroll_instead_of_typing_themselves() {
        use super::{drain_keys, Controls};
        use std::sync::{Arc, Mutex};
        let burst: Vec<u8> = b"\x1b[<64;33;10M".repeat(6);
        // Every chunk size, not just the old 32, so no boundary is special-cased.
        for size in [1usize, 3, 5, 8, 12, 32, 64] {
            let c = Arc::new(Mutex::new(Controls {
                fzf: true,
                cursor: 10,
                ..Default::default()
            }));
            let mut pend: Vec<u8> = Vec::new();
            for chunk in burst.chunks(size) {
                pend.extend_from_slice(chunk);
                assert!(!drain_keys(&c, &mut pend, true), "chunk {size}: stopped");
            }
            let g = c.lock().unwrap();
            assert_eq!(g.filter, "", "chunk {size}: mouse bytes leaked as text");
            assert_eq!(g.cursor, 4, "chunk {size}: 6 wheel-ups not applied");
            assert!(!g.quit, "chunk {size}: a split report quit the UI");
            assert!(pend.is_empty(), "chunk {size}: bytes left uncarried");
        }
    }

    /// A longer CSI (modified arrow, F-key, `~`-key, bracketed paste) is
    /// consumed whole. The fixed 3-byte skip typed the rest into the filter:
    /// Ctrl-Up left `;5A`, F5 left `5~`.
    #[test]
    fn long_csi_sequences_are_swallowed_not_typed() {
        use super::{csi_len, drain_keys, Controls};
        use std::sync::{Arc, Mutex};
        assert_eq!(csi_len(b"\x1b[A", 0), Some(3));
        assert_eq!(csi_len(b"\x1b[1;5A", 0), Some(6));
        assert_eq!(csi_len(b"\x1b[15~", 0), Some(5));
        assert_eq!(csi_len(b"\x1b[<64;33;10M", 0), Some(12));
        assert_eq!(csi_len(b"\x1b[200~", 0), Some(6));
        assert_eq!(csi_len(b"\x1b[1;5", 0), None); // truncated

        let c = Arc::new(Mutex::new(Controls {
            fzf: true,
            cursor: 5,
            ..Default::default()
        }));
        let mut pend = b"\x1b[1;5A\x1b[15~\x1b[1;5B".to_vec();
        assert!(!drain_keys(&c, &mut pend, true));
        let g = c.lock().unwrap();
        assert_eq!(g.filter, "", "CSI tail leaked into the filter");
        assert_eq!(g.cursor, 5, "Ctrl-Up then Ctrl-Down nets zero movement");
        assert!(pend.is_empty());
    }

    /// The carry must not swallow a real Esc: with nothing following it, the
    /// second pass (`defer = false`, what the reader does after the escape-wait
    /// poll times out) decodes it as the Esc key and quits fzf mode.
    #[test]
    fn lone_esc_still_quits_after_the_escape_wait() {
        use super::{drain_keys, Controls};
        use std::sync::{Arc, Mutex};
        let c = Arc::new(Mutex::new(Controls {
            fzf: true,
            ..Default::default()
        }));
        let mut pend = b"\x1b".to_vec();
        assert!(!drain_keys(&c, &mut pend, true)); // deferred, nothing decoded
        assert_eq!(pend, b"\x1b");
        assert!(!c.lock().unwrap().quit);
        assert!(drain_keys(&c, &mut pend, false)); // wait expired: it was Esc
        assert!(c.lock().unwrap().quit);
    }

    #[test]
    fn partial_escape_spots_truncated_sequences_only() {
        use super::partial_escape;
        for t in [
            &b"\x1b"[..],
            b"\x1b[",
            b"\x1b[<",
            b"\x1b[<64",
            b"\x1b[<64;33;",
            b"\x1b[<64;33;10",
        ] {
            assert!(partial_escape(t, 0), "{t:?} should be carried");
        }
        for t in [
            &b"\x1b[<64;33;10M"[..], // complete press report
            b"\x1b[<0;1;1m",         // complete release report
            b"\x1b[A",               // complete arrow
            b"\x1b[1;5A",            // complete modified arrow
            b"\x1b[15~",             // complete F5
            b"abc",                  // plain text
            b"\x1bO",                // SS3 — not a CSI, decoded as Esc + `O`
        ] {
            assert!(!partial_escape(t, 0), "{t:?} should decode now");
        }
        // Only the tail is partial; the leading complete report decodes first.
        assert!(!partial_escape(b"\x1b[<64;33;10M\x1b[<64;3", 0));
        assert!(partial_escape(b"\x1b[<64;33;10M\x1b[<64;3", 12));
    }
}
