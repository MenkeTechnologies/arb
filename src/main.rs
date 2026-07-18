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

use std::io::{self, BufRead, BufReader, IsTerminal, Write};
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
    /// fzf select mode: interactively filter the stream and pick one line;
    /// the chosen line is printed to stdout on Enter (`vim $(ls | arb --fzf)`).
    #[arg(long = "fzf",
        help = "\x1b[32m//\x1b[0m fzf mode: filter + select one line, printed to stdout on Enter")]
    fzf: bool,
    /// Run a pipeline with arb as the interactive stage, e.g.
    /// `arb --fzf --run 'sudo find / | _ | grep foo'`. arb spawns the
    /// surrounding commands (each via `sh -c`, so globs/quotes work) and controls
    /// their fds: the producer's stdout feeds arb's stream, its stderr goes to a
    /// pane instead of corrupting the TUI.
    #[arg(long = "run", value_name = "PIPELINE",
        help = "\x1b[32m//\x1b[0m Run 'PROD | _ | CONS': arb spawns them, stderr -> pane")]
    run: Option<String>,
    /// fzf preview: run this command for the line under the cursor (`{}` is the
    /// current line, shell-escaped) and show its output in a right pane, updated
    /// as you move. E.g. `arb --fzf --preview 'bat --color=always {}'`.
    #[arg(long = "preview", value_name = "CMD",
        help = "\x1b[32m//\x1b[0m fzf preview: run CMD on the cursor line ({}), output in a pane")]
    preview: Option<String>,
    /// fzf prompt string (default `> `).
    #[arg(long = "prompt", value_name = "STR",
        help = "\x1b[32m//\x1b[0m fzf prompt string (default '> ')")]
    prompt: Option<String>,
    /// fzf header line shown above the list.
    #[arg(long = "header", value_name = "STR",
        help = "\x1b[32m//\x1b[0m fzf header line shown above the list")]
    header: Option<String>,
    /// fzf height: render inline in N rows (or `N%` of the terminal) at the
    /// bottom instead of full-screen, keeping the scrollback. E.g. `--height 40%`.
    #[arg(long = "height", value_name = "N|N%",
        help = "\x1b[32m//\x1b[0m fzf height: inline in N rows or N% (not full-screen)")]
    height: Option<String>,
    /// Preview command after `--`: re-run over arb's current post-filter output
    /// whenever the filter changes; its stdout+stderr show in a pane, always in
    /// sync with the filter and never touching the terminal.
    /// E.g. `find / | arb -- grep error`.
    #[arg(last = true,
        help = "\x1b[32m//\x1b[0m Preview `-- CMD …`: re-run over the filtered output, shown in a pane")]
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
    // A run-pipeline: `--run`, or a positional containing the `_` arb-stage marker
    // (`arb --fzf 'find / | _ | grep x'`). When the positional IS the pipeline,
    // arb runs it rather than loading it as a spec file.
    let run_pipeline: Option<String> = cli
        .run
        .clone()
        .or_else(|| cli.spec.as_ref().filter(|s| is_arb_pipeline(s)).cloned());
    let positional_pipeline = cli.run.is_none() && run_pipeline.is_some();

    let no_spec_args = cli.spec.is_none() && cli.eval.is_none() && cli.preset.is_none();
    if no_spec_args && run_pipeline.is_none() && io::stdin().is_terminal() {
        arb::repl::run();
        return Ok(());
    }

    let spec = if positional_pipeline {
        // The positional was a pipeline, not a spec file — use the zero-config
        // default (a select list under `--fzf`, otherwise a stream tail).
        spec::build(&parser::parse(default_spec_src(cli.fzf)).unwrap()).unwrap()
    } else {
        match load_spec(&cli) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("arb: {e}");
                std::process::exit(1);
            }
        }
    };

    // Select mode is the fzf surface expressed as a widget: `--fzf` synthesizes a
    // `select` spec, and a hand-written `select .name` widget turns it on too — so
    // fzf mode is literally a one-widget DSL spec. The select widget's `-prompt`/
    // `-header` opts feed the prompt line and header when the flags weren't passed.
    let fzf_mode = cli.fzf || spec.widgets.iter().any(|w| w.kind == spec::WidgetKind::Select);
    let (sel_prompt, sel_header) = spec
        .widgets
        .iter()
        .find(|w| w.kind == spec::WidgetKind::Select)
        .map(|w| (w.opts.get("prompt").cloned(), w.opts.get("header").cloned()))
        .unwrap_or((None, None));

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

    // `--run 'PROD | arb | CONS'`: arb spawns the surrounding commands and owns
    // their fds. The producer's stdout feeds the stream, its stderr an error pane.
    let (producer, consumer) = match &run_pipeline {
        Some(p) => {
            let (pr, co) = parse_pipeline(p);
            (Some(pr), co)
        }
        None => (None, None),
    };

    let needs_stdin =
        fzf_mode || spec.out.is_some() || spec.widgets.iter().any(|w| w.source.is_some());
    if needs_stdin && run_pipeline.is_none() && io::stdin().is_terminal() {
        eprintln!("arb: spec reads stdin but nothing is piped — e.g. `find / | arb`");
        std::process::exit(2);
    }

    // fzf select mode keeps every line (no ring drop), so marks persist and the
    // cursor stays put as the stream grows; the dashboard uses the bounded ring.
    let state = Arc::new(Mutex::new(if fzf_mode {
        StreamState::with_cap(usize::MAX)
    } else {
        StreamState::new()
    }));

    // Interactive TUI whenever a controlling terminal is reachable (a `/dev/tty`
    // we can open — see `tui::events_available`); the TUI renders THERE, not to
    // stdout, so it runs even when stdout is piped onward. Exception: an explicit
    // `out { … }` reshape with a downstream consumer takes the data path below so
    // the consumer gets the transformed stream. With no controlling tty (CI, a
    // detached exec) it also falls through, instead of crashing on the reader.
    let downstream_reshape = spec.out.is_some() && !io::stdout().is_terminal();
    if tui::events_available() && !downstream_reshape {
        let controls = Arc::new(Mutex::new(tui::Controls::default()));

        // Feed the stream: from a spawned producer (`--run`) or from stdin.
        let err_pane = if let Some(prod) = &producer {
            match spawn_producer(prod, state.clone()) {
                Ok(es) => Some((es, "stderr".to_string())),
                Err(e) => {
                    eprintln!("arb: run: {e}");
                    std::process::exit(1);
                }
            }
        } else {
            if needs_stdin {
                // Tee the live filtered stream to stdout (only when piped onward,
                // so arb never blocks or corrupts the terminal). fzf mode never
                // tees. The filter narrows the passthrough live — the megafilter.
                let tee = !fzf_mode && !io::stdout().is_terminal();
                spawn_reader(state.clone(), tee, controls.clone());
            }
            None
        };

        // Preview pane. In fzf mode: a per-item `--preview` (run CMD on the cursor
        // line). Otherwise: `arb -- CMD` / the `| CONS` consumer, re-run over the
        // whole filtered output.
        let down_cmd: Vec<String> = match &consumer {
            Some(c) => vec!["sh".to_string(), "-c".to_string(), c.clone()],
            None => cli.down.clone(),
        };
        let down_pane = if fzf_mode {
            cli.preview.as_ref().map(|pv| {
                let pstate = Arc::new(Mutex::new(StreamState::new()));
                spawn_item_preview(pv.clone(), controls.clone(), pstate.clone());
                (pstate, "preview".to_string())
            })
        } else if down_cmd.is_empty() {
            None
        } else {
            let dstate = Arc::new(Mutex::new(StreamState::new()));
            spawn_preview(down_cmd.clone(), state.clone(), controls.clone(), dstate.clone());
            let label = consumer.clone().unwrap_or_else(|| cli.down.join(" "));
            Some((dstate, label))
        };
        {
            let mut c = controls.lock().unwrap();
            // Flags win; else fall back to the `select` widget's -prompt/-header.
            c.prompt = cli.prompt.clone().or(sel_prompt.clone()).unwrap_or_default();
            c.header = cli.header.clone().or(sel_header.clone()).unwrap_or_default();
            // Form mode: register `input .name` widgets so typing edits them and
            // `apply .name` resolves against their live values.
            c.inputs = spec
                .widgets
                .iter()
                .filter(|w| w.kind == spec::WidgetKind::Input)
                .map(|w| (w.path.trim_start_matches('.').to_string(), String::new()))
                .collect();
        }
        let outcome = tui::run(&spec, state, controls.clone(), down_pane, err_pane, fzf_mode, cli.height.clone());
        if fzf_mode {
            // On Enter (submit) emit the selection (marked lines, or the cursor
            // line). With a `| CONS` consumer, pipe the selection through it first
            // (`find / | _ | perl -pe …` transforms the picked lines). Abort
            // (Esc/Ctrl-C) exits 130 with no output.
            let c = controls.lock().unwrap();
            if c.submit {
                let out = match &consumer {
                    Some(cons) => {
                        run_capture(&["sh".into(), "-c".into(), cons.clone()], &c.result)
                    }
                    None => c.result.clone(),
                };
                for line in out {
                    println!("{line}");
                }
            } else {
                std::process::exit(130);
            }
        }
        outcome
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
        // Zero-config default: a select list under `--fzf`, else a stream tail.
        default_spec_src(cli.fzf).to_string()
    };
    spec::build(&parser::parse(&src)?)
}

