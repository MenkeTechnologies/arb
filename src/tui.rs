//! ratatui render loop. Widgets are auto-tiled vertically (explicit `pack`/`grid`
//! geometry arrives later). Each widget's `source` pipeline is evaluated against
//! the shared stream every frame; `text`/`tail`/`list` render the resulting
//! lines and `gauge` renders a scalar against `-max`. Widget kinds without a
//! renderer yet show an honest placeholder rather than faking output.

use std::io::{self, Stdout};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
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

/// Run the TUI until the user quits (`q`/Esc/Ctrl-C). Any I/O error inside the
/// loop breaks out so the terminal is always restored (raw mode off, alternate
/// screen left, cursor shown) before the error is returned — never left wedged.
pub fn run(spec: &Spec, state: Arc<Mutex<StreamState>>) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal: Terminal<CrosstermBackend<Stdout>> =
        Terminal::new(CrosstermBackend::new(stdout))?;

    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let outcome = loop {
        if last_draw.elapsed() >= Duration::from_millis(120) {
            last_draw = Instant::now();
            let st = state.lock().unwrap();
            let draw = terminal.draw(|f| render(f, spec, &st));
            drop(st);
            if let Err(e) = draw {
                break Err(e);
            }
        }
        // Key events come from `/dev/tty` (see `events_available`), so they
        // arrive even though stdin is carrying the data pipe. Errors break the
        // loop rather than `?`-propagating, so the restore code below still runs.
        match event::poll(Duration::from_millis(50)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                    let quit = matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                        || (k.code == KeyCode::Char('c')
                            && k.modifiers.contains(KeyModifiers::CONTROL));
                    if quit {
                        break Ok(());
                    }
                }
                Ok(_) => {}
                Err(e) => break Err(e),
            },
            Ok(false) => {}
            Err(e) => break Err(e),
        }
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
