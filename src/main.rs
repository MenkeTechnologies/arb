//! arb — visualize and modify Unix pipelines.
//!
//! Pipe a stream in and arb renders a dynamic TUI built from a declarative spec.
//! With no spec it synthesizes a full-screen `tail` of stdin. With no controlling
//! terminal on stdout (piped onward / CI) it prints the parsed spec, each source
//! pipeline, and its evaluated result instead of a TUI — correct behavior and the
//! headless-test path.
//!
//! M1/M2a interpret the declarative subset (widgets, `source` query pipelines,
//! binds). The expression layer, fusevm lowering, web target, actors, and package
//! manager are later milestones (see SPEC.md) and are not faked here.

use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use clap::Parser;

use arb::query::{self, QueryOp, QueryResult};
use arb::spec::{self, Spec};
use arb::stream::StreamState;
use arb::{parser, tui};

// Cyberpunk `-h` help — help template, ASCII banner, and footer, ported from the
// `temprs` (`tp -h`) house style (`temprs/src/model/opts.rs`): `{before-help}`
// banner, cyberpunk section dividers, green `//` per-option prefixes, and a
// `── SYSTEM ──` footer. clap emits raw ANSI in these verbatim.
const HELP_TEMPLATE: &str = "
{before-help}
{about}

\x1b[33m  USAGE:\x1b[0m {usage}

\x1b[36m  ── OPTIONS ────────────────────────────────────────────\x1b[0m
{options}
\x1b[36m  ── POSITIONAL ─────────────────────────────────────────\x1b[0m
{positionals}
{after-help}";

const BANNER: &str = concat!(
    "\x1b[36m █████╗ ██████╗ ██████╗\x1b[0m\n",
    "\x1b[36m██╔══██╗██╔══██╗██╔══██╗\x1b[0m\n",
    "\x1b[35m███████║██████╔╝██████╔╝\x1b[0m\n",
    "\x1b[35m██╔══██║██╔══██╗██╔══██╗\x1b[0m\n",
    "\x1b[31m██║  ██║██║  ██║██████╔╝\x1b[0m\n",
    "\x1b[31m╚═╝  ╚═╝╚═╝  ╚═╝╚═════╝\x1b[0m\n",
    "\x1b[36m ┌──────────────────────────────────────────────────────┐\x1b[0m\n",
    "\x1b[36m │ STATUS: ONLINE  // SIGNAL: ████████░░ // v",
    env!("CARGO_PKG_VERSION"),
    "\x1b[36m      │\x1b[0m\n",
    "\x1b[36m └──────────────────────────────────────────────────────┘\x1b[0m\n",
    "\x1b[35m  >> A TUI FOR EVERY PIPELINE // FULL SPECTRUM <<\x1b[0m"
);

const AFTER: &str = concat!(
    "\x1b[36m  ── SYSTEM ─────────────────────────────────────────\x1b[0m\n",
    "\x1b[35m  v",
    env!("CARGO_PKG_VERSION"),
    " \x1b[0m// \x1b[33m(c) MenkeTechnologies\x1b[0m\n",
    "\x1b[35m  A TUI for every pipeline.\x1b[0m\n",
    "\x1b[33m  >>> PIPE IN. SHAPE THE STREAM. OWN YOUR OUTPUT. <<<\x1b[0m\n",
    "\x1b[36m ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░\x1b[0m"
);

/// arb — pipe a stream in, get a live TUI.
#[derive(Parser, Debug)]
#[command(
    name = "arb",
    version,
    about = "Visualize and modify Unix pipelines.",
    help_template = HELP_TEMPLATE,
    before_help = BANNER,
    after_help = AFTER,
)]
struct Cli {
    /// Dashboard spec file (.arb).
    #[arg(help = "\x1b[32m//\x1b[0m Dashboard spec file (.arb)")]
    spec: Option<String>,
    /// Inline spec, e.g. `-e 'gauge .g -max 100; source .g { in; count }'`.
    #[arg(short = 'e', long = "eval",
        help = "\x1b[32m//\x1b[0m Inline spec, e.g. -e 'gauge .g -max 100; source .g { in; count }'")]
    eval: Option<String>,
    /// Run a preset / stdlib module by name, e.g. `-p logs` (== `import logs`).
    #[arg(short = 'p', long = "preset",
        help = "\x1b[32m//\x1b[0m Run a preset / stdlib module by name (== import NAME)")]
    preset: Option<String>,
    /// List available presets (bundled stdlib + `~/.arb/lib`) and exit.
    #[arg(short = 'l', long = "list",
        help = "\x1b[32m//\x1b[0m List available presets (stdlib + ~/.arb/lib) and exit")]
    list: bool,
    /// Save a spec as a named user preset in `~/.arb/lib`, then exit.
    /// Source is the `FILE` argument or `-e SRC`. E.g. `arb --save api dash.arb`.
    #[arg(long = "save", value_name = "NAME",
        help = "\x1b[32m//\x1b[0m Save a spec (FILE or -e SRC) as a named user preset, then exit")]
    save: Option<String>,
    /// Interactive REPL — author + test specs against a sample buffer.
    #[arg(short = 'r', long = "repl",
        help = "\x1b[32m//\x1b[0m Interactive REPL — author + test specs against a sample buffer")]
    repl: bool,
    /// Generate a static HTML dashboard from the spec to stdout, then exit
    /// (`arb -p logs --html > dash.html`).
    #[arg(long = "html",
        help = "\x1b[32m//\x1b[0m Emit a static HTML dashboard from the spec to stdout, then exit")]
    html: bool,
    /// Validate the spec (parse + build) and exit 0/1 without reading stdin.
    #[arg(long = "check",
        help = "\x1b[32m//\x1b[0m Validate the spec (parse + build) and exit 0/1, no stdin")]
    check: bool,
    /// With an `out { … }` pipeline, emit results as JSON (array / number /
    /// object) instead of plain lines — pipe to `jq` or programs.
    #[arg(long = "json",
        help = "\x1b[32m//\x1b[0m With an out { } pipeline, emit JSON instead of plain lines")]
    json: bool,
    /// Downstream command after `--`: arb spawns it, feeds it the filtered
    /// stream, and hooks its stdout+stderr to a temp file it tails in a pane —
    /// so the program's output shows inside arb instead of corrupting the TUI.
    /// E.g. `find / | arb -- grep error`.
    #[arg(last = true,
        help = "\x1b[32m//\x1b[0m Downstream `-- CMD …`: run it, capture its output into a pane")]
    down: Vec<String>,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    if cli.repl {
        arb::repl::run();
        return Ok(());
    }

    if cli.list {
        let mut out = io::stdout().lock();
        for (name, desc) in spec::list_presets() {
            writeln!(out, "{name:<10} {desc}")?;
        }
        return Ok(());
    }

    if let Some(name) = cli.save.clone() {
        return save_preset(&name, &cli);
    }

    // Bare `arb` on an interactive terminal — no spec/`-e`/`-p` and nothing
    // piped in — drops into the REPL rather than erroring on the stdin-tail
    // default (which needs a pipe). A piped `find / | arb` still tails.
    let no_spec_args = cli.spec.is_none() && cli.eval.is_none() && cli.preset.is_none();
    if no_spec_args && io::stdin().is_terminal() {
        arb::repl::run();
        return Ok(());
    }

    let spec = match load_spec(&cli) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("arb: {e}");
            std::process::exit(1);
        }
    };

    if cli.html {
        print!("{}", arb::web::render_html(&spec));
        return Ok(());
    }

    if cli.check {
        println!(
            "arb: ok \u{2014} {} widget(s){}",
            spec.widgets.len(),
            if spec.out.is_some() {
                ", out pipeline"
            } else {
                ""
            }
        );
        return Ok(());
    }

    let needs_stdin = spec.out.is_some() || spec.widgets.iter().any(|w| w.source.is_some());
    if needs_stdin && io::stdin().is_terminal() {
        eprintln!("arb: spec reads stdin but nothing is piped — e.g. `find / | arb`");
        std::process::exit(2);
    }

    let state = Arc::new(Mutex::new(StreamState::new()));

    // Interactive TUI whenever a controlling terminal is reachable (a `/dev/tty`
    // we can open — see `tui::events_available`); the TUI renders THERE, not to
    // stdout, so it runs even when stdout is piped onward. Exception: an explicit
    // `out { … }` reshape with a downstream consumer takes the data path below so
    // the consumer gets the transformed stream. With no controlling tty (CI, a
    // detached exec) it also falls through, instead of crashing on the reader.
    let downstream_reshape = spec.out.is_some() && !io::stdout().is_terminal();
    if tui::events_available() && !downstream_reshape {
        let controls = Arc::new(Mutex::new(tui::Controls::default()));

        // Optional `arb -- CMD`: spawn CMD with its stdout+stderr hooked to a temp
        // file, tail that file into a display pane, and feed CMD the filtered
        // stream on its stdin. Its output shows INSIDE arb instead of printing to
        // the terminal (which would corrupt the TUI).
        let (child_stdin, down_pane) = if cli.down.is_empty() {
            (None, None)
        } else {
            match spawn_downstream(&cli.down) {
                Ok((stdin, dstate)) => (Some(stdin), Some((dstate, cli.down.join(" ")))),
                Err(e) => {
                    eprintln!("arb: {}: {e}", cli.down[0]);
                    std::process::exit(1);
                }
            }
        };

        if needs_stdin {
            // Feed the live filtered stream to stdout (only when piped onward, so
            // arb never blocks or corrupts the terminal) AND to the downstream
            // child's stdin. The filter narrows both in real time (the megafilter).
            spawn_reader(
                state.clone(),
                !io::stdout().is_terminal(),
                controls.clone(),
                child_stdin,
            );
        }
        tui::run(&spec, state, controls, down_pane)
    } else if let Some(out_ops) = &spec.out {
        // Piped downstream: arb modifies the stream. A per-line pipeline streams
        // (so `tail -f | arb 'out {…}' | …` works live); reducers/sorts batch.
        if needs_stdin && !cli.json && query::is_line_streamable(out_ops) {
            stream_out(out_ops)
        } else {
            if needs_stdin {
                read_stdin_sync(&state);
            }
            emit_out(out_ops, &state, cli.json)
        }
    } else if needs_stdin {
        // A dashboard spec piped onward with no `out { … }` reshape — arb is a
        // passive tap: forward the stream through untouched so the downstream
        // consumer still receives it (`find / | arb dash.arb | stryke`). Only an
        // explicit `out { … }` changes what flows downstream.
        passthrough()
    } else {
        dump(&spec, &state)
    }
}