/// The synthesized spec source when the user gave no spec/`-e`/`-p`. `--fzf`
/// yields a one-widget `select` list (fzf as a DSL spec); otherwise a full-screen
/// tail of stdin. Both bind `in` so the shared stream feeds the widget.
fn default_spec_src(fzf: bool) -> &'static str {
    if fzf {
        "select .sel\nsource .sel { in }"
    } else {
        "tail .stream\nsource .stream { in }"
    }
}

/// Read stdin into the shared stream for the TUI. When `tee`, each line is also
/// written to stdout immediately (a live passthrough for a downstream consumer),
/// so `find / | arb | consumer` feeds `consumer` continuously while the TUI
/// renders to /dev/tty — arb never blocks the pipeline. A closed consumer stops
/// the passthrough but the TUI keeps updating.
/// Whether a string is an arb pipeline: it has a bare `_` stage (the marker for
/// "arb goes here"), e.g. `sudo find / | _ | perl -pe '…'`.
fn is_arb_pipeline(s: &str) -> bool {
    s.split('|').any(|seg| seg.trim() == "_")
}

/// Split an arb pipeline on the `_` stage marker into (producer, consumer).
/// `find / | _ | grep x` -> ("find /", Some("grep x")); no `_` marker -> the
/// whole string is the producer. (Segments are shelled out per-stage; a zshrs
/// lexer can replace that later without changing arb's fd orchestration.)
fn parse_pipeline(s: &str) -> (String, Option<String>) {
    let segs: Vec<&str> = s.split('|').map(str::trim).collect();
    match segs.iter().position(|seg| *seg == "_") {
        Some(p) => {
            let producer = segs[..p].join(" | ");
            let consumer = segs[p + 1..].join(" | ");
            (
                producer,
                if consumer.is_empty() {
                    None
                } else {
                    Some(consumer)
                },
            )
        }
        None => (s.trim().to_string(), None),
    }
}

