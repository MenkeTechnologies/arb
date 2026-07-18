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
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};

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
    // Read the request line + headers (needed for the WebSocket upgrade key).
    let (path, headers) = {
        let mut reader = BufReader::new(&conn);
        let mut req = String::new();
        reader.read_line(&mut req)?;
        let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let t = line.trim_end().to_string();
            if t.is_empty() {
                break;
            }
            headers.push(t);
        }
        (path, headers)
    };
    let header = |name: &str| -> Option<String> {
        headers.iter().find_map(|h| {
            let (k, v) = h.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    };
    // `GET /ws` with a WebSocket key: upgrade and push updates (no more polling).
    if path == "/ws" {
        if let Some(key) = header("Sec-WebSocket-Key") {
            return ws_serve(conn, spec, state, &key);
        }
    }
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

/// Complete the WebSocket handshake, then push the widget data as a text frame
/// every 250 ms until the client disconnects (a failed write ends the loop).
fn ws_serve(
    mut conn: TcpStream,
    spec: &Spec,
    state: &Arc<Mutex<StreamState>>,
    key: &str,
) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        ws_accept(key)
    );
    conn.write_all(resp.as_bytes())?;
    loop {
        let frame = ws_text_frame(&data_json(spec, state));
        if conn.write_all(&frame).is_err() {
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

/// RFC 6455 handshake response token: `base64(SHA1(key + magic GUID))`.
fn ws_accept(key: &str) -> String {
    const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let mut buf = key.as_bytes().to_vec();
    buf.extend_from_slice(WS_GUID.as_bytes());
    STANDARD.encode(sha1(&buf))
}

/// Encode a server→client text frame (FIN + opcode 0x1, unmasked per the RFC).
fn ws_text_frame(payload: &str) -> Vec<u8> {
    let data = payload.as_bytes();
    let mut frame = vec![0x81u8]; // FIN=1, opcode=text
    let len = data.len();
    if len < 126 {
        frame.push(len as u8);
    } else if len <= u16::MAX as usize {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(data);
    frame
}

/// SHA-1 (FIPS 180-1), hand-rolled so the WebSocket handshake needs no crypto
/// dependency. Used only for the RFC 6455 accept token, never for security.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let ml = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&ml.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
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
        WidgetKind::Bars | WidgetKind::Histo => {
            let pairs: Vec<(String, u64)> = match &result {
                Some(QueryResult::Pairs(p)) => p.clone(),
                _ => Vec::new(),
            };
            let top = w.opts.get("top").and_then(|s| s.parse::<usize>().ok()).unwrap_or(20);
            base(json!({ "pairs": pairs, "top": top }))
        }
        WidgetKind::Spark => {
            let series: Vec<f64> = match &result {
                Some(QueryResult::Pairs(p)) => p.iter().map(|(_, v)| *v as f64).collect(),
                Some(QueryResult::Lines(ls)) => crate::query::numeric_series(ls),
                Some(QueryResult::Scalar(v)) => vec![*v],
                None => crate::query::numeric_series(raw),
            };
            let n = series.len();
            let series = &series[n.saturating_sub(400)..]; // cap points per poll
            base(json!({ "spark": crate::query::sparkline(series) }))
        }
        WidgetKind::Chart => {
            let series: Vec<f64> = match &result {
                Some(QueryResult::Pairs(p)) => p.iter().map(|(_, v)| *v as f64).collect(),
                Some(QueryResult::Lines(ls)) => crate::query::numeric_series(ls),
                Some(QueryResult::Scalar(v)) => vec![*v],
                None => crate::query::numeric_series(raw),
            };
            let n = series.len();
            let series = &series[n.saturating_sub(500)..]; // cap points per poll
            base(json!({ "series": series }))
        }
        WidgetKind::Table => {
            let lines: Vec<String> = match &result {
                Some(QueryResult::Lines(ls)) => ls.clone(),
                Some(QueryResult::Pairs(p)) => p.iter().map(|(k, v)| format!("{k} {v}")).collect(),
                _ => raw.to_vec(),
            };
            let (headers, rows) =
                crate::query::table_data(&lines, w.opts.get("cols").map(String::as_str));
            let n = rows.len();
            let rows = &rows[n.saturating_sub(200)..]; // cap per poll
            base(json!({ "headers": headers, "rows": rows }))
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
.tbl { border-collapse: collapse; width: 100%; font-size: 0.9em; }\n\
.tbl th, .tbl td { border: 1px solid var(--edge); padding: 2px 6px; text-align: left; white-space: nowrap; }\n\
.tbl th { color: var(--cyan); position: sticky; top: 0; background: var(--panel); }\n\
.spark { color: var(--cyan); font-size: 2rem; line-height: 1; letter-spacing: 1px; word-break: break-all; }\n\
.chart { width: 100%; height: 6rem; display: block; }\n\
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
  } else if (it.spark !== undefined) {\n\
    const s = document.createElement('div'); s.className = 'spark';\n\
    s.textContent = it.spark || '\\u2014'; el.appendChild(s);\n\
  } else if (it.series) {\n\
    const NS = 'http://www.w3.org/2000/svg', W = 100, H = 40;\n\
    const svg = document.createElementNS(NS, 'svg');\n\
    svg.setAttribute('viewBox', '0 0 ' + W + ' ' + H);\n\
    svg.setAttribute('class', 'chart'); svg.setAttribute('preserveAspectRatio', 'none');\n\
    const s = it.series;\n\
    if (s.length > 1) {\n\
      const mn = Math.min(...s), mx = Math.max(...s), rng = (mx - mn) || 1;\n\
      const pts = s.map((v, i) => (i / (s.length - 1) * W) + ',' + (H - (v - mn) / rng * H)).join(' ');\n\
      const pl = document.createElementNS(NS, 'polyline');\n\
      pl.setAttribute('points', pts); pl.setAttribute('fill', 'none');\n\
      pl.setAttribute('stroke', '#00e5ff'); pl.setAttribute('stroke-width', '1');\n\
      pl.setAttribute('vector-effect', 'non-scaling-stroke'); svg.appendChild(pl);\n\
    }\n\
    el.appendChild(svg);\n\
  } else if (it.rows) {\n\
    const t = document.createElement('table'); t.className = 'tbl';\n\
    if (it.headers && it.headers.length) {\n\
      const tr = document.createElement('tr');\n\
      it.headers.forEach(h => { const th = document.createElement('th'); th.textContent = h; tr.appendChild(th); });\n\
      const thead = document.createElement('thead'); thead.appendChild(tr); t.appendChild(thead);\n\
    }\n\
    const tb = document.createElement('tbody');\n\
    it.rows.forEach(r => {\n\
      const tr = document.createElement('tr');\n\
      r.forEach(c => { const td = document.createElement('td'); td.textContent = c; tr.appendChild(td); });\n\
      tb.appendChild(tr);\n\
    });\n\
    t.appendChild(tb); el.appendChild(t);\n\
  } else if (it.pairs) {\n\
    const rows = it.pairs.slice(0, it.top || 20);\n\
    const maxv = Math.max(1, ...rows.map(p => p[1]));\n\
    rows.forEach(([k, v]) => el.appendChild(bar(k + '  ' + v, v / maxv * 100)));\n\
  } else {\n\
    const pre = document.createElement('pre'); pre.className = 'txt';\n\
    pre.textContent = it.text || ''; el.appendChild(pre);\n\
  }\n\
}\n\
function paint(items) {\n\
  items.forEach((it, i) => {\n\
    const el = document.getElementById('wb' + i);\n\
    if (el) render(el, it);\n\
  });\n\
  stat.textContent = 'live \\u00b7 ' + new Date().toLocaleTimeString();\n\
}\n\
let polling = false;\n\
async function tick() {\n\
  try {\n\
    const r = await fetch('/data', {cache: 'no-store'});\n\
    paint(await r.json());\n\
  } catch (e) { stat.textContent = 'disconnected'; }\n\
}\n\
function startPolling() {\n\
  if (polling) return; polling = true;\n\
  tick(); setInterval(tick, 500);\n\
}\n\
function connect() {\n\
  try {\n\
    const proto = location.protocol === 'https:' ? 'wss://' : 'ws://';\n\
    const ws = new WebSocket(proto + location.host + '/ws');\n\
    ws.onmessage = (ev) => paint(JSON.parse(ev.data));\n\
    ws.onerror = () => ws.close();\n\
    ws.onclose = () => startPolling();\n\
  } catch (e) { startPolling(); }\n\
}\n\
connect();\n";

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
    fn data_json_spark_carries_sparkline() {
        let spec = build(&parse("spark .s\nsource .s { in }").unwrap()).unwrap();
        let st = state_with(&["1", "2", "3", "4"]);
        let json: serde_json::Value = serde_json::from_str(&data_json(&spec, &st)).unwrap();
        assert_eq!(json[0]["kind"], "spark");
        // Rising series → first tick lowest, last tick highest.
        let spark = json[0]["spark"].as_str().unwrap();
        assert_eq!(spark.chars().count(), 4);
        assert!(spark.starts_with('▁') && spark.ends_with('█'));
    }

    #[test]
    fn data_json_chart_carries_series() {
        let spec = build(&parse("chart .c\nsource .c { in }").unwrap()).unwrap();
        let st = state_with(&["3", "1", "4", "1", "5"]);
        let json: serde_json::Value = serde_json::from_str(&data_json(&spec, &st)).unwrap();
        assert_eq!(json[0]["kind"], "chart");
        assert_eq!(json[0]["series"], serde_json::json!([3.0, 1.0, 4.0, 1.0, 5.0]));
    }

    #[test]
    fn data_json_table_carries_headers_and_rows() {
        let spec =
            build(&parse("table .t -cols \"a,b\"\nsource .t { in }").unwrap()).unwrap();
        let st = state_with(&["1 2", "3 4"]);
        let json: serde_json::Value = serde_json::from_str(&data_json(&spec, &st)).unwrap();
        assert_eq!(json[0]["kind"], "table");
        assert_eq!(json[0]["headers"], serde_json::json!(["a", "b"]));
        assert_eq!(json[0]["rows"], serde_json::json!([["1", "2"], ["3", "4"]]));
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

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sha1_matches_known_vectors() {
        // FIPS 180-1 / RFC 3174 test vectors.
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            hex(&sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn ws_accept_matches_rfc6455_example() {
        // RFC 6455 §1.3: key → accept token.
        assert_eq!(ws_accept("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn ws_text_frame_encodes_length() {
        // Short payload: FIN|text, 7-bit length, then bytes.
        assert_eq!(ws_text_frame("hi"), vec![0x81, 0x02, b'h', b'i']);
        // 200-byte payload uses the 16-bit extended length (0x7e marker).
        let big = "x".repeat(200);
        let f = ws_text_frame(&big);
        assert_eq!(&f[..4], &[0x81, 126, 0x00, 0xC8]);
        assert_eq!(f.len(), 4 + 200);
    }

    #[test]
    fn served_page_prefers_websocket_with_polling_fallback() {
        let spec = build(&parse("gauge .g -max 100").unwrap()).unwrap();
        let page = render_page(&spec);
        assert!(page.contains("new WebSocket"));
        assert!(page.contains("startPolling"));
        assert!(page.contains("/ws"));
    }

    #[test]
    fn widget_without_source_tails_latest_line() {
        let spec = build(&parse("text .t").unwrap()).unwrap();
        let val = widget_json(&spec.widgets[0], &["x".into(), "y".into()], 0.0);
        assert_eq!(val["text"], "y");
    }
}