/// arb as a transparent tap: copy every stdin line to stdout unchanged, flushing
/// per line so a live upstream (`tail -f`) reaches the downstream consumer
/// promptly. Used when a dashboard spec is piped onward with no `out { … }`.
fn passthrough() -> io::Result<()> {
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    for line in stdin.lock().lines() {
        let l = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if let Err(e) = writeln!(out, "{l}").and_then(|()| out.flush()) {
            return ok_on_broken_pipe(Err(e));
        }
    }
    Ok(())
}

/// Treat a closed downstream pipe (the consumer exited — `| head`, `| stryke`
/// quitting) as clean EOF rather than an error, like any well-behaved Unix
/// filter. Other I/O errors propagate.
fn ok_on_broken_pipe(r: io::Result<()>) -> io::Result<()> {
    match r {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

/// Write the `out { … }` pipeline's result to stdout (arb as a pipe filter).
/// Stream a per-line `out` pipeline: apply it to each line as it arrives and
/// emit results immediately with a flush, so live pipes work without buffering.
fn stream_out(ops: &[QueryOp]) -> io::Result<()> {
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if let QueryResult::Lines(ls) = query::eval(ops, std::slice::from_ref(&line), 0.0) {
            for l in ls {
                if let Err(e) = writeln!(out, "{l}") {
                    return ok_on_broken_pipe(Err(e));
                }
            }
        }
        if let Err(e) = out.flush() {
            return ok_on_broken_pipe(Err(e));
        }
    }
    Ok(())
}

fn emit_out(ops: &[QueryOp], state: &Arc<Mutex<StreamState>>, json: bool) -> io::Result<()> {
    let st = state.lock().unwrap();
    let raw: Vec<String> = st.lines.iter().cloned().collect();
    let elapsed = st.start.elapsed().as_secs_f64();
    let result = query::eval(ops, &raw, elapsed);
    let mut out = io::stdout().lock();
    if json {
        let v = match result {
            QueryResult::Lines(ls) => {
                serde_json::Value::Array(ls.into_iter().map(serde_json::Value::String).collect())
            }
            QueryResult::Scalar(s) => serde_json::Number::from_f64(s)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            QueryResult::Pairs(ps) => {
                let mut m = serde_json::Map::new();
                for (k, c) in ps {
                    m.insert(k, serde_json::Value::Number(c.into()));
                }
                serde_json::Value::Object(m)
            }
        };
        writeln!(out, "{}", serde_json::to_string(&v).unwrap_or_default())?;
        return Ok(());
    }
    match result {
        QueryResult::Lines(ls) => {
            for l in ls {
                writeln!(out, "{l}")?;
            }
        }
        QueryResult::Scalar(v) => writeln!(out, "{v}")?,
        QueryResult::Pairs(ps) => {
            for (k, c) in ps {
                writeln!(out, "{k}\t{c}")?;
            }
        }
    }
    Ok(())
}

/// Validate a spec and copy it into `~/.arb/lib/NAME.arb` so it can later be run
/// with `arb -p NAME` from anywhere.
fn save_preset(name: &str, cli: &Cli) -> io::Result<()> {
    let src = if let Some(e) = &cli.eval {
        e.clone()
    } else if let Some(f) = &cli.spec {
        std::fs::read_to_string(f)?
    } else {
        eprintln!("arb: --save needs a spec — a FILE argument or -e SRC");
        std::process::exit(2);
    };
    if let Err(e) = parser::parse(&src).and_then(|c| spec::build(&c)) {
        eprintln!("arb: --save: invalid spec: {e}");
        std::process::exit(1);
    }
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME not set"))?;
    let dir = home.join(".arb/lib");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.arb"));
    std::fs::write(&path, src)?;
    eprintln!("arb: saved preset `{name}` -> {}", path.display());
    Ok(())
}

fn load_spec(cli: &Cli) -> Result<Spec, String> {
    let src = if let Some(p) = &cli.preset {
        format!("import {p}")
    } else if let Some(e) = &cli.eval {
        e.clone()
    } else if let Some(path) = &cli.spec {
        std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?
    } else {
        // Zero-config default: a full-screen tail of stdin.
        "tail .stream\nsource .stream { in }".to_string()
    };
    spec::build(&parser::parse(&src)?)
}

/// Read stdin into the shared stream for the TUI. When `tee`, each line is also
/// written to stdout immediately (a live passthrough for a downstream consumer),
/// so `find / | arb | consumer` feeds `consumer` continuously while the TUI
/// renders to /dev/tty — arb never blocks the pipeline. A closed consumer stops
/// the passthrough but the TUI keeps updating.
fn spawn_reader(
    state: Arc<Mutex<StreamState>>,
    tee: bool,
    controls: Arc<Mutex<tui::Controls>>,
    mut child_stdin: Option<std::process::ChildStdin>,
) {
    thread::spawn(move || {
        let mut out = io::stdout().lock();
        let mut downstream_open = tee;
        for line in io::stdin().lock().lines() {
            let l = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            // Megafilter: only lines matching the live filter flow onward, so what
            // the user types reshapes both stdout and the child's input live.
            let pass = tui::filter_matches(&l, &controls.lock().unwrap().filter);
            if pass {
                if downstream_open && writeln!(out, "{l}").and_then(|()| out.flush()).is_err() {
                    downstream_open = false;
                }
                if let Some(ci) = child_stdin.as_mut() {
                    let _ = writeln!(ci, "{l}");
                }
            }
            state.lock().unwrap().push(l);
        }
        // Close the child's stdin so it sees EOF and can finish.
        drop(child_stdin);
    });
}

/// Spawn `arb -- CMD`: run CMD with its stdout+stderr redirected to a temp file
/// (never the terminal), start a tailer that streams that file into a fresh
/// `StreamState` for the display pane, and hand back CMD's stdin so the reader
/// can feed it the filtered stream. The temp file is left in place on exit.
fn spawn_downstream(
    cmd: &[String],
) -> io::Result<(std::process::ChildStdin, Arc<Mutex<StreamState>>)> {
    let tmp = std::env::temp_dir().join(format!("arb-{}.down", std::process::id()));
    let out = File::create(&tmp)?;
    let err = out.try_clone()?;
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("no child stdin"))?;
    // Detach: the child runs on its own; dropping the handle does not kill it.
    drop(child);
    let dstate = Arc::new(Mutex::new(StreamState::new()));
    spawn_tailer(tmp, dstate.clone());
    Ok((stdin, dstate))
}

