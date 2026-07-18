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

use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use clap::Parser;

use arb::query::{self, QueryOp, QueryResult};
use arb::spec::{self, Spec};
use arb::stream::StreamState;
use arb::{parser, tui};

/// arb — pipe a stream in, get a live TUI.
#[derive(Parser, Debug)]
#[command(name = "arb", version, about = "Visualize and modify Unix pipelines.")]
struct Cli {
    /// Dashboard spec file (.arb).
    spec: Option<String>,
    /// Inline spec, e.g. `-e 'gauge .g -max 100; source .g { in; count }'`.
    #[arg(short = 'e', long = "eval")]
    eval: Option<String>,
    /// Run a preset / stdlib module by name, e.g. `-p logs` (== `import logs`).
    #[arg(short = 'p', long = "preset")]
    preset: Option<String>,
    /// List available presets (bundled stdlib + `~/.arb/lib`) and exit.
    #[arg(short = 'l', long = "list")]
    list: bool,
    /// Save a spec as a named user preset in `~/.arb/lib`, then exit.
    /// Source is the `FILE` argument or `-e SRC`. E.g. `arb --save api dash.arb`.
    #[arg(long = "save", value_name = "NAME")]
    save: Option<String>,
    /// Interactive REPL — author + test specs against a sample buffer.
    #[arg(short = 'r', long = "repl")]
    repl: bool,
    /// Generate a static HTML dashboard from the spec to stdout, then exit
    /// (`arb -p logs --html > dash.html`).
    #[arg(long = "html")]
    html: bool,
    /// Validate the spec (parse + build) and exit 0/1 without reading stdin.
    #[arg(long = "check")]
    check: bool,
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

    if io::stdout().is_terminal() {
        if needs_stdin {
            spawn_reader(state.clone());
        }
        tui::run(&spec, state)
    } else if let Some(out_ops) = &spec.out {
        // Piped downstream: arb modifies the stream — emit the `out` pipeline.
        if needs_stdin {
            read_stdin_sync(&state);
        }
        emit_out(out_ops, &state)
    } else {
        if needs_stdin {
            read_stdin_sync(&state);
        }
        dump(&spec, &state)
    }
}

/// Write the `out { … }` pipeline's result to stdout (arb as a pipe filter).
fn emit_out(ops: &[QueryOp], state: &Arc<Mutex<StreamState>>) -> io::Result<()> {
    let st = state.lock().unwrap();
    let raw: Vec<String> = st.lines.iter().cloned().collect();
    let elapsed = st.start.elapsed().as_secs_f64();
    let mut out = io::stdout().lock();
    match query::eval(ops, &raw, elapsed) {
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

fn spawn_reader(state: Arc<Mutex<StreamState>>) {
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            match line {
                Ok(l) => state.lock().unwrap().push(l),
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
