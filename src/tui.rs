//! ratatui render loop. Widgets are auto-tiled vertically (explicit `pack`/`grid`
//! geometry arrives later). Each widget's `source` pipeline is evaluated against
//! the shared stream every frame; `text`/`tail`/`list` render the resulting
//! lines and `gauge` renders a scalar against `-max`. Widget kinds without a
//! renderer yet show an honest placeholder rather than faking output.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::{BarChart, Block, Borders, Gauge, List, ListItem, Paragraph};
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

/// Spawn a thread that reads key bytes straight from `/dev/tty` and flags quit
/// on `q` / Esc / Ctrl-C. We read the terminal device directly — exactly how
/// `vipe` gets the keyboard while stdin carries the pipe — instead of using
/// crossterm's event source, whose `mio`-on-tty reader fails to initialize on
/// some hosts (the "failed to initialize input reader" crash on `find / | arb`).
/// Raw mode (set on the terminal device by `enable_raw_mode`) makes these bytes
/// arrive unbuffered, so a single keypress is seen immediately.
fn spawn_key_reader(quit: Arc<AtomicBool>) {
    if let Ok(mut tty) = OpenOptions::new().read(true).open("/dev/tty") {
        thread::spawn(move || {
            let mut buf = [0u8; 1];
            while let Ok(1) = tty.read(&mut buf) {
                if matches!(buf[0], b'q' | 0x1b | 0x03) {
                    quit.store(true, Ordering::SeqCst);
                    break;
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
pub fn run(spec: &Spec, state: Arc<Mutex<StreamState>>) -> io::Result<()> {
    let tty: File = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    enable_raw_mode()?;
    // Wrap the tty in the backend before entering the alternate screen: the
    // backend is write-only, so `execute!` isn't ambiguous over `File`'s Read +
    // Write `by_ref`.
    let mut terminal = Terminal::new(CrosstermBackend::new(tty))?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;

    let quit = Arc::new(AtomicBool::new(false));
    spawn_key_reader(quit.clone());

    // Redraw on a fixed cadence so live stream updates show; the key reader runs
    // independently, so the render loop never blocks on input (the pipeline keeps
    // flowing regardless of keypresses).
    let outcome = loop {
        if quit.load(Ordering::SeqCst) {
            break Ok(());
        }
        let st = state.lock().unwrap();
        let draw = terminal.draw(|f| render(f, spec, &st));
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

fn render(f: &mut Frame, spec: &Spec, st: &StreamState) {
    let area = f.area();
    if spec.widgets.is_empty() {
        let msg = Paragraph::new("arb: spec has no widgets")
            .block(Block::default().borders(Borders::ALL).title(" arb "));
        f.render_widget(msg, area);
        return;
    }

    let rects = compute_rects(area, spec);
    // One materialization of the ring per frame, shared by every widget's eval.
    let raw: Vec<String> = st.lines.iter().cloned().collect();
    let elapsed = st.start.elapsed().as_secs_f64();
    for (i, w) in spec.widgets.iter().enumerate() {
        let result = w.source.as_ref().map(|s| eval(&s.pipeline, &raw, elapsed));
        render_widget(f, rects[i], w, st, result);
    }
}

/// One rect per widget. Auto vertical stack unless any widget has a grid cell,
/// in which case a rows×cols grid is built and each widget placed in its cell
/// (widgets sharing a cell overlap; last-drawn wins). Spans arrive later.
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

fn render_widget(f: &mut Frame, area: Rect, w: &Widget, st: &StreamState, result: Option<QueryResult>) {
    let title = format!(
        " {} · {} · {} ln {:.0}/s ",
        w.path,
        w.kind.label(),
        st.total,
        st.rate()
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    match w.kind {
        WidgetKind::Text => {
            let s = match &result {
                Some(QueryResult::Scalar(v)) => format!("{v:.2}"),
                Some(QueryResult::Lines(ls)) => ls.last().cloned().unwrap_or_default(),
                Some(QueryResult::Pairs(p)) => p
                    .first()
                    .map(|(k, v)| format!("{k} ({v})"))
                    .unwrap_or_default(),
                None => st.lines.back().cloned().unwrap_or_default(),
            };
            f.render_widget(Paragraph::new(s).block(block), area);
        }
        WidgetKind::Tail | WidgetKind::List => {
            let owned: Vec<String> = match &result {
                Some(QueryResult::Lines(ls)) => ls.clone(),
                Some(QueryResult::Scalar(v)) => vec![format!("{v}")],
                Some(QueryResult::Pairs(p)) => {
                    p.iter().map(|(k, v)| format!("{k}  {v}")).collect()
                }
                None => st.lines.iter().cloned().collect(),
            };
            let inner_h = area.height.saturating_sub(2) as usize;
            let skip = owned.len().saturating_sub(inner_h);
            let items: Vec<ListItem> = owned
                .iter()
                .skip(skip)
                .map(|l| ListItem::new(l.as_str()))
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
