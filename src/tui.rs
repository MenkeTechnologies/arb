//! ratatui render loop. Widgets are auto-tiled vertically (explicit `pack`/`grid`
//! geometry arrives in a later milestone). `text`/`tail`/`list` render live from
//! the shared stream; other widget kinds show an honest "not yet rendered"
//! placeholder rather than faking output.

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
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::{Frame, Terminal};

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

    for (i, w) in spec.widgets.iter().enumerate() {
        render_widget(f, chunks[i], w, st);
    }
}

fn render_widget(f: &mut Frame, area: Rect, w: &Widget, st: &StreamState) {
    let title = format!(" {} · {} · {} ln {:.0}/s ", w.path, w.kind.label(), st.total, st.rate());
    let block = Block::default().borders(Borders::ALL).title(title);
    match w.kind {
        WidgetKind::Text => {
            let last = st.lines.back().map(String::as_str).unwrap_or("");
            f.render_widget(Paragraph::new(last).block(block), area);
        }
        WidgetKind::Tail | WidgetKind::List => {
            let inner_h = area.height.saturating_sub(2) as usize;
            let skip = st.lines.len().saturating_sub(inner_h);
            let items: Vec<ListItem> = st
                .lines
                .iter()
                .skip(skip)
                .map(|l| ListItem::new(l.as_str()))
                .collect();
            f.render_widget(List::new(items).block(block), area);
        }
        _ => {
            let msg = format!("{} — not yet rendered (M1)", w.kind.label());
            f.render_widget(Paragraph::new(msg).block(block), area);
        }
    }
}
