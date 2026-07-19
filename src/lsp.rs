//! Language Server (`arb --lsp`): a std-only, no-async LSP over stdio JSON-RPC
//! for `.arb` specs. It parses + builds each open document and republishes the
//! parser/interpreter error as a diagnostic, plus `documentSymbol` (the widgets)
//! and `hover` (verb help). The transport shape is the standard `Content-Length`
//! framing; the language logic (`diagnose`/`handle`) is pure and unit-testable.
//!
//! Limitation: arb parse/build errors are position-less `String`s, so every
//! diagnostic anchors to the first line. Per-token ranges need source spans
//! threaded through the lexer/parser (out of scope here).

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

/// A single diagnostic: a message and the (line, col..col) range it covers.
#[derive(Debug, Clone, PartialEq)]
pub struct Diag {
    pub message: String,
    pub line: u32,
    pub start_col: u32,
    pub end_col: u32,
}

/// Parse + build `src`; map the first error (if any) to a diagnostic. Empty when
/// the spec is valid. Anchored to line 0 (arb errors carry no source span).
pub fn diagnose(src: &str) -> Vec<Diag> {
    let end_col = src.lines().next().map(|l| l.chars().count() as u32).unwrap_or(0);
    let one = |message: String| {
        vec![Diag { message, line: 0, start_col: 0, end_col: end_col.max(1) }]
    };
    let cmds = match crate::parser::parse(src) {
        Ok(c) => c,
        Err(m) => return one(m),
    };
    match crate::spec::build(&cmds) {
        Ok(_) => Vec::new(),
        Err(m) => one(m),
    }
}

/// The server's open-document store, keyed by URI.
#[derive(Default)]
pub struct Server {
    docs: HashMap<String, String>,
}

/// Handle one incoming message, returning the replies + notifications to send
/// (pure: no IO, so this is integration-testable). Requests echo their `id`.
pub fn handle(server: &mut Server, msg: &Value) -> Vec<Value> {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let id = msg.get("id").cloned();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let reply = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result });

    match method {
        "initialize" => vec![reply(json!({
            "capabilities": {
                "textDocumentSync": 1,          // Full sync
                "documentSymbolProvider": true,
                "hoverProvider": true,
            },
            "serverInfo": { "name": "arb", "version": env!("CARGO_PKG_VERSION") },
        }))],
        "initialized" | "exit" => vec![],
        "shutdown" => vec![reply(Value::Null)],
        "textDocument/didOpen" => {
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
            let text = params["textDocument"]["text"].as_str().unwrap_or("").to_string();
            let notif = publish_diagnostics(&uri, &text);
            server.docs.insert(uri, text);
            vec![notif]
        }
        "textDocument/didChange" => {
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
            // Full sync: the last content change carries the whole document.
            let text = params["contentChanges"]
                .as_array()
                .and_then(|a| a.last())
                .and_then(|c| c["text"].as_str())
                .unwrap_or("")
                .to_string();
            let notif = publish_diagnostics(&uri, &text);
            server.docs.insert(uri, text);
            vec![notif]
        }
        "textDocument/didClose" => {
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
            server.docs.remove(&uri);
            // Clear squiggles by publishing an empty set.
            vec![json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": { "uri": uri, "diagnostics": [] },
            })]
        }
        "textDocument/documentSymbol" => {
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
            let src = server.docs.get(uri).cloned().unwrap_or_default();
            vec![reply(json!(document_symbols(&src)))]
        }
        "textDocument/hover" => {
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
            let src = server.docs.get(uri).cloned().unwrap_or_default();
            let line = params["position"]["line"].as_u64().unwrap_or(0) as u32;
            let ch = params["position"]["character"].as_u64().unwrap_or(0) as u32;
            match hover(&src, line, ch) {
                Some(md) => vec![reply(json!({ "contents": { "kind": "markdown", "value": md } }))],
                None => vec![reply(Value::Null)],
            }
        }
        // Unknown request (has an id) -> MethodNotFound; notification -> ignore.
        _ => {
            if id.is_some() {
                vec![json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {method}") },
                })]
            } else {
                vec![]
            }
        }
    }
}

/// A `textDocument/publishDiagnostics` notification for `src`.
fn publish_diagnostics(uri: &str, src: &str) -> Value {
    let diags: Vec<Value> = diagnose(src).iter().map(diag_to_json).collect();
    json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": { "uri": uri, "diagnostics": diags },
    })
}

