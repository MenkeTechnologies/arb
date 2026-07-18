//! Web target: serve the same [`Spec`] as a live browser dashboard. A std-only
//! HTTP server (no async runtime, no framework) binds a local port, serves one
//! self-contained page at `/`, and answers `GET /data` with the current widget
//! values as JSON. The page polls `/data` and swaps each panel's text — so the
//! same spec that drives the ratatui TUI drives a browser, live.
//!
//! v1 renders every widget's evaluated body as text (the server does the eval and
//! formatting; the client just displays it). Richer per-widget rendering and a
//! WebSocket push path can replace polling later without changing the spec.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::query::{eval, QueryResult};
use crate::spec::{Spec, Widget, WidgetKind};
use crate::stream::StreamState;
use crate::web::{escape, STYLE};

/// Bind `127.0.0.1:port` and serve the dashboard until the process exits. `port`
/// 0 lets the OS pick a free port (the chosen address is printed). Blocks.
pub fn serve(spec: Spec, state: Arc<Mutex<StreamState>>, port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let addr = listener.local_addr()?;
    // A served URL is expected output for a serve command (like `textual serve`);
    // it goes to stderr so stdout stays clean if arb is also teeing a pipe.
    eprintln!("arb: serving dashboard at http://{addr}/  (Ctrl-C to stop)");

    let spec = Arc::new(spec);
    let page = Arc::new(render_page(&spec));
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let (spec, state, page) = (spec.clone(), state.clone(), page.clone());
        // One thread per connection so a slow/holding client never blocks others.
        thread::spawn(move || {
            let _ = handle(conn, &spec, &state, &page);
        });
    }
    Ok(())
}

/// Read the request line, route on its path, write one `Connection: close`
/// response. Request headers/body are ignored — this only serves GETs.
fn handle(
    mut conn: TcpStream,
    spec: &Spec,
    state: &Arc<Mutex<StreamState>>,
    page: &str,
) -> std::io::Result<()> {
    let path = {
        let mut reader = BufReader::new(&conn);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        line.split_whitespace().nth(1).unwrap_or("/").to_string()
    };
    let (ctype, body) = match path.as_str() {
        "/data" => ("application/json", data_json(spec, state)),
        "/" => ("text/html; charset=utf-8", page.to_string()),
        _ => {
            let msg = "not found";
            let resp = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{msg}",
                msg.len()
            );
            return conn.write_all(resp.as_bytes());
        }
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    conn.write_all(resp.as_bytes())
}

/// Evaluate every widget against the current stream and return their data as a
/// JSON array in spec order (the page maps by index). Each item is shaped by kind
/// so the client can render a real widget: `gauge` → `{scalar,max}`, `bars`/
/// `histo`/`spark` → `{pairs,top}`, everything else → `{text}`.
fn data_json(spec: &Spec, state: &Arc<Mutex<StreamState>>) -> String {
    let (raw, elapsed): (Vec<String>, f64) = {
        let st = state.lock().unwrap();
        (st.lines.iter().cloned().collect(), st.start.elapsed().as_secs_f64())
    };
    let items: Vec<serde_json::Value> =
        spec.widgets.iter().map(|w| widget_json(w, &raw, elapsed)).collect();
    serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string())
}

/// One widget's data as JSON, shaped by kind so the client renders a bar/gauge/
/// text rather than a flat string.
fn widget_json(w: &Widget, raw: &[String], elapsed: f64) -> serde_json::Value {
    use serde_json::json;
    let base = |extra: serde_json::Value| {
        let mut m = json!({ "path": w.path, "kind": w.kind.label() });
        if let (Some(obj), Some(ex)) = (m.as_object_mut(), extra.as_object()) {
            for (k, v) in ex {
                obj.insert(k.clone(), v.clone());
            }
        }
        m
    };
    let opt_f64 = |k: &str, d: f64| w.opts.get(k).and_then(|s| s.parse::<f64>().ok()).unwrap_or(d);
    let result = w.source.as_ref().map(|s| eval(&s.pipeline, raw, elapsed));
    match w.kind {
        WidgetKind::Gauge => {
            let scalar = match &result {
                Some(QueryResult::Scalar(v)) => *v,
                _ => 0.0,
            };
            base(json!({ "scalar": scalar, "max": opt_f64("max", 100.0) }))
        }
        WidgetKind::Bars | WidgetKind::Histo | WidgetKind::Spark => {
            let pairs: Vec<(String, u64)> = match &result {
                Some(QueryResult::Pairs(p)) => p.clone(),
                _ => Vec::new(),
            };
            let top = w.opts.get("top").and_then(|s| s.parse::<usize>().ok()).unwrap_or(20);
            base(json!({ "pairs": pairs, "top": top }))
        }
        _ => base(json!({ "text": widget_text(w, raw, elapsed, result) })),
    }
}

