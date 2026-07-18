//! Interactive REPL for `arb` — utop-style line editor backed by `reedline`,
//! ported from the strykelang REPL onto arb's spec/query evaluator.
//!
//! Layout per turn:
//!
//! ```text
//! ─( HH:MM:SS )──< command N >──────────────────────────────{ arb 0.0.1 }─
//! arb❯ <buffer>
//!      avg   bars   count   each   gauge   grep   source   table   tail   …
//! ```
//!
//! arb normally reads its data stream from stdin, but the REPL owns stdin for
//! the line editor — so it keeps an in-memory *sample buffer* the user seeds
//! with dot-commands, and evaluates each typed spec against it. Each accepted
//! line is parsed to a spec and its widgets are dumped; any `source` query runs
//! against the sample buffer, exactly like the headless (`no-TTY`) render path.
//!
//! * Top "modeline" repaints with the buffer via `Prompt::render_prompt_left`.
//! * Tab pops a `ColumnarMenu` of arb verbs, query ops, `source`/`import`, and
//!   the names of the bundled + user presets.
//! * History is `~/.arb/history` via `FileBackedHistory`.
//!
//! Dot-commands manage the sample buffer:
//!   `.feed TEXT`  push a line   `.load FILE` read a file into the buffer
//!   `.buf`        show buffer   `.clear`     empty it
//!   `.presets`    list presets  `exit`/`quit`/Ctrl-D  leave

use std::borrow::Cow;
use std::process;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use nu_ansi_term::{Color as NuColor, Style};
use reedline::{
    default_emacs_keybindings, default_vi_insert_keybindings, default_vi_normal_keybindings,
    ColumnarMenu, Completer, EditMode, Emacs, FileBackedHistory, KeyCode, KeyModifiers,
    Keybindings, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch,
    PromptHistorySearchStatus, Reedline, ReedlineEvent, ReedlineMenu, Signal, Span, Suggestion, Vi,
};

use crate::query::{self, QueryResult};
use crate::{parser, spec};

const ARB_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Widget verbs, query ops, and structural keywords — the static completion
/// corpus. Kept in sync with `spec::WidgetKind::from` and `spec::build_query`.
const WIDGET_VERBS: &[&str] = &[
    "text", "tail", "list", "gauge", "bars", "histo", "spark", "chart", "table", "tabs", "block",
    "frame",
];
const QUERY_OPS: &[&str] = &[
    "in", "sel", "match", "grep", "reject", "grepv", "field", "each", "count", "rate", "tally",
    "sum", "min", "max", "avg", "keys", "vals", "calc", "where", "map", "drop", "take", "first",
    "last", "rev", "sort", "uniq", "upper", "lower", "trim",
];
const STRUCTURAL: &[&str] = &["source", "import"];
const DOT_COMMANDS: &[&str] = &[".feed", ".load", ".buf", ".clear", ".presets"];

fn arb_dir() -> std::path::PathBuf {
    let dir = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".arb"))
        .unwrap_or_else(|| std::path::PathBuf::from(".arb"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn history_path() -> std::path::PathBuf {
    arb_dir().join("history")
}

fn config_path() -> std::path::PathBuf {
    arb_dir().join("config.toml")
}

/// Contents of the auto-seeded `~/.arb/config.toml`. Every setting is commented
/// out so the file documents the schema without changing behavior.
const DEFAULT_CONFIG_TOML: &str = r#"# arb runtime config — auto-generated on first launch.
# Lines starting with `#` are comments. Uncomment + edit a line to
# override the in-code default. Delete this file and arb will
# regenerate it on the next run.

[repl]
# Edit mode for the interactive REPL. Defaults to emacs.
#
#   "emacs" — Ctrl-A/Ctrl-E/Ctrl-K/etc., readline-style (default)
#   "vi"    — modal editing; Esc → normal mode, i/a → insert, etc.
#
# Tab + Shift+Tab cycle the completion menu in either mode.
# Override per-session with `ARB_REPL_MODE=vi arb --repl`.
# mode = "emacs"
"#;

/// First-run seed: write `~/.arb/config.toml` if it does not exist. No-op when
/// the file already exists; honors `ARB_NO_CONFIG=1`.
fn ensure_default_config_seeded() {
    if std::env::var_os("ARB_NO_CONFIG").is_some() {
        return;
    }
    let path = config_path();
    if path.exists() {
        return;
    }
    let _ = std::fs::write(&path, DEFAULT_CONFIG_TOML);
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ReplMode {
    Emacs,
    Vi,
}

/// Resolve the REPL edit mode: `ARB_REPL_MODE` env → `~/.arb/config.toml`
/// `[repl] mode` → default `Emacs`.
fn resolve_repl_mode() -> ReplMode {
    if let Some(env) = std::env::var_os("ARB_REPL_MODE") {
        let s = env.to_string_lossy().to_ascii_lowercase();
        if s == "vi" || s == "vim" {
            return ReplMode::Vi;
        }
        if s == "emacs" {
            return ReplMode::Emacs;
        }
    }
    let raw = match std::fs::read_to_string(config_path()) {
        Ok(s) => s,
        Err(_) => return ReplMode::Emacs,
    };
    let parsed: toml::Value = match toml::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return ReplMode::Emacs,
    };
    let mode = parsed
        .get("repl")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("mode"))
        .and_then(|v| v.as_str())
        .unwrap_or("emacs");
    match mode.to_ascii_lowercase().as_str() {
        "vi" | "vim" => ReplMode::Vi,
        _ => ReplMode::Emacs,
    }
}

fn install_menu_bindings(keybindings: &mut Keybindings) {
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::MenuPrevious,
    );
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::BackTab,
        ReedlineEvent::MenuPrevious,
    );
}

