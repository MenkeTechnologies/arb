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
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph};
use ratatui::{Frame, Terminal};

use crate::query::{eval, QueryResult};
use crate::spec::{Spec, Widget, WidgetKind};
use crate::stream::StreamState;

/// Run the TUI until the user quits (`q`/Esc/Ctrl-C).
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
        // crossterm reads /dev/tty on Unix, so key events arrive even though
        // stdin is carrying the data pipe.
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

fn render(f: &mut Frame, spec: &Spec, st: &StreamState) {
    let area = f.area();
    let n = spec.widgets.len().max(1) as u32;
    let constraints: Vec<Constraint> = (0..n).map(|_| Constraint::Ratio(1, n)).collect();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    if spec.widgets.is_empty() {
        let msg = Paragraph::new("arb: spec has no widgets")
            .block(Block::default().borders(Borders::ALL).title(" arb "));
        f.render_widget(msg, chunks[0]);
        return;
    }

    // One materialization of the ring per frame, shared by every widget's eval.
    let raw: Vec<String> = st.lines.iter().cloned().collect();
    let elapsed = st.start.elapsed().as_secs_f64();
    for (i, w) in spec.widgets.iter().enumerate() {
        let result = w.source.as_ref().map(|s| eval(&s.pipeline, &raw, elapsed));
        render_widget(f, chunks[i], w, st, result);
    }
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
                None => st.lines.back().cloned().unwrap_or_default(),
            };
            f.render_widget(Paragraph::new(s).block(block), area);
        }
        WidgetKind::Tail | WidgetKind::List => {
            let owned: Vec<String> = match &result {
                Some(QueryResult::Lines(ls)) => ls.clone(),
                Some(QueryResult::Scalar(v)) => vec![format!("{v}")],
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
        _ => {
            let msg = format!("{} — not yet rendered (M2a)", w.kind.label());
            f.render_widget(Paragraph::new(msg).block(block), area);
        }
    }
}