/// Render a text-kind widget's evaluated body as plain text (client displays it
/// verbatim). `result` is the pre-evaluated pipeline output, if the widget is bound.
fn widget_text(w: &Widget, raw: &[String], _elapsed: f64, result: Option<QueryResult>) -> String {
    let Some(result) = result else {
        // No source: show the latest stream line as a lightweight tail.
        let _ = w;
        return raw.last().cloned().unwrap_or_default();
    };
    match result {
        QueryResult::Lines(ls) => {
            // Cap to the last 200 lines so a huge stream doesn't bloat each poll.
            let n = ls.len();
            ls[n.saturating_sub(200)..].join("\n")
        }
        QueryResult::Scalar(v) => format!("{v:.2}"),
        QueryResult::Pairs(ps) => ps
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// The self-contained dashboard page: the shared cyberpunk stylesheet, one panel
/// per widget (body identified by index), and a poller that refreshes `/data`.
fn render_page(spec: &Spec) -> String {
    let mut panels = String::new();
    for (i, w) in spec.widgets.iter().enumerate() {
        panels.push_str(&format!(
            "<section class=\"panel\">\n\
             <header class=\"phead\"><span class=\"ppath\">{}</span><span class=\"pkind\">{}</span></header>\n\
             <div class=\"pbody\" id=\"wb{i}\"></div>\n\
             </section>\n",
            escape(&w.path),
            escape(w.kind.label()),
        ));
    }
    if spec.widgets.is_empty() {
        panels.push_str("<p class=\"empty\">no widgets</p>\n");
    }
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>arb dashboard</title>\n{STYLE}{WIDGET_CSS}\
         </head>\n<body>\n\
         <h1>arb dashboard <span id=\"stat\" class=\"pkind\"></span></h1>\n\
         <main class=\"grid\">\n{panels}</main>\n\
         <script>\n{POLLER}</script>\n\
         </body>\n</html>\n"
    )
}

/// Extra styles for the live widgets: text bodies, and the label+track+fill bar
/// rows used by `gauge`/`bars`/`histo`/`spark`.
const WIDGET_CSS: &str = "<style>\n\
.pbody { max-height: 16rem; overflow: auto; }\n\
.txt { margin: 0; white-space: pre-wrap; word-break: break-word; font: inherit; }\n\
.row { display: flex; align-items: center; gap: 0.5rem; margin: 0.15rem 0; }\n\
.lab { flex: 0 0 40%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--fg); }\n\
.track { flex: 1; height: 0.8rem; background: rgba(0,229,255,0.08); border-radius: 2px; overflow: hidden; }\n\
.fill { height: 100%; background: var(--cyan); }\n\
</style>\n";

/// Client poller: fetch `/data` on an interval and render each widget by kind.
/// All labels go through `textContent` and bar widths are numeric — stream data
/// can never inject markup (no `innerHTML` with data).
const POLLER: &str = "\
const stat = document.getElementById('stat');\n\
function bar(label, pct) {\n\
  const row = document.createElement('div'); row.className = 'row';\n\
  const lab = document.createElement('span'); lab.className = 'lab'; lab.textContent = label;\n\
  const track = document.createElement('div'); track.className = 'track';\n\
  const fill = document.createElement('div'); fill.className = 'fill';\n\
  fill.style.width = Math.max(0, Math.min(100, pct)) + '%';\n\
  track.appendChild(fill); row.appendChild(lab); row.appendChild(track);\n\
  return row;\n\
}\n\
function render(el, it) {\n\
  el.replaceChildren();\n\
  if (it.kind === 'gauge') {\n\
    const max = it.max || 100, v = it.scalar || 0;\n\
    el.appendChild(bar(v.toFixed(0) + ' / ' + max, max ? v / max * 100 : 0));\n\
  } else if (it.pairs) {\n\
    const rows = it.pairs.slice(0, it.top || 20);\n\
    const maxv = Math.max(1, ...rows.map(p => p[1]));\n\
    rows.forEach(([k, v]) => el.appendChild(bar(k + '  ' + v, v / maxv * 100)));\n\
  } else {\n\
    const pre = document.createElement('pre'); pre.className = 'txt';\n\
    pre.textContent = it.text || ''; el.appendChild(pre);\n\
  }\n\
}\n\
async function tick() {\n\
  try {\n\
    const r = await fetch('/data', {cache: 'no-store'});\n\
    const items = await r.json();\n\
    items.forEach((it, i) => {\n\
      const el = document.getElementById('wb' + i);\n\
      if (el) render(el, it);\n\
    });\n\
    stat.textContent = 'live \\u00b7 ' + new Date().toLocaleTimeString();\n\
  } catch (e) {\n\
    stat.textContent = 'disconnected';\n\
  }\n\
}\n\
tick();\n\
setInterval(tick, 500);\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;
    use crate::spec::build;

    fn state_with(lines: &[&str]) -> Arc<Mutex<StreamState>> {
        let st = Arc::new(Mutex::new(StreamState::new()));
        {
            let mut s = st.lock().unwrap();
            for l in lines {
                s.push((*l).to_string());
            }
        }
        st
    }

    #[test]
    fn data_json_reflects_widget_eval() {
        let spec = build(
            &parse("gauge .g -max 100\nsource .g { in; count }\nlist .l\nsource .l { in }").unwrap(),
        )
        .unwrap();
        let st = state_with(&["a", "b", "c"]);
        let json: serde_json::Value = serde_json::from_str(&data_json(&spec, &st)).unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // Gauge carries the count scalar + its max for the client bar.
        assert_eq!(arr[0]["path"], ".g");
        assert_eq!(arr[0]["scalar"], 3.0);
        assert_eq!(arr[0]["max"], 100.0);
        // List carries text (the lines joined).
        assert_eq!(arr[1]["path"], ".l");
        assert_eq!(arr[1]["text"], "a\nb\nc");
    }

    #[test]
    fn data_json_bars_carry_pairs() {
        let spec =
            build(&parse("histo .h\nsource .h { in; tally }").unwrap()).unwrap();
        let st = state_with(&["x", "y", "x", "x", "y"]);
        let json: serde_json::Value = serde_json::from_str(&data_json(&spec, &st)).unwrap();
        let pairs = json[0]["pairs"].as_array().unwrap();
        // tally counts occurrences → x:3, y:2 (order is count-desc).
        assert_eq!(pairs[0][0], "x");
        assert_eq!(pairs[0][1], 3);
        assert_eq!(pairs[1][0], "y");
        assert_eq!(pairs[1][1], 2);
    }

    #[test]
    fn render_page_has_panel_per_widget_and_poller() {
        let spec = build(&parse("gauge .g -max 100\nlist .l").unwrap()).unwrap();
        let page = render_page(&spec);
        assert!(page.contains("<title>arb dashboard</title>"));
        assert!(page.contains("id=\"wb0\""));
        assert!(page.contains("id=\"wb1\""));
        assert!(page.contains("setInterval"));
        // Widget paths are escaped into the panel headers.
        assert!(page.contains(".g"));
        assert!(page.contains(".l"));
    }

    #[test]
    fn widget_without_source_tails_latest_line() {
        let spec = build(&parse("text .t").unwrap()).unwrap();
        let val = widget_json(&spec.widgets[0], &["x".into(), "y".into()], 0.0);
        assert_eq!(val["text"], "y");
    }
}