fn build_static_completions() -> Vec<String> {
    let mut v: Vec<String> = WIDGET_VERBS
        .iter()
        .chain(QUERY_OPS.iter())
        .chain(STRUCTURAL.iter())
        .chain(DOT_COMMANDS.iter())
        .map(|s| (*s).to_string())
        .collect();
    // Preset names complete after `import ` and as `-p` targets.
    v.extend(spec::list_presets().into_iter().map(|(name, _)| name));
    v.sort();
    v.dedup();
    v
}

/// Byte index `start` and the incomplete word before cursor. Word boundaries
/// are whitespace and spec punctuation; a leading `.` is kept so both widget
/// paths (`.g`) and dot-commands (`.feed`) complete as one token.
fn completion_word_start(line: &str, pos: usize) -> (usize, &str) {
    let pos = pos.min(line.len());
    let before = line.get(..pos).unwrap_or("");
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| {
            c.is_whitespace() || matches!(*c, '(' | ')' | ',' | ';' | '{' | '}' | '|' | '<' | '-')
        })
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    (start, line.get(start..pos).unwrap_or(""))
}

struct ArbCompleter {
    words: Vec<String>,
}

impl Completer for ArbCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let (start, prefix) = completion_word_start(line, pos);
        let span = Span::new(start, pos);
        // A leading `.` on the word matches both `.feed`-style dot-commands and
        // `.g`-style widget paths; the corpus already carries the dot-commands,
        // and widget paths are user-defined so only the literal prefix filters.
        let mut out: Vec<Suggestion> = Vec::new();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for w in &self.words {
            if !w.starts_with(prefix) || !seen.insert(w.as_str()) {
                continue;
            }
            out.push(Suggestion {
                value: w.clone(),
                description: None,
                style: None,
                extra: None,
                span,
                append_whitespace: false,
                display_override: None,
                match_indices: None,
            });
        }
        out.sort_by(|a, b| a.value.cmp(&b.value));
        out
    }
}

struct ArbPrompt {
    cmd_count: Arc<Mutex<u64>>,
}

fn now_hms() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if ok {
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    } else {
        let s = (secs as u64) % 86_400;
        format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    }
}

fn term_cols() -> usize {
    use std::os::unix::io::AsRawFd;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = std::io::stdout().as_raw_fd();
    let cols = if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
        ws.ws_col as usize
    } else {
        std::env::var("COLUMNS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(80)
    };
    cols.max(40)
}

fn render_status_bar(cmd_count: u64) -> String {
    let cols = term_cols();
    let dim = NuColor::DarkGray;
    let accent = NuColor::Cyan;
    let label = NuColor::LightYellow;

    let left = format!(" {} ", now_hms());
    let mid = format!(" command {} ", cmd_count);
    let right = format!(" arb {} ", ARB_VERSION);

    let frame_chars = "─()──<>{}─".chars().count();
    let visible = left.chars().count() + mid.chars().count() + right.chars().count() + frame_chars;
    let dashes = cols.saturating_sub(visible);
    if dashes < 2 {
        return format!(
            "{lp}{l}{rp}{ml}{m}{mr}",
            lp = Style::new().fg(dim).paint("─("),
            l = Style::new().fg(accent).paint(left),
            rp = Style::new().fg(dim).paint(")"),
            ml = Style::new().fg(dim).paint("──<"),
            m = Style::new().fg(label).bold().paint(mid),
            mr = Style::new().fg(dim).paint(">"),
        );
    }
    let left_dash = dashes / 2;
    let right_dash = dashes - left_dash;
    let bar_l = "─".repeat(left_dash);
    let bar_r = "─".repeat(right_dash);

    format!(
        "{lp}{l}{rp}{ml}{m}{mr}{bar}{rl}{r}{rr}",
        lp = Style::new().fg(dim).paint("─("),
        l = Style::new().fg(accent).paint(left),
        rp = Style::new().fg(dim).paint(")"),
        ml = Style::new().fg(dim).paint("──<"),
        m = Style::new().fg(label).bold().paint(mid),
        mr = Style::new().fg(dim).paint(">"),
        bar = Style::new().fg(dim).paint(format!("{}{}", bar_l, bar_r)),
        rl = Style::new().fg(dim).paint("{"),
        r = Style::new().fg(NuColor::Magenta).paint(right),
        rr = Style::new().fg(dim).paint("}─"),
    )
}