/// Tail a growing file (the downstream child's captured output) into `dstate`,
/// like `tail -f`: read new lines as they appear, sleeping at EOF.
fn spawn_tailer(path: PathBuf, dstate: Arc<Mutex<StreamState>>) {
    thread::spawn(move || {
        let file = loop {
            match File::open(&path) {
                Ok(f) => break f,
                Err(_) => thread::sleep(Duration::from_millis(50)),
            }
        };
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => thread::sleep(Duration::from_millis(100)),
                Ok(_) => dstate.lock().unwrap().push(line.trim_end().to_string()),
                Err(_) => break,
            }
        }
    });
}

fn read_stdin_sync(state: &Arc<Mutex<StreamState>>) {
    for line in io::stdin().lock().lines() {
        match line {
            Ok(l) => state.lock().unwrap().push(l),
            Err(_) => break,
        }
    }
}

fn dump(spec: &Spec, state: &Arc<Mutex<StreamState>>) -> io::Result<()> {
    let st = state.lock().unwrap();
    let raw: Vec<String> = st.lines.iter().cloned().collect();
    let elapsed = st.start.elapsed().as_secs_f64();
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "arb: spec — {} widget(s) (no terminal; render skipped)",
        spec.widgets.len()
    )?;
    for w in &spec.widgets {
        let (src, res) = match &w.source {
            Some(s) => {
                let r = match query::eval(&s.pipeline, &raw, elapsed) {
                    QueryResult::Scalar(v) => format!("= {v}"),
                    QueryResult::Lines(ls) => format!("-> {} line(s)", ls.len()),
                    QueryResult::Pairs(p) => format!("-> {} group(s)", p.len()),
                };
                (format!("stdin[{} op]", s.pipeline.len()), r)
            }
            None => ("-".to_string(), String::new()),
        };
        writeln!(
            out,
            "  {:<10} {:<6} source={:<12} {:<16} opts={:?}",
            w.path,
            w.kind.label(),
            src,
            res,
            w.opts
        )?;
    }
    writeln!(out, "stream: {} lines", st.total)?;
    Ok(())
}