/// Drain any reader (a spawned command's stdout or stderr) line-by-line into a
/// `StreamState` — used to feed arb's stream from the producer's stdout and the
/// error pane from its stderr.
fn spawn_source_reader<R: io::Read + Send + 'static>(reader: R, state: Arc<Mutex<StreamState>>) {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(l) => state.lock().unwrap().push(l),
                Err(_) => break,
            }
        }
    });
}

/// Spawn the producer command (`sh -c`) with arb owning its fds: stdout streams
/// into `state`, stderr into a fresh `StreamState` returned for the error pane.
/// The child is detached (runs on its own; killed when arb exits).
fn spawn_producer(
    producer: &str,
    state: Arc<Mutex<StreamState>>,
) -> io::Result<Arc<Mutex<StreamState>>> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(producer)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out = child.stdout.take().ok_or_else(|| io::Error::other("no stdout"))?;
    let err = child.stderr.take().ok_or_else(|| io::Error::other("no stderr"))?;
    drop(child);
    spawn_source_reader(out, state);
    let err_state = Arc::new(Mutex::new(StreamState::new()));
    spawn_source_reader(err, err_state.clone());
    Ok(err_state)
}

fn spawn_reader(state: Arc<Mutex<StreamState>>, tee: bool, controls: Arc<Mutex<tui::Controls>>) {
    thread::spawn(move || {
        // Only hold the stdout lock when actually teeing. Otherwise (e.g. --fzf,
        // which emits its selection from `main` after the TUI exits) this thread
        // would keep stdout locked for its whole life and deadlock `main`'s final
        // `println!` — the process would never terminate after selection.
        let mut out = if tee { Some(io::stdout().lock()) } else { None };
        let mut downstream_open = tee;
        for line in io::stdin().lock().lines() {
            let l = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if let Some(o) = out.as_mut() {
                // Megafilter: only lines matching the live filter flow to stdout,
                // so what the user types reshapes the downstream pipe in real time.
                let pass = tui::filter_matches(&l, &controls.lock().unwrap().filter);
                if pass
                    && downstream_open
                    && writeln!(o, "{l}").and_then(|()| o.flush()).is_err()
                {
                    downstream_open = false;
                }
            }
            state.lock().unwrap().push(l);
        }
    });
}