impl Prompt for ArbPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        let count = self.cmd_count.lock().map(|g| *g).unwrap_or(0);
        let bar = render_status_bar(count);
        let prompt = Style::new()
            .fg(NuColor::Cyan)
            .bold()
            .paint("arb")
            .to_string();
        Cow::Owned(format!("{}\n{}", bar, prompt))
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Owned(
            Style::new()
                .fg(NuColor::LightCyan)
                .bold()
                .paint("❯ ")
                .to_string(),
        )
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Owned(
            Style::new()
                .fg(NuColor::DarkGray)
                .paint("····❯ ")
                .to_string(),
        )
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let prefix = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!(
            "({}reverse-search: {}) ",
            prefix, history_search.term
        ))
    }
}

/// Run the REPL until `exit` / `quit` / Ctrl-D.
pub fn run() {
    ensure_default_config_seeded();

    crate::banner::print_banner();
    println!(
        "\x1b[2m  type a spec (e.g. `gauge .g; source .g {{ in; count }}`) — .feed to \
         seed the sample stream, `exit` or Ctrl-D to leave, Tab to complete\x1b[0m"
    );
    println!();

    // The in-REPL sample stream a typed spec's `source` queries evaluate against.
    let buffer: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let start = Instant::now();
    let cmd_count = Arc::new(Mutex::new(0u64));

    let completer = ArbCompleter {
        words: build_static_completions(),
    };

    let menu = ColumnarMenu::default()
        .with_name("completion_menu")
        .with_columns(4)
        .with_column_padding(2);

    let edit_mode: Box<dyn EditMode> = match resolve_repl_mode() {
        ReplMode::Emacs => {
            let mut kb = default_emacs_keybindings();
            install_menu_bindings(&mut kb);
            Box::new(Emacs::new(kb))
        }
        ReplMode::Vi => {
            let mut insert_kb = default_vi_insert_keybindings();
            install_menu_bindings(&mut insert_kb);
            let normal_kb = default_vi_normal_keybindings();
            Box::new(Vi::new(insert_kb, normal_kb))
        }
    };

    let history = match FileBackedHistory::with_file(5_000, history_path()) {
        Ok(h) => Box::new(h) as Box<dyn reedline::History>,
        Err(e) => {
            eprintln!("repl: history unavailable: {}", e);
            Box::new(FileBackedHistory::new(5_000).unwrap_or_else(|_| {
                eprintln!("repl: cannot create in-memory history");
                process::exit(1);
            })) as Box<dyn reedline::History>
        }
    };

    let mut line_editor = Reedline::create()
        .with_completer(Box::new(completer))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_edit_mode(edit_mode)
        .with_history(history);

    let prompt = ArbPrompt {
        cmd_count: Arc::clone(&cmd_count),
    };

    loop {
        let sig = match line_editor.read_line(&prompt) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("repl: {}", e);
                break;
            }
        };

        match sig {
            Signal::Success(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if matches!(trimmed, "exit" | "quit") {
                    break;
                }
                if trimmed.starts_with('.') && handle_dot_command(trimmed, &buffer) {
                    continue;
                }
                if let Ok(mut g) = cmd_count.lock() {
                    *g += 1;
                }
                eval_spec(trimmed, &buffer, start);
            }
            Signal::CtrlC => continue,
            Signal::CtrlD => break,
            _ => break,
        }
    }
}

