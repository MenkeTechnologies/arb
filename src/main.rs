//! arb — visualize and modify Unix pipelines.
//!
//! Pipe a stream in and arb renders a dynamic TUI built from a declarative spec.
//! With no spec it synthesizes a full-screen `tail` of stdin. With no controlling
//! terminal on stdout (piped onward / CI) it prints the parsed spec plus stream
//! stats instead of a TUI — correct behavior and the headless-test path.
//!
//! M1 interprets the declarative subset (widgets, `source`, binds). The
//! expression layer, fusevm lowering, query verbs, web target, actors, and
//! package manager are later milestones (see SPEC.md) and are not faked here.

use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use clap::Parser;

use arb::spec::{self, Source, Spec};
use arb::stream::StreamState;
use arb::{parser, tui};

/// arb — pipe a stream in, get a live TUI.
#[derive(Parser, Debug)]
#[command(name = "arb", version, about = "Visualize and modify Unix pipelines.")]
struct Cli {
    /// Dashboard spec file (.arb).
    spec: Option<String>,
    /// Inline spec, e.g. `-e 'text .t; source .t { in }'`.
    #[arg(short = 'e', long = "eval")]
    eval: Option<String>,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    let spec = match load_spec(&cli) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("arb: {e}");
            std::process::exit(1);
        }
    };

    let needs_stdin = spec.widgets.iter().any(|w| w.source == Some(Source::Stdin));
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
    } else {
        if needs_stdin {
            read_stdin_sync(&state);
        }
        dump(&spec, &state)
    }
}

fn load_spec(cli: &Cli) -> Result<Spec, String> {
    let src = if let Some(e) = &cli.eval {
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
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "arb: spec — {} widget(s) (no terminal; render skipped)",
        spec.widgets.len()
    )?;
    for w in &spec.widgets {
        let src = match &w.source {
            Some(Source::Stdin) => "stdin",
            None => "-",
        };
        writeln!(
            out,
            "  {:<10} {:<6} source={:<6} opts={:?}",
            w.path,
            w.kind.label(),
            src,
            w.opts
        )?;
    }
    writeln!(out, "stream: {} lines", st.total)?;
    for l in st.lines.iter().rev().take(5).rev() {
        writeln!(out, "  {l}")?;
    }
    Ok(())
}
