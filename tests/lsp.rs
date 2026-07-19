//! LSP frontend tests — headless, CI-safe (pure `diagnose`/`handle`/framing, no
//! stdin/stdout/tty).

use arb::lsp::{diagnose, handle, read_message, write_message, Server};
use serde_json::{json, Value};

#[test]
fn diagnose_maps_parse_and_build_errors() {
    // Valid spec -> no diagnostics.
    assert!(diagnose("gauge .g -max 100").is_empty());
    assert!(diagnose("tail .t\nsource .t { in; count }").is_empty());
    // Build error: widget path must start with '.'.
    let d = diagnose("gauge foo");
    assert_eq!(d.len(), 1);
    assert!(d[0].message.contains("must start with"), "got: {}", d[0].message);
    assert_eq!(d[0].line, 0);
    // Build error: missing widget path.
    assert!(!diagnose("text").is_empty());
    // Parse error: unterminated block.
    assert!(!diagnose("text .t {").is_empty());
}

#[test]
fn framing_round_trips() {
    let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
    let mut buf: Vec<u8> = Vec::new();
    write_message(&mut buf, &msg).unwrap();
    // The header must be a byte Content-Length + CRLFCRLF.
    let head = String::from_utf8_lossy(&buf);
    assert!(head.starts_with("Content-Length: "));
    let mut slice = &buf[..];
    let got = read_message(&mut slice).unwrap().unwrap();
    assert_eq!(got, msg);
}

#[test]
fn initialize_advertises_capabilities() {
    let mut s = Server::default();
    let reply = handle(&mut s, &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }));
    assert_eq!(reply.len(), 1);
    let caps = &reply[0]["result"]["capabilities"];
    assert_eq!(caps["documentSymbolProvider"], true);
    assert_eq!(caps["hoverProvider"], true);
    assert_eq!(reply[0]["result"]["serverInfo"]["name"], "arb");
}

#[test]
fn didopen_bad_spec_publishes_diagnostics() {
    let mut s = Server::default();
    let out = handle(
        &mut s,
        &json!({
            "jsonrpc": "2.0", "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": "file:///x.arb", "text": "gauge foo" } },
        }),
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["method"], "textDocument/publishDiagnostics");
    let diags = out[0]["params"]["diagnostics"].as_array().unwrap();
    assert_eq!(diags.len(), 1);
    assert!(diags[0]["message"].as_str().unwrap().contains("must start with"));
}

#[test]
fn didopen_good_then_documentsymbol_lists_widgets() {
    let mut s = Server::default();
    handle(
        &mut s,
        &json!({
            "jsonrpc": "2.0", "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": "file:///y.arb", "text": "gauge .cpu -max 100\ntail .log" } },
        }),
    );
    let reply = handle(
        &mut s,
        &json!({
            "jsonrpc": "2.0", "id": 2, "method": "textDocument/documentSymbol",
            "params": { "textDocument": { "uri": "file:///y.arb" } },
        }),
    );
    let syms: &Vec<Value> = reply[0]["result"].as_array().unwrap();
    let names: Vec<&str> = syms.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(names.contains(&".cpu"));
    assert!(names.contains(&".log"));
}

#[test]
fn didchange_updates_stored_document() {
    let mut s = Server::default();
    handle(
        &mut s,
        &json!({
            "jsonrpc": "2.0", "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": "file:///z.arb", "text": "gauge .g" } },
        }),
    );
    // Change good -> bad; the follow-up diagnostics must be non-empty.
    let out = handle(
        &mut s,
        &json!({
            "jsonrpc": "2.0", "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": "file:///z.arb" },
                "contentChanges": [{ "text": "gauge bad" }],
            },
        }),
    );
    let diags = out[0]["params"]["diagnostics"].as_array().unwrap();
    assert!(!diags.is_empty());
}

#[test]
fn unknown_request_is_method_not_found() {
    let mut s = Server::default();
    let reply = handle(&mut s, &json!({ "jsonrpc": "2.0", "id": 9, "method": "textDocument/frobnicate" }));
    assert_eq!(reply[0]["error"]["code"], -32601);
    // Unknown notification (no id) is silently ignored.
    assert!(handle(&mut s, &json!({ "jsonrpc": "2.0", "method": "$/whatever" })).is_empty());
}

#[test]
fn dap_initialize_handshake_and_honest_unsupported() {
    let mut seq = 0i64;
    // initialize -> a success response + an `initialized` event.
    let out = arb::dap::handle(&json!({ "seq": 1, "type": "request", "command": "initialize" }), &mut seq);
    assert_eq!(out[0]["type"], "response");
    assert_eq!(out[0]["success"], true);
    assert_eq!(out[1]["event"], "initialized");
    // A stepping request is honestly unsupported (not faked).
    let step = arb::dap::handle(&json!({ "seq": 2, "type": "request", "command": "stepIn" }), &mut seq);
    assert_eq!(step[0]["success"], false);
}