/// Handle a `.`-prefixed meta-command. Returns `true` if the line was a
/// recognized dot-command (and should not be evaluated as a spec). A leading
/// `.` that is not a known command (e.g. a widget path `.g` typed alone) falls
/// through to spec evaluation.
fn handle_dot_command(line: &str, buffer: &Arc<Mutex<Vec<String>>>) -> bool {
    let mut parts = line.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();
    match cmd {
        ".feed" => {
            if rest.is_empty() {
                eprintln!("arb: .feed needs text — `.feed 200 GET /`");
            } else if let Ok(mut b) = buffer.lock() {
                b.push(rest.to_string());
                println!("\x1b[2m  fed 1 line — {} in buffer\x1b[0m", b.len());
            }
            true
        }
        ".load" => {
            if rest.is_empty() {
                eprintln!("arb: .load needs a file path");
            } else {
                match std::fs::read_to_string(rest) {
                    Ok(text) => {
                        if let Ok(mut b) = buffer.lock() {
                            let n = text.lines().count();
                            b.extend(text.lines().map(str::to_string));
                            println!("\x1b[2m  loaded {n} line(s) — {} in buffer\x1b[0m", b.len());
                        }
                    }
                    Err(e) => eprintln!("arb: .load {rest}: {e}"),
                }
            }
            true
        }
        ".buf" => {
            if let Ok(b) = buffer.lock() {
                println!("\x1b[2m  sample buffer — {} line(s):\x1b[0m", b.len());
                for (i, l) in b.iter().enumerate().take(20) {
                    println!("  {:>4}  {}", i + 1, l);
                }
                if b.len() > 20 {
                    println!("\x1b[2m  … {} more\x1b[0m", b.len() - 20);
                }
            }
            true
        }
        ".clear" => {
            if let Ok(mut b) = buffer.lock() {
                b.clear();
            }
            println!("\x1b[2m  buffer cleared\x1b[0m");
            true
        }
        ".presets" => {
            for (name, desc) in spec::list_presets() {
                println!("  {name:<10} {desc}");
            }
            true
        }
        _ => false,
    }
}

/// Parse a typed line as an arb spec, build it, and dump its widgets — running
/// each `source` query against the sample buffer (same shape as the headless
/// no-TTY render path in `main::dump`).
fn eval_spec(line: &str, buffer: &Arc<Mutex<Vec<String>>>, start: Instant) {
    let cmds = match parser::parse(line) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}", NuColor::Red.paint(format!("arb: {e}")));
            return;
        }
    };
    let built = match spec::build(&cmds) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", NuColor::Red.paint(format!("arb: {e}")));
            return;
        }
    };

    let raw: Vec<String> = buffer.lock().map(|b| b.clone()).unwrap_or_default();
    let elapsed = start.elapsed().as_secs_f64();

    if built.widgets.is_empty() {
        println!("\x1b[2m  (no widgets)\x1b[0m");
        return;
    }
    for w in &built.widgets {
        let arrow = Style::new().fg(NuColor::DarkGray).paint("→");
        match &w.source {
            Some(s) => {
                let res = match query::eval(&s.pipeline, &raw, elapsed) {
                    QueryResult::Scalar(v) => format!("= {v}"),
                    QueryResult::Lines(ls) => format!("{} line(s)", ls.len()),
                    QueryResult::Pairs(p) => format!("{} group(s)", p.len()),
                };
                println!(
                    "  {} {} {arrow} source[{} op] {}",
                    Style::new().fg(NuColor::Cyan).bold().paint(&w.path),
                    Style::new().fg(NuColor::LightYellow).paint(w.kind.label()),
                    s.pipeline.len(),
                    Style::new().fg(NuColor::Green).paint(res),
                );
            }
            None => {
                println!(
                    "  {} {} {arrow} {}",
                    Style::new().fg(NuColor::Cyan).bold().paint(&w.path),
                    Style::new().fg(NuColor::LightYellow).paint(w.kind.label()),
                    Style::new().fg(NuColor::DarkGray).paint("(no source)"),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_word_at_cursor() {
        let s = "source .g { co";
        let (st, pre) = completion_word_start(s, s.len());
        assert_eq!(pre, "co");
        assert_eq!(&s[st..], "co");
    }

    #[test]
    fn static_completions_include_verbs_ops_and_dots() {
        let v = build_static_completions();
        assert!(v.iter().any(|w| w == "gauge"));
        assert!(v.iter().any(|w| w == "count"));
        assert!(v.iter().any(|w| w == "source"));
        assert!(v.iter().any(|w| w == ".feed"));
    }

    #[test]
    fn dot_feed_pushes_to_buffer() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        assert!(handle_dot_command(".feed hello world", &buf));
        assert_eq!(buf.lock().unwrap().as_slice(), &["hello world".to_string()]);
    }

    #[test]
    fn unknown_dot_is_not_a_command() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        // `.g` (a widget path typed alone) is not a dot-command — falls through.
        assert!(!handle_dot_command(".g", &buf));
    }
}