fn diag_to_json(d: &Diag) -> Value {
    json!({
        "range": {
            "start": { "line": d.line, "character": d.start_col },
            "end": { "line": d.line, "character": d.end_col },
        },
        "severity": 1, // Error
        "source": "arb",
        "message": d.message,
    })
}

/// Line-scan for widget declarations (`<verb> .path …`) -> LSP document symbols.
fn document_symbols(src: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let mut it = line.split_whitespace();
        let (Some(verb), Some(path)) = (it.next(), it.next()) else { continue };
        if crate::spec::WidgetKind::from(verb).is_some() && path.starts_with('.') {
            let len = line.chars().count() as u32;
            out.push(json!({
                "name": path,
                "detail": verb,
                "kind": 8, // SymbolKind.Field
                "range": symbol_range(i as u32, len),
                "selectionRange": symbol_range(i as u32, len),
            }));
        }
    }
    out
}

fn symbol_range(line: u32, len: u32) -> Value {
    json!({
        "start": { "line": line, "character": 0 },
        "end": { "line": line, "character": len },
    })
}

/// Help text for the verb under the cursor, as markdown, or `None`.
fn hover(src: &str, line: u32, ch: u32) -> Option<String> {
    let l = src.lines().nth(line as usize)?;
    let chars: Vec<char> = l.chars().collect();
    let ci = (ch as usize).min(chars.len());
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut start = ci;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = ci;
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    let word: String = chars[start..end].iter().collect();
    verb_help(&word).map(|h| format!("**`{word}`** — {h}"))
}

/// Static one-line help for a widget or query verb. The verb *set* comes from
/// `WidgetKind::from`; this table owns only the help *text*.
fn verb_help(word: &str) -> Option<&'static str> {
    let h = match word {
        "text" => "single-value text widget",
        "tail" => "scrolling tail of recent lines",
        "list" => "list of lines",
        "gauge" => "a bar toward `-max`",
        "bars" => "bar chart of (label, value) pairs",
        "histo" => "histogram (bar chart of counts)",
        "spark" => "unicode sparkline of a numeric series",
        "chart" => "line chart of a numeric series",
        "table" => "table (`-cols a,b,c`)",
        "tabs" => "labelled tab bar (`-tabs {a b}`)",
        "block" | "frame" => "a bordered container",
        "input" => "interactive text field driving `apply`/`out`",
        "select" => "fuzzy picker (fzf as a widget)",
        "source" => "bind a query pipeline to a widget: `source .w { in; … }`",
        "out" => "downstream passthrough pipeline (megafilter/map)",
        "grid" => "place a widget: `-row -col -span`",
        "bind" => "key binding: `bind C-x ACTION`",
        "expect" => "stream reaction: `expect /re/ ACTION`",
        "timeout" => "idle reaction: `timeout Ns ACTION`",
        "import" => "import a module/preset (`import X as Y`)",
        "where" => "filter by predicate (jq select)",
        "map" => "transform each value",
        "field" => "project a key/column",
        "each" => "iterate (jq [])",
        "count" => "count lines",
        "tally" => "count by value",
        "rate" => "lines per second",
        _ => return None,
    };
    Some(h)
}

/// Read one `Content-Length`-framed JSON-RPC message. `Ok(None)` on clean EOF.
pub fn read_message<R: BufRead>(r: &mut R) -> io::Result<Option<Value>> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(None); // EOF
        }
        let t = line.trim_end();
        if t.is_empty() {
            break; // end of headers
        }
        if let Some((k, v)) = t.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse::<usize>().ok();
            }
        }
    }
    let n = match len {
        Some(n) => n,
        None => return Ok(None),
    };
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf).unwrap_or(Value::Null)))
}

/// Write one `Content-Length`-framed JSON-RPC message (byte length, then flush).
pub fn write_message<W: Write>(w: &mut W, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg).unwrap_or_default();
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()
}

/// Run the LSP over stdio until EOF or `exit`.
pub fn run() {
    let stdin = io::stdin();
    let mut r = stdin.lock();
    let stdout = io::stdout();
    let mut w = stdout.lock();
    let mut server = Server::default();
    while let Ok(Some(msg)) = read_message(&mut r) {
        let is_exit = msg.get("method").and_then(Value::as_str) == Some("exit");
        for out in handle(&mut server, &msg) {
            if write_message(&mut w, &out).is_err() {
                return;
            }
        }
        if is_exit {
            return;
        }
    }
}
