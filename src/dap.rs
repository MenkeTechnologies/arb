//! Debug Adapter (`arb --dap`): a handshake-only DAP stub over the same stdio
//! `Content-Length` framing as the LSP. arb specs describe a live dashboard, not
//! a stepping program — there is no runtime to set breakpoints in or step
//! through — so this answers the initialize/configuration handshake and exits
//! cleanly rather than faking a debugger. It exists so an editor's "attach a
//! debug adapter" path finds `arb --dap` and gets an honest capabilities reply.

use std::io;

use serde_json::{json, Value};

use crate::lsp::{read_message, write_message};

/// Handle one DAP request, returning the responses/events to send. `seq` is the
/// server's monotonically increasing sequence number (advanced per message).
pub fn handle(msg: &Value, seq: &mut i64) -> Vec<Value> {
    let command = msg.get("command").and_then(Value::as_str).unwrap_or("");
    let req_seq = msg.get("seq").and_then(Value::as_i64).unwrap_or(0);
    let mut next = || {
        *seq += 1;
        *seq
    };
    let response = |seq: i64, command: &str, success: bool, body: Value| {
        json!({
            "seq": seq, "type": "response", "request_seq": req_seq,
            "success": success, "command": command, "body": body,
        })
    };

    match command {
        "initialize" => {
            let resp = response(
                next(),
                "initialize",
                true,
                json!({ "supportsConfigurationDoneRequest": false }),
            );
            let event = json!({ "seq": next(), "type": "event", "event": "initialized" });
            vec![resp, event]
        }
        "disconnect" | "terminate" => {
            vec![response(next(), command, true, Value::Null)]
        }
        // arb has no stepping runtime; every other request is honestly unsupported.
        _ => vec![response(next(), command, false, json!({ "error": "arb specs are not steppable" }))],
    }
}

/// Run the DAP handshake stub over stdio until EOF, `disconnect`, or `terminate`.
pub fn run() {
    let stdin = io::stdin();
    let mut r = stdin.lock();
    let stdout = io::stdout();
    let mut w = stdout.lock();
    let mut seq: i64 = 0;
    while let Ok(Some(msg)) = read_message(&mut r) {
        let cmd = msg.get("command").and_then(Value::as_str).unwrap_or("");
        for out in handle(&msg, &mut seq) {
            if write_message(&mut w, &out).is_err() {
                return;
            }
        }
        if cmd == "disconnect" || cmd == "terminate" {
            return;
        }
    }
}
