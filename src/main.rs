//! arb — visualize and modify Unix pipelines.
//!
//! Milestone 0 (walking skeleton): zero-config mode. Pipe a stream in and arb
//! renders a live tail TUI of it — `find / | arb`. When there is no controlling
//! terminal on stdout (piped/redirected/CI), it degrades to a headless summary
//! instead of a TUI, which is both correct behavior and the CI-testable path.
//!
//! The spec language (Tcl/Tk-flavored), fusevm lowering, query engine, web
//! target, actors, and package manager arrive in later milestones (see SPEC.md).
//! Nothing here fakes those — M0 is exactly the stdin -> ratatui tail spine.

use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use ratatui::crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem};

/// arb — pipe a stream in, get a live TUI.
#[derive(Parser, Debug)]
#[command(name = "arb", version, about = "Visualize and modify Unix pipelines.")]
struct Cli {
    /// Dashboard spec file (.arb). Not yet interpreted in M0 — zero-config tail only.
    spec: Option<String>,
    /// Inline spec. Not yet interpreted in M0.
    #[arg(short = 'e', long = "eval")]
    eval: Option<String>,
}

/// How many lines the tail retains in memory (older lines are dropped from view).
const RING_CAP: usize = 5000;

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    if cli.spec.is_some() || cli.eval.is_some() {
        eprintln!("arb: spec interpretation lands in M1; running zero-config tail for now");
    }

    if io::stdin().is_terminal() {
        eprintln!("arb: no input — pipe a stream in, e.g. `find / | arb`");
        std::process::exit(2);
    }

    // Read the data pipe (stdin) on a background thread so a fast producer never
    // blocks the render/input loop. crossterm reads key events from /dev/tty on
    // Unix, so using stdin for data does not steal the UI's input.
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    if io::stdout().is_terminal() {
        run_tui(rx)
    } else {
        run_headless(rx)
    }
}

/// No terminal on stdout: drain the stream and print a summary. Correct behavior
/// (no TUI without a tty) and the path exercised by the headless test.
fn run_headless(rx: mpsc::Receiver<String>) -> io::Result<()> {
    let mut count: u64 = 0;
    let mut tail: Vec<String> = Vec::new();
    while let Ok(line) = rx.recv() {
        count += 1;
        tail.push(line);
        if tail.len() > 10 {
            tail.remove(0);
        }
    }
    let mut out = io::stdout().lock();
    writeln!(out, "arb: {count} lines (no terminal; TUI skipped). tail:")?;
    for l in &tail {
        writeln!(out, "  {l}")?;
    }
    Ok(())
}

/// Live tail TUI: render the trailing window of the stream with a live count/rate
/// header. `q`/Esc/Ctrl-C quits.
fn run_tui(rx: mpsc::Receiver<String>) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut lines: Vec<String> = Vec::new();
    let mut total: u64 = 0;
    let start = Instant::now();
    let mut last_draw = Instant::now() - Duration::from_secs(1);

    let outcome = loop {
        let mut got = false;
        while let Ok(line) = rx.try_recv() {
            lines.push(line);
            total += 1;
            got = true;
        }
        if lines.len() > RING_CAP {
            let excess = lines.len() - RING_CAP;
            lines.drain(0..excess);
        }

        if got || last_draw.elapsed() >= Duration::from_millis(200) {
            last_draw = Instant::now();
            let draw = terminal.draw(|f| {
                let area = f.area();
                let visible = area.height.saturating_sub(2) as usize;
                let start_idx = lines.len().saturating_sub(visible);
                let items: Vec<ListItem> = lines[start_idx..]
                    .iter()
                    .map(|l| ListItem::new(l.as_str()))
                    .collect();
                let secs = start.elapsed().as_secs_f64().max(0.001);
                let rate = total as f64 / secs;
                let title = format!(" arb · {total} lines · {rate:.0}/s · q to quit ");
                let list =
                    List::new(items).block(Block::default().borders(Borders::ALL).title(title));
                f.render_widget(list, area);
            });
            if let Err(e) = draw {
                break Err(e);
            }
        }

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    let quit = matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                        || (k.code == KeyCode::Char('c')
                            && k.modifiers.contains(KeyModifiers::CONTROL));
                    if quit {
                        break Ok(());
                    }
                }
            }
        }
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    outcome
}