/// `arb -- CMD` preview: re-run the command chain over arb's CURRENT post-filter
/// output whenever the filter or the stream changes, capturing its stdout+stderr
/// into `dstate` for the pane. The chain always sees the filtered lines, and the
/// pane stays in sync with the filter — no stale pre-filter accumulation, and the
/// command's output never touches the terminal.
fn spawn_preview(
    cmd: Vec<String>,
    state: Arc<Mutex<StreamState>>,
    controls: Arc<Mutex<tui::Controls>>,
    dstate: Arc<Mutex<StreamState>>,
) {
    thread::spawn(move || {
        let mut last: Option<(String, u64)> = None;
        loop {
            thread::sleep(Duration::from_millis(250));
            let (filter, quit) = {
                let c = controls.lock().unwrap();
                (c.filter.clone(), c.quit)
            };
            if quit {
                break;
            }
            // Re-run only when the filter or the line count changed.
            let key = (filter.clone(), state.lock().unwrap().total);
            if last.as_ref() == Some(&key) {
                continue;
            }
            last = Some(key);
            let input: Vec<String> = {
                let s = state.lock().unwrap();
                s.lines
                    .iter()
                    .filter(|l| tui::filter_matches(l, &filter))
                    .cloned()
                    .collect()
            };
            let output = run_capture(&cmd, &input);
            let mut d = dstate.lock().unwrap();
            *d = StreamState::new();
            for l in output {
                d.push(l);
            }
        }
    });
}

/// Single-quote a string for safe `sh -c` substitution of `{}` (a line may hold
/// spaces or shell metacharacters — e.g. a path). `'` -> `'\''`.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// fzf `--preview`: run `template` (with `{}` replaced by the line under the
/// cursor) whenever the cursor moves, capturing its output into `pstate` for the
/// preview pane. Debounced; skips re-running on an unchanged line.
fn spawn_item_preview(
    template: String,
    controls: Arc<Mutex<tui::Controls>>,
    pstate: Arc<Mutex<StreamState>>,
) {
    thread::spawn(move || {
        let mut last = String::from("\u{0}");
        loop {
            thread::sleep(Duration::from_millis(120));
            let (cur, quit) = {
                let c = controls.lock().unwrap();
                (c.current.clone(), c.quit)
            };
            if quit {
                break;
            }
            if cur == last {
                continue;
            }
            last = cur.clone();
            if cur.is_empty() {
                continue;
            }
            let cmd = template.replace("{}", &shell_escape(&cur));
            let output = run_capture(&["sh".to_string(), "-c".to_string(), cmd], &[]);
            let mut p = pstate.lock().unwrap();
            *p = StreamState::new();
            for l in output {
                p.push(l);
            }
        }
    });
}

/// Run `cmd`, feeding `input` on its stdin, and collect its stdout then stderr as
/// lines. A spawn/exec error becomes a single diagnostic line so it shows in the
/// pane rather than crashing arb.
fn run_capture(cmd: &[String], input: &[String]) -> Vec<String> {
    let child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return vec![format!("arb: {}: {e}", cmd[0])],
    };
    if let Some(mut si) = child.stdin.take() {
        for l in input {
            if writeln!(si, "{l}").is_err() {
                break; // consumer closed its stdin early (e.g. `head`)
            }
        }
    }
    match child.wait_with_output() {
        Ok(o) => {
            let mut v: Vec<String> = String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(String::from)
                .collect();
            v.extend(String::from_utf8_lossy(&o.stderr).lines().map(String::from));
            v
        }
        Err(e) => vec![format!("arb: {}: {e}", cmd[0])],
    }
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
