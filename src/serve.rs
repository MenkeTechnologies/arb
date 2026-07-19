//! Web target: serve the same [`Spec`] as a live browser dashboard. A std-only
//! HTTP server (no async runtime, no framework) binds a local port, serves one
//! page at `/` built from the vendored `zgui-core` toolkit, and answers
//! `GET /data` (and a `/ws` push) with the current widget values as JSON — so the
//! same spec that drives the ratatui TUI drives a browser, live.
//!
//! Interactive: `input` widgets `POST /set?name=..&value=..`; the server holds a
//! live input store and re-resolves each widget's pipeline against it every
//! frame, so a typed field reshapes the dashboard like the TUI megafilter.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use percent_encoding::percent_decode_str;

use crate::query::{eval, QueryResult};
use crate::spec::{resolve_pipeline, Spec, Widget, WidgetKind};
use crate::stream::StreamState;

/// Live control values (`input .name` widgets) the browser writes via `POST
/// /set`; every `/data` and `/ws` frame re-resolves each widget's pipeline
/// against this store, so a typed field reshapes the dashboard live.
type Inputs = Arc<Mutex<HashMap<String, String>>>;
/// The vendored `zgui-core` toolkit, bundled at build time (see `build.rs`) from
/// the `lib/zgui-core` submodule. Served as `/zgui.js` + `/zgui.css` and driven
/// by the dashboard page. Empty if the submodule was not checked out.
const ZGUI_JS: &str = include_str!(concat!(env!("OUT_DIR"), "/zgui_bundle.js"));
const ZGUI_CSS: &str = include_str!(concat!(env!("OUT_DIR"), "/zgui_bundle.css"));

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
    // Live input store, seeded like the TUI (main.rs): every control widget ->
    // its initial value (slider = min, check = "0", else "").
    let inputs: Inputs = Arc::new(Mutex::new(
        spec.widgets
            .iter()
            .filter(|w| w.kind.is_control())
            .map(|w| {
                let name = w.path.trim_start_matches('.').to_string();
                let init = match w.kind {
                    WidgetKind::Slider => crate::query::fmt_scalar(crate::spec::parse_scalar(
                        w.opts.get("min").map(String::as_str).unwrap_or("0"),
                    )),
                    WidgetKind::Check => "0".to_string(),
                    _ => String::new(),
                };
                (name, init)
            })
            .collect(),
    ));
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let (spec, state, page, inputs) =
            (spec.clone(), state.clone(), page.clone(), inputs.clone());
        // One thread per connection so a slow/holding client never blocks others.
        thread::spawn(move || {
            let _ = handle(conn, &spec, &state, &page, &inputs);
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
    inputs: &Inputs,
) -> std::io::Result<()> {
    // Read + write timeouts so a client that stalls cannot pin this connection's
    // thread forever: the read timeout covers a slowloris that never finishes its
    // request; the write timeout covers a client that stops reading, so the
    // response `write_all` (and the 250ms `/ws` push loop) unblocks and the thread
    // unwinds instead of blocking on a full socket send buffer indefinitely.
    let _ = conn.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = conn.set_write_timeout(Some(Duration::from_secs(15)));
    // Read the request line + headers (needed for the WebSocket upgrade key).
    // Cap the total bytes buffered for the request line + headers: an attacker
    // streaming megabytes with no newline would otherwise grow `read_line`'s
    // String without bound (memory DoS). 64 KiB is far above any real header set.
    const MAX_REQ_BYTES: u64 = 64 * 1024;
    let (path, headers) = {
        let mut reader = BufReader::new((&conn).take(MAX_REQ_BYTES));
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
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    };
    // `GET /ws` with a WebSocket key: upgrade and push updates (no more polling).
    if path == "/ws" {
        if let Some(key) = header("Sec-WebSocket-Key") {
            return ws_serve(conn, spec, state, inputs, &key);
        }
    }
    // `POST /set?name=..&value=..`: a browser control writes a live input value.
    // Data rides in the request-line query, so no request-body/masked-frame read.
    if let Some(q) = path.strip_prefix("/set?") {
        let (name, value) = parse_set_query(q);
        // Only a declared control (seeded into the store at startup) may be
        // written — update in place rather than insert, so an undeclared name is
        // ignored and a client cannot grow the map without bound.
        if !name.is_empty() {
            if let Some(slot) = inputs.lock().unwrap().get_mut(&name) {
                *slot = value;
            }
        }
        return conn.write_all(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
    }
    let snap = inputs.lock().unwrap().clone();
    let (ctype, body) = match path.as_str() {
        "/data" => ("application/json", data_json(spec, state, &snap)),
        "/zgui.js" => ("application/javascript; charset=utf-8", ZGUI_JS.to_string()),
        "/zgui.css" => ("text/css; charset=utf-8", ZGUI_CSS.to_string()),
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
    inputs: &Inputs,
    key: &str,
) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        ws_accept(key)
    );
    conn.write_all(resp.as_bytes())?;
    loop {
        // Snapshot the live input store each tick (released before write/sleep),
        // so a `POST /set` between ticks shows on the next push.
        let snap = inputs.lock().unwrap().clone();
        let frame = ws_text_frame(&data_json(spec, state, &snap));
        if conn.write_all(&frame).is_err() {
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

/// Parse a `POST /set?name=..&value=..` query string, percent-decoding both.
/// `encodeURIComponent` on the client escapes literal `&`/`=` in the value, so
/// the `&`/`=` split is unambiguous.
fn parse_set_query(q: &str) -> (String, String) {
    let dec = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
    let (mut name, mut value) = (String::new(), String::new());
    for kv in q.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            match k {
                "name" => name = dec(v),
                "value" => value = dec(v),
                _ => {}
            }
        }
    }
    (name, value)
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
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
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
            *word = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
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
fn data_json(
    spec: &Spec,
    state: &Arc<Mutex<StreamState>>,
    inputs: &HashMap<String, String>,
) -> String {
    let (raw, elapsed): (Vec<String>, f64) = {
        let st = state.lock().unwrap();
        (
            st.lines.iter().cloned().collect(),
            st.start.elapsed().as_secs_f64(),
        )
    };
    let items: Vec<serde_json::Value> = spec
        .widgets
        .iter()
        .map(|w| widget_json(w, &raw, elapsed, inputs))
        .collect();
    serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string())
}

/// One widget's data as JSON, shaped by kind so the client renders a bar/gauge/
/// text rather than a flat string. `inputs` are the live control values, so a
/// bound `apply .name` / `where … .name` reflects what the browser typed.
fn widget_json(
    w: &Widget,
    raw: &[String],
    elapsed: f64,
    inputs: &HashMap<String, String>,
) -> serde_json::Value {
    use serde_json::json;
    let color = crate::spec::color_hex(w.opts.get("color").map(String::as_str));
    let base = |extra: serde_json::Value| {
        let mut m = json!({ "path": w.path, "kind": w.kind.label(), "color": color });
        if let (Some(obj), Some(ex)) = (m.as_object_mut(), extra.as_object()) {
            for (k, v) in ex {
                obj.insert(k.clone(), v.clone());
            }
        }
        m
    };
    let opt_f64 = |k: &str, d: f64| {
        w.opts
            .get(k)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(d)
    };
    // Resolve control placeholders against live input values (mirrors tui.rs).
    let result = w
        .source
        .as_ref()
        .map(|s| eval(&resolve_pipeline(&s.pipeline, inputs), raw, elapsed));
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
            let top = w
                .opts
                .get("top")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(20);
            base(json!({ "pairs": pairs, "top": top }))
        }
        WidgetKind::Spark => {
            // Raw numeric series — the client renders it with `ZGui.sparkline`.
            let series: Vec<f64> = match &result {
                Some(QueryResult::Pairs(p)) => p.iter().map(|(_, v)| *v as f64).collect(),
                Some(QueryResult::Lines(ls)) => crate::query::numeric_series(ls),
                Some(QueryResult::Scalar(v)) => vec![*v],
                None => crate::query::numeric_series(raw),
            };
            let n = series.len();
            let series = &series[n.saturating_sub(400)..]; // cap points per poll
            base(json!({ "series": series, "spark": true }))
        }
        WidgetKind::Tabs => {
            // Tab labels from `-tabs {a b}` (stored comma-joined) for a web tab bar.
            let tabs: Vec<&str> = w
                .opts
                .get("tabs")
                .map(|s| s.split(',').filter(|t| !t.is_empty()).collect())
                .unwrap_or_default();
            base(json!({ "tabs": tabs }))
        }
        WidgetKind::Input => {
            // A live editable field: the client POSTs /set on change.
            let name = w.path.trim_start_matches('.');
            base(json!({
                "name": name,
                "value": inputs.get(name).cloned().unwrap_or_default(),
                "placeholder": w.opts.get("placeholder").or_else(|| w.opts.get("title")).cloned().unwrap_or_default(),
                "input": true,
            }))
        }
        WidgetKind::Filter => {
            // Like Input, but carries a label; drives `where match(.name)`.
            let name = w.path.trim_start_matches('.');
            base(json!({
                "name": name,
                "value": inputs.get(name).cloned().unwrap_or_default(),
                "placeholder": w.opts.get("placeholder").or_else(|| w.opts.get("label")).or_else(|| w.opts.get("title")).cloned().unwrap_or_default(),
                "filter": true,
            }))
        }
        WidgetKind::Check => {
            // Boolean: `value` as a bool for the checkbox; the client POSTs "1"/"0".
            let name = w.path.trim_start_matches('.');
            base(json!({
                "name": name,
                "value": inputs.get(name).map(String::as_str).unwrap_or("0") == "1",
                "label": w.opts.get("label").or_else(|| w.opts.get("title")).map(String::as_str).unwrap_or(name),
                "check": true,
            }))
        }
        WidgetKind::Slider => {
            // Range: parse_scalar so durations/sizes (5s, 4mb) resolve like the TUI.
            let name = w.path.trim_start_matches('.');
            let min =
                crate::spec::parse_scalar(w.opts.get("min").map(String::as_str).unwrap_or("0"));
            let max = w
                .opts
                .get("max")
                .map(|s| crate::spec::parse_scalar(s))
                .unwrap_or(100.0);
            let step = w
                .opts
                .get("step")
                .map(|s| crate::spec::parse_scalar(s))
                .unwrap_or(1.0);
            let value = inputs
                .get(name)
                .and_then(|v| v.trim().parse::<f64>().ok())
                .unwrap_or(min);
            base(json!({
                "name": name, "value": value, "min": min, "max": max,
                "step": if step > 0.0 { step } else { 1.0 },
                "label": w.opts.get("label").or_else(|| w.opts.get("title")).map(String::as_str).unwrap_or(name),
                "slider": true,
            }))
        }
        WidgetKind::Facet => {
            // Candidates: -opts, else server-computed distinct -field values over
            // the live stream. selected = the current comma-set.
            let name = w.path.trim_start_matches('.');
            let cands = crate::tui::facet_candidates(w, raw);
            let sel = inputs.get(name).map(String::as_str).unwrap_or("");
            let selected: Vec<&str> = sel
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            base(json!({
                "name": name, "opts": cands, "selected": selected,
                "label": w.opts.get("label").or_else(|| w.opts.get("title")).map(String::as_str).unwrap_or(name),
                "facet": true,
            }))
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
            // `-limit N` caps the rows shown; otherwise the last 200 so a huge
            // stream doesn't bloat each poll.
            let cap = crate::tui::widget_limit(w).map_or(200, |n| n.min(200));
            let n = ls.len();
            ls[n.saturating_sub(cap)..].join("\n")
        }
        QueryResult::Scalar(v) => format!("{v:.2}"),
        QueryResult::Pairs(ps) => ps
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// The dashboard page: it loads the bundled `zgui-core` toolkit (`/zgui.css` +
/// `/zgui.js`), mounts `ZGui.appShell` (the standard cyberpunk chrome — splash,
/// filter bar, ⌘K palette, settings/colorscheme), and builds one `ZGui.*`
/// component per widget, fed live from `/data` (or the `/ws` push). The widget
/// list is injected as `window.ARB_META`.
fn render_page(spec: &Spec) -> String {
    use serde_json::json;
    let widgets: Vec<serde_json::Value> = spec
        .widgets
        .iter()
        .map(|w| {
            let name = w
                .opts
                .get("label")
                .or_else(|| w.opts.get("title"))
                .map(String::as_str)
                .unwrap_or(&w.path);
            json!({ "name": name, "kind": w.kind.label() })
        })
        .collect();
    // Embed the metadata as JSON; neutralize any `</script>` inside a widget
    // name so it can't break out of the inline script.
    let meta = json!({ "title": "dashboard", "widgets": widgets })
        .to_string()
        .replace("</", "<\\/");
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>arb dashboard</title>\n\
         <link rel=\"stylesheet\" href=\"/zgui.css\">\n\
         <style>\n{ARB_CSS}</style>\n\
         </head>\n<body>\n\
         <div id=\"app\"></div>\n\
         <script src=\"/zgui.js\"></script>\n\
         <script>window.ARB_META = {meta};</script>\n\
         <script>\n{ZINIT}</script>\n\
         </body>\n</html>\n"
    )
}

/// arb-specific layout on top of the zgui theme: the widget grid and the simple
/// tab strip (`ZGui.*` components style themselves via the bundled stylesheet).
const ARB_CSS: &str = "\
html, body { height: 100%; margin: 0; }\n\
.arb-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(320px, 1fr)); gap: 12px; padding: 12px; }\n\
.arb-grid > * { min-width: 0; }\n\
.arb-tabs { display: flex; gap: 4px; flex-wrap: wrap; }\n\
.arb-tab { padding: 2px 10px; border: 1px solid var(--border, #333); border-radius: 3px; font-size: 0.9em; }\n\
.arb-tab.sel { background: var(--cyan, #05d9e8); color: #000; font-weight: bold; }\n\
.arb-ctl { display: flex; align-items: center; gap: 8px; padding: 4px 0; }\n\
.arb-ctl-label { font-size: 0.85em; color: var(--fg-dim, #9e9e9e); }\n\
.arb-ctl-val { font-variant-numeric: tabular-nums; min-width: 3ch; text-align: right; }\n\
.zg-range { flex: 1; accent-color: var(--cyan, #05d9e8); }\n\
.arb-check input, .arb-facet-opt input { accent-color: var(--cyan, #05d9e8); }\n\
.arb-facet { display: flex; flex-direction: column; gap: 2px; }\n\
.arb-facet-opt { display: flex; align-items: center; gap: 6px; font-size: 0.9em; cursor: pointer; }\n";

/// Client init: mount `ZGui.appShell`, build one `ZGui.*` component per widget
/// (from `window.ARB_META`), and drive them from the `/ws` push (falling back to
/// polling `/data`). All data reaches components through their handles / DOM
/// `textContent`, never `innerHTML`, so stream data can't inject markup.
const ZINIT: &str = r#"
(function () {
  "use strict";
  var app = document.getElementById('app');
  if (!window.ZGui || !ZGui.appShell) {
    app.textContent = 'zgui-core not bundled — run: git submodule update --init, then rebuild';
    return;
  }
  var meta = window.ARB_META || { widgets: [] };
  var slots = [];
  var shell = ZGui.appShell(app, {
    brand: { glyph: '◆', title: 'arb', subtitle: meta.title || 'dashboard' },
    filterPlaceholder: 'Filter widgets…',
    onFilter: function (q) {
      q = (q || '').toLowerCase();
      slots.forEach(function (s) {
        var hay = (s.w.name + ' ' + s.w.kind).toLowerCase();
        s.card.el.style.display = (!q || hay.indexOf(q) >= 0) ? '' : 'none';
      });
    }
  });
  var grid = document.createElement('div');
  grid.className = 'arb-grid';
  shell.body.appendChild(grid);

  function pairsRows(p) { return (p || []).map(function (x) { return { label: String(x[0]), value: Number(x[1]) || 0 }; }); }
  function cols(h) { return (h || []).map(function (x, i) { return { key: 'c' + i, label: x }; }); }
  function tblRows(h, r) { return (r || []).map(function (row) { var o = {}; (row || []).forEach(function (c, i) { o['c' + i] = c; }); return o; }); }
  function postSet(name, val) {
    fetch('/set?name=' + encodeURIComponent(name) + '&value=' + encodeURIComponent(val), { method: 'POST' }).catch(function () {});
  }

  slots = (meta.widgets || []).map(function (w) {
    var card = ZGui.card({ title: w.name + '  ·  ' + w.kind });
    grid.appendChild(card.el);
    return { w: w, host: card.body, card: card, h: null };
  });

  // Build the right ZGui component for a widget the first time its data arrives;
  // returns an update(data) closure.
  function build(slot, it) {
    var host = slot.host, k = it.kind, color = it.color;
    if (k === 'gauge') {
      var g = ZGui.gauge({ value: 0, min: 0, max: it.max || 100, label: slot.w.name, color: color });
      host.appendChild(g.el);
      return function (d) { g.set(d.scalar || 0); };
    }
    if (k === 'chart') {
      var c = ZGui.chart(host, { series: [{ data: it.series || [], color: color, type: 'line' }], height: 120, grid: true, axes: true });
      return function (d) { c.setSeries([{ data: d.series || [], color: color, type: 'line' }]); };
    }
    if (k === 'spark') {
      var s = ZGui.sparkline(host, it.series || [], { type: 'line', color: color });
      return function (d) { s.set(d.series || []); };
    }
    if (k === 'bars' || k === 'histo') {
      var b = ZGui.statBars(host, pairsRows(it.pairs), {});
      return function (d) { b.set(pairsRows(d.pairs)); };
    }
    if (k === 'table') {
      var t = ZGui.dataTable(host, { columns: cols(it.headers), rows: tblRows(it.headers, it.rows) });
      return function (d) { t.setRows(tblRows(d.headers, d.rows)); };
    }
    if (k === 'tabs') {
      var bar = document.createElement('div'); bar.className = 'arb-tabs'; host.appendChild(bar);
      return function (d) {
        bar.replaceChildren();
        (d.tabs || []).forEach(function (t, i) {
          var sp = document.createElement('span');
          sp.className = 'arb-tab' + (i === 0 ? ' sel' : '');
          sp.textContent = t; bar.appendChild(sp);
        });
      };
    }
    if (k === 'input' || k === 'filter') {
      // A live editable field: debounced POST /set on change reshapes the pipe.
      var inp = document.createElement('input');
      inp.type = 'text'; inp.className = 'zg-input';
      inp.placeholder = it.placeholder || slot.w.name;
      inp.value = it.value || '';
      var tmr = null;
      inp.addEventListener('input', function () {
        clearTimeout(tmr);
        var name = it.name, val = inp.value;
        tmr = setTimeout(function () { postSet(name, val); }, 120);
      });
      if (k === 'filter' && it.placeholder) {
        var wrap = document.createElement('label'); wrap.className = 'arb-ctl';
        var lb = document.createElement('span'); lb.className = 'arb-ctl-label'; lb.textContent = it.placeholder;
        wrap.appendChild(lb); wrap.appendChild(inp); host.appendChild(wrap);
      } else { host.appendChild(inp); }
      // Don't clobber the field the user is editing when /data refreshes.
      return function (d) { if (document.activeElement !== inp && d.value != null) inp.value = d.value; };
    }
    if (k === 'slider') {
      var sw = document.createElement('label'); sw.className = 'arb-ctl';
      var sl = document.createElement('span'); sl.className = 'arb-ctl-label'; sl.textContent = it.label || slot.w.name;
      var rng = document.createElement('input'); rng.type = 'range'; rng.className = 'zg-range';
      rng.min = it.min; rng.max = it.max; rng.step = it.step; rng.value = it.value;
      var ov = document.createElement('span'); ov.className = 'arb-ctl-val'; ov.textContent = it.value;
      var st = null;
      rng.addEventListener('input', function () {
        ov.textContent = rng.value;
        clearTimeout(st); var name = it.name, val = rng.value;
        st = setTimeout(function () { postSet(name, val); }, 120);
      });
      sw.appendChild(sl); sw.appendChild(rng); sw.appendChild(ov); host.appendChild(sw);
      return function (d) { if (document.activeElement !== rng && d.value != null) { rng.value = d.value; ov.textContent = d.value; } };
    }
    if (k === 'check') {
      var cw = document.createElement('label'); cw.className = 'arb-ctl arb-check';
      var cb = document.createElement('input'); cb.type = 'checkbox'; cb.checked = !!it.value;
      var ct = document.createElement('span'); ct.className = 'arb-ctl-label'; ct.textContent = it.label || slot.w.name;
      cb.addEventListener('change', function () { postSet(it.name, cb.checked ? '1' : '0'); });
      cw.appendChild(cb); cw.appendChild(ct); host.appendChild(cw);
      return function (d) { if (document.activeElement !== cb && d.value != null) cb.checked = !!d.value; };
    }
    if (k === 'facet') {
      var fw = document.createElement('div'); fw.className = 'arb-facet';
      if (it.label) { var fl = document.createElement('div'); fl.className = 'arb-ctl-label'; fl.textContent = it.label; fw.appendChild(fl); }
      var boxes = {};
      function rebuild(opts, selected) {
        Array.prototype.slice.call(fw.querySelectorAll('.arb-facet-opt')).forEach(function (n) { n.remove(); });
        boxes = {};
        (opts || []).forEach(function (opt) {
          var row = document.createElement('label'); row.className = 'arb-facet-opt';
          var b = document.createElement('input'); b.type = 'checkbox'; b.checked = (selected || []).indexOf(opt) >= 0;
          b.addEventListener('change', function () {
            var set = []; Object.keys(boxes).forEach(function (o) { if (boxes[o].checked) set.push(o); });
            postSet(it.name, set.join(','));
          });
          boxes[opt] = b;
          var t = document.createElement('span'); t.textContent = opt;
          row.appendChild(b); row.appendChild(t); fw.appendChild(row);
        });
      }
      var optsKey = (it.opts || []).join('');
      rebuild(it.opts, it.selected || []); host.appendChild(fw);
      return function (d) {
        // -field candidates grow as the stream flows: rebuild only when the option
        // list changes, and never while the user is toggling a box.
        if (document.activeElement && fw.contains(document.activeElement)) return;
        var key = (d.opts || []).join('');
        if (key !== optsKey) { optsKey = key; rebuild(d.opts, d.selected || []); }
      };
    }
    // text / tail / list / block / frame / select -> a streaming log view.
    var lv = ZGui.logView.create(host, { maxLines: 2000, ansi: true });
    var last = null;
    return function (d) {
      var txt = d.text || '';
      if (txt !== last) { lv.clear(); if (txt) lv.append(txt); last = txt; }
    };
  }

  function paint(items) {
    (items || []).forEach(function (it, i) {
      var slot = slots[i];
      if (!slot) return;
      if (!slot.h) slot.h = build(slot, it);
      slot.h(it);
    });
  }

  var polling = false;
  function tick() { fetch('/data', { cache: 'no-store' }).then(function (r) { return r.json(); }).then(paint).catch(function () {}); }
  function startPolling() { if (polling) return; polling = true; tick(); setInterval(tick, 500); }
  function connect() {
    try {
      var proto = location.protocol === 'https:' ? 'wss://' : 'ws://';
      var ws = new WebSocket(proto + location.host + '/ws');
      ws.onmessage = function (ev) { try { paint(JSON.parse(ev.data)); } catch (e) {} };
      ws.onerror = function () { ws.close(); };
      ws.onclose = function () { startPolling(); };
    } catch (e) { startPolling(); }
  }
  connect();
})();
"#;

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
            &parse("gauge .g -max 100\nsource .g { in; count }\nlist .l\nsource .l { in }")
                .unwrap(),
        )
        .unwrap();
        let st = state_with(&["a", "b", "c"]);
        let json: serde_json::Value =
            serde_json::from_str(&data_json(&spec, &st, &HashMap::new())).unwrap();
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
        let spec = build(&parse("histo .h\nsource .h { in; tally }").unwrap()).unwrap();
        let st = state_with(&["x", "y", "x", "x", "y"]);
        let json: serde_json::Value =
            serde_json::from_str(&data_json(&spec, &st, &HashMap::new())).unwrap();
        let pairs = json[0]["pairs"].as_array().unwrap();
        // tally counts occurrences → x:3, y:2 (order is count-desc).
        assert_eq!(pairs[0][0], "x");
        assert_eq!(pairs[0][1], 3);
        assert_eq!(pairs[1][0], "y");
        assert_eq!(pairs[1][1], 2);
    }

    #[test]
    fn data_json_spark_carries_series() {
        let spec = build(&parse("spark .s\nsource .s { in }").unwrap()).unwrap();
        let st = state_with(&["1", "2", "3", "4"]);
        let json: serde_json::Value =
            serde_json::from_str(&data_json(&spec, &st, &HashMap::new())).unwrap();
        assert_eq!(json[0]["kind"], "spark");
        // Raw numeric series — the client renders it with ZGui.sparkline.
        assert_eq!(json[0]["series"], serde_json::json!([1.0, 2.0, 3.0, 4.0]));
    }

    #[test]
    fn data_json_chart_carries_series() {
        let spec = build(&parse("chart .c\nsource .c { in }").unwrap()).unwrap();
        let st = state_with(&["3", "1", "4", "1", "5"]);
        let json: serde_json::Value =
            serde_json::from_str(&data_json(&spec, &st, &HashMap::new())).unwrap();
        assert_eq!(json[0]["kind"], "chart");
        assert_eq!(
            json[0]["series"],
            serde_json::json!([3.0, 1.0, 4.0, 1.0, 5.0])
        );
    }

    #[test]
    fn data_json_table_carries_headers_and_rows() {
        let spec = build(&parse("table .t -cols \"a,b\"\nsource .t { in }").unwrap()).unwrap();
        let st = state_with(&["1 2", "3 4"]);
        let json: serde_json::Value =
            serde_json::from_str(&data_json(&spec, &st, &HashMap::new())).unwrap();
        assert_eq!(json[0]["kind"], "table");
        assert_eq!(json[0]["headers"], serde_json::json!(["a", "b"]));
        assert_eq!(json[0]["rows"], serde_json::json!([["1", "2"], ["3", "4"]]));
    }

    #[test]
    fn list_limit_caps_rows() {
        let spec = build(&parse("list .l -limit 3\nsource .l { in }").unwrap()).unwrap();
        let st = state_with(&["1", "2", "3", "4", "5"]);
        let json: serde_json::Value =
            serde_json::from_str(&data_json(&spec, &st, &HashMap::new())).unwrap();
        // Only the last 3 lines are shown.
        assert_eq!(json[0]["text"], "3\n4\n5");
    }

    #[test]
    fn render_page_uses_label_over_path() {
        let spec = build(&parse("gauge .cpu -max 100 -label \"CPU %\"").unwrap()).unwrap();
        let page = render_page(&spec);
        assert!(page.contains("CPU %"));
        // The raw dot-path is no longer shown in the header once a label is set.
        assert!(!page.contains(">.cpu<"));
    }

    #[test]
    fn render_page_mounts_zgui_appshell_with_widget_meta() {
        let spec = build(&parse("gauge .g -max 100\nlist .l").unwrap()).unwrap();
        let page = render_page(&spec);
        assert!(page.contains("<title>arb dashboard</title>"));
        // Loads the bundled zgui toolkit and mounts the standard shell.
        assert!(page.contains("/zgui.js"));
        assert!(page.contains("/zgui.css"));
        assert!(page.contains("ZGui.appShell"));
        // Widget metadata is injected for the client to build components from.
        assert!(page.contains("ARB_META"));
        assert!(page.contains("\"kind\":\"gauge\""));
        assert!(page.contains("\"kind\":\"list\""));
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sha1_matches_known_vectors() {
        // FIPS 180-1 / RFC 3174 test vectors.
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn ws_accept_matches_rfc6455_example() {
        // RFC 6455 §1.3: key → accept token.
        assert_eq!(
            ws_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
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
        let val = widget_json(
            &spec.widgets[0],
            &["x".into(), "y".into()],
            0.0,
            &HashMap::new(),
        );
        assert_eq!(val["text"], "y");
    }

    #[test]
    fn parse_set_query_percent_decodes() {
        // %20 -> space, %26 -> & survive the &/= split of the query string.
        assert_eq!(
            parse_set_query("name=q&value=a%20b%26c"),
            ("q".into(), "a b&c".into())
        );
        assert_eq!(parse_set_query("value=v&name=n"), ("n".into(), "v".into()));
    }

    #[test]
    fn widget_json_input_carries_value_and_placeholder() {
        let spec = build(&parse("input .q -placeholder P").unwrap()).unwrap();
        let mut m = HashMap::new();
        m.insert("q".to_string(), "hi".to_string());
        let v = widget_json(&spec.widgets[0], &[], 0.0, &m);
        assert_eq!(v["input"], true);
        assert_eq!(v["value"], "hi");
        assert_eq!(v["placeholder"], "P");
    }

    #[test]
    fn set_route_value_resolves_apply_in_data_json() {
        // The client->server->dashboard loop, minus the socket: a POSTed input
        // value flows through resolve_pipeline into the served /data.
        let spec = build(&parse("list .l\nsource .l { in; apply .q }").unwrap()).unwrap();
        let st = state_with(&["1", "2", "3", "4"]);
        // Empty store -> apply is a no-op, all lines pass.
        let j0: serde_json::Value =
            serde_json::from_str(&data_json(&spec, &st, &HashMap::new())).unwrap();
        assert_eq!(j0[0]["text"], "1\n2\n3\n4");
        // Simulate POST /set?name=q&value=over 2, then re-resolve.
        let (name, value) = parse_set_query("name=q&value=over%202");
        let mut store = HashMap::new();
        store.insert(name, value);
        let j1: serde_json::Value = serde_json::from_str(&data_json(&spec, &st, &store)).unwrap();
        assert_eq!(j1[0]["text"], "3\n4");
    }

    #[test]
    fn served_page_wires_input_post_set() {
        let spec = build(&parse("input .q").unwrap()).unwrap();
        let page = render_page(&spec);
        assert!(page.contains("/set"));
        assert!(page.contains("addEventListener"));
        assert!(page.contains("zg-input"));
    }

    #[test]
    fn widget_json_control_kinds_carry_state() {
        // filter
        let s = build(&parse("filter .q -placeholder P").unwrap()).unwrap();
        let mut m = HashMap::new();
        m.insert("q".to_string(), "hi".to_string());
        let v = widget_json(&s.widgets[0], &[], 0.0, &m);
        assert_eq!(v["filter"], true);
        assert_eq!(v["value"], "hi");
        assert_eq!(v["placeholder"], "P");
        // check
        let s = build(&parse("check .c -label On").unwrap()).unwrap();
        let v = widget_json(&s.widgets[0], &[], 0.0, &HashMap::new());
        assert_eq!(v["check"], true);
        assert_eq!(v["value"], false);
        assert_eq!(v["label"], "On");
        let mut m = HashMap::new();
        m.insert("c".to_string(), "1".to_string());
        assert_eq!(widget_json(&s.widgets[0], &[], 0.0, &m)["value"], true);
        // slider
        let s = build(&parse("slider .th -min 0 -max 10 -step 2").unwrap()).unwrap();
        let mut m = HashMap::new();
        m.insert("th".to_string(), "4".to_string());
        let v = widget_json(&s.widgets[0], &[], 0.0, &m);
        assert_eq!(v["slider"], true);
        assert_eq!(v["value"], 4.0);
        assert_eq!(v["min"], 0.0);
        assert_eq!(v["max"], 10.0);
        assert_eq!(v["step"], 2.0);
        // facet: static -opts + selected
        let s = build(&parse("facet .lv -opts {a b c}").unwrap()).unwrap();
        let mut m = HashMap::new();
        m.insert("lv".to_string(), "a,c".to_string());
        let v = widget_json(&s.widgets[0], &[], 0.0, &m);
        assert_eq!(v["facet"], true);
        assert_eq!(v["opts"], serde_json::json!(["a", "b", "c"]));
        assert_eq!(v["selected"], serde_json::json!(["a", "c"]));
    }

    #[test]
    fn widget_json_facet_distinct_field_candidates() {
        // No -opts: candidates are distinct -field values over the raw stream.
        let s = build(&parse("facet .lv -field level").unwrap()).unwrap();
        let raw: Vec<String> = ["INFO", "WARN", "INFO"]
            .iter()
            .map(|l| format!(r#"{{"level":"{l}"}}"#))
            .collect();
        let v = widget_json(&s.widgets[0], &raw, 0.0, &HashMap::new());
        assert_eq!(v["opts"], serde_json::json!(["INFO", "WARN"]));
    }

    #[test]
    fn set_route_slider_and_facet_resolve_in_data_json() {
        // Slider -> numeric where.
        let s = build(&parse("list .l\nsource .l { in; where x < .th }").unwrap()).unwrap();
        let st = state_with(&["1", "2", "3", "4", "5"]);
        let (n, val) = parse_set_query("name=th&value=3");
        let mut store = HashMap::new();
        store.insert(n, val);
        let j: serde_json::Value = serde_json::from_str(&data_json(&s, &st, &store)).unwrap();
        assert_eq!(j[0]["text"], "1\n2");
        // Facet -> `where field in .set`.
        let s = build(&parse("list .l\nsource .l { in; where level in .sel }").unwrap()).unwrap();
        let st = state_with(&[
            r#"{"level":"INFO"}"#,
            r#"{"level":"WARN"}"#,
            r#"{"level":"INFO"}"#,
        ]);
        // Empty selection -> all pass.
        let j0: serde_json::Value =
            serde_json::from_str(&data_json(&s, &st, &HashMap::new())).unwrap();
        assert_eq!(j0[0]["text"].as_str().unwrap().lines().count(), 3);
        let (n, val) = parse_set_query("name=sel&value=INFO");
        let mut store = HashMap::new();
        store.insert(n, val);
        let j1: serde_json::Value = serde_json::from_str(&data_json(&s, &st, &store)).unwrap();
        assert_eq!(j1[0]["text"].as_str().unwrap().lines().count(), 2);
    }

    #[test]
    fn served_page_wires_all_controls() {
        let spec = build(
            &parse("filter .q\nslider .s -min 0 -max 9\ncheck .c\nfacet .f -opts {a b}").unwrap(),
        )
        .unwrap();
        let page = render_page(&spec);
        assert!(page.contains("postSet"));
        assert!(page.contains("'range'"));
        assert!(page.contains("'checkbox'"));
        assert!(page.contains("arb-facet"));
        assert!(page.contains("k === 'slider'"));
        assert!(page.contains("k === 'facet'"));
    }
}
