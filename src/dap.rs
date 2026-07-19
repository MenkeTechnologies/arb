//! Debug Adapter (`arb --dap`): a real, steppable DAP over the same stdio
//! `Content-Length` framing as the LSP. The debugger model reinterprets arb's
//! stream world onto DAP concepts — the same per-line predicate scan the TUI
//! already runs against `expect /regex/` reactions, run headless and made to
//! *block* the feed thread on a condvar instead of firing an action:
//!
//! * program = the input stream (each incoming line is one step)
//! * breakpoint = a regex predicate on lines (a `SourceBreakpoint.condition`, or
//!   an unconditional breakpoint = single-step)
//! * stack trace = the query pipeline stages the paused line flows through
//! * scopes/vars = the matched line + stream stats + input control values
//! * evaluate = arb's real expression evaluator run against the paused line
//!
//! Because DAP owns stdio in `--dap` mode, the data stream cannot also be stdin:
//! the input source (an `input` file) and the spec (`program`) come from the
//! `launch` request. `next`/`stepIn`/`stepOut` collapse to one "advance to the
//! next line" (a stream has no call nesting).

use std::io::{self, BufRead};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;

use regex::Regex;
use serde_json::{json, Value};

use crate::lsp::{read_message, write_message};

/// How a paused feed thread should resume.
#[derive(Clone, Copy, PartialEq)]
enum Resume {
    Continue,
    Step,
}

/// A breakpoint: a regex predicate on incoming lines. `None` condition breaks on
/// every line (an unconditional breakpoint == single-step from that point).
struct Bp {
    line: u32,
    cond: Option<Regex>,
}

/// Shared debugger state: breakpoints, the resume signal, and the pause snapshot
/// the feed thread writes and the reader thread answers `variables`/`stackTrace`
/// from (so no executor round-trip is needed to inspect a paused line).
#[derive(Default)]
pub struct DapState {
    bps: Vec<Bp>,
    resume: Option<Resume>,
    stepping: bool,
    terminated: bool,
    // Pause snapshot.
    step_index: u64,
    matched_line: String,
    transformed: String,
    total: u64,
    rate: f64,
    controls: Vec<(String, String)>,
    pipeline: Vec<String>,
    stop_reason: String,
}

/// The condvar-guarded state the feed thread pauses on and the request handlers
/// wake.
pub struct DapShared {
    state: Mutex<DapState>,
    cv: Condvar,
}

impl DapShared {
    fn new() -> Self {
        DapShared {
            state: Mutex::new(DapState::default()),
            cv: Condvar::new(),
        }
    }
}

static SHARED: OnceLock<DapShared> = OnceLock::new();
static SEQ: AtomicI64 = AtomicI64::new(1);
static OUT: OnceLock<Mutex<io::Stdout>> = OnceLock::new();
static EXECUTOR: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();

fn shared() -> &'static DapShared {
    SHARED.get_or_init(DapShared::new)
}

fn executor_slot() -> &'static Mutex<Option<JoinHandle<()>>> {
    EXECUTOR.get_or_init(|| Mutex::new(None))
}

fn next_seq() -> i64 {
    SEQ.fetch_add(1, Ordering::SeqCst)
}

/// Write one framed message to stdout. Both the reader loop and the feed thread
/// emit, so the writer is mutex-guarded.
fn send(msg: Value) {
    let out = OUT.get_or_init(|| Mutex::new(io::stdout()));
    let _ = write_message(&mut *out.lock().unwrap(), &msg);
}

fn send_event(ev: &str, body: Value) {
    send(json!({ "seq": next_seq(), "type": "event", "event": ev, "body": body }));
}

/// Compile the `setBreakpoints` payload into line-predicate breakpoints. A bp
/// with a non-empty `condition` uses it as the match regex; one without breaks
/// on every line.
fn compile_bps(args: &Value) -> Vec<Bp> {
    args.get("breakpoints")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|b| {
                    let line = b.get("line").and_then(Value::as_u64)? as u32;
                    let cond = b
                        .get("condition")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                        .and_then(|s| Regex::new(s).ok());
                    Some(Bp { line, cond })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The stack frames for a paused line: frame 0 is the line, each deeper frame is
/// one pipeline stage it flows through.
fn stack_frames(st: &DapState) -> Vec<Value> {
    let mut frames = vec![json!({
        "id": 1, "name": format!("line {}", st.step_index),
        "line": st.step_index, "column": 1,
    })];
    for (i, op) in st.pipeline.iter().enumerate() {
        frames.push(json!({ "id": i + 2, "name": op, "line": st.step_index, "column": 1 }));
    }
    frames
}

/// The `variables` rows for a scope reference (1000 Stream / 2000 Controls /
/// 3000 Pipeline). All leaves.
fn variables_body(st: &DapState, vref: u64) -> Vec<Value> {
    let row = |n: &str, v: String| json!({ "name": n, "value": v, "variablesReference": 0 });
    match vref {
        1000 => vec![
            row("line", st.matched_line.clone()),
            row("out", st.transformed.clone()),
            row("step", st.step_index.to_string()),
            row("total", st.total.to_string()),
            row("rate", format!("{:.2}/s", st.rate)),
        ],
        2000 => st.controls.iter().map(|(k, v)| row(k, v.clone())).collect(),
        3000 => st
            .pipeline
            .iter()
            .enumerate()
            .map(|(i, op)| row(&format!("[{i}]"), op.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

/// Evaluate an expression against the paused line: a control name resolves from
/// the snapshot, else it is parsed and run through arb's real evaluator (`x` =
/// line-as-number, `.field` = json/logfmt field).
fn evaluate_body(st: &DapState, expr: &str) -> String {
    if let Some((_, v)) = st.controls.iter().find(|(k, _)| k == expr) {
        return v.clone();
    }
    match crate::expr::parse(expr) {
        Ok(e) => {
            let line = st.matched_line.clone();
            let x = line.trim().parse::<f64>().unwrap_or(f64::NAN);
            let resolve = |n: &str| crate::query::field_num_pub(&line, n);
            crate::query::fmt_scalar(crate::expr::eval_ctx(&e, x, &resolve).unwrap_or(f64::NAN))
        }
        Err(_) => "<not a control or valid expression>".into(),
    }
}

/// The per-line step hook, parameterized over the shared state so it is testable
/// without the process-global. Blocks the calling (feed) thread whenever the
/// line hits a breakpoint or single-step is armed, until a resume/terminate.
#[allow(clippy::too_many_arguments)]
fn check_line_on(
    sh: &DapShared,
    idx: u64,
    line: &str,
    total: u64,
    rate: f64,
    transformed: &str,
    controls: &[(String, String)],
    pipeline: &[String],
) {
    let mut st = sh.state.lock().unwrap();
    if st.terminated {
        return;
    }
    let bp_hit = st
        .bps
        .iter()
        .any(|b| b.cond.as_ref().is_none_or(|re| re.is_match(line)));
    if !st.stepping && !bp_hit {
        return;
    }
    st.stepping = false;
    st.step_index = idx;
    st.matched_line = line.to_string();
    st.transformed = transformed.to_string();
    st.total = total;
    st.rate = rate;
    st.controls = controls.to_vec();
    st.pipeline = pipeline.to_vec();
    st.stop_reason = if bp_hit { "breakpoint" } else { "step" }.to_string();
    let reason = st.stop_reason.clone();
    drop(st);
    send_event(
        "stopped",
        json!({
            "reason": reason, "threadId": 1, "allThreadsStopped": true,
            "text": format!("line {idx}: {line}"),
        }),
    );
    let mut st = sh.state.lock().unwrap();
    loop {
        if st.terminated {
            return;
        }
        if let Some(r) = st.resume.take() {
            if r == Resume::Step {
                st.stepping = true;
            }
            return;
        }
        st = sh.cv.wait(st).unwrap();
    }
}

/// Feed-thread entry over the process-global shared state.
fn check_line(
    idx: u64,
    line: &str,
    total: u64,
    rate: f64,
    transformed: &str,
    controls: &[(String, String)],
    pipeline: &[String],
) {
    if let Some(sh) = SHARED.get() {
        check_line_on(sh, idx, line, total, rate, transformed, controls, pipeline);
    }
}

/// Parse the spec, open the input file, and drive `check_line` per line on a
/// background thread, emitting each transformed line as an `output` event.
fn spawn_feed(program: Option<String>, input: Option<String>) {
    let handle = std::thread::spawn(move || {
        let src = program
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let spec = crate::parser::parse(&src)
            .ok()
            .and_then(|c| crate::spec::build(&c).ok());
        let out_ops = spec.and_then(|s| s.out).unwrap_or_default();
        let labels: Vec<String> = out_ops.iter().map(|o| format!("{o:?}")).collect();
        let mut stream = crate::stream::StreamState::new();
        let reader: Box<dyn BufRead> =
            match input.as_deref().and_then(|p| std::fs::File::open(p).ok()) {
                Some(f) => Box::new(io::BufReader::new(f)),
                None => Box::new(io::BufReader::new(io::empty())),
            };
        let mut idx = 0u64;
        for line in reader.lines().map_while(Result::ok) {
            if shared().state.lock().unwrap().terminated {
                break;
            }
            idx += 1;
            stream.push(line.clone());
            let secs = stream.start.elapsed().as_secs_f64();
            let transformed = match crate::query::eval(&out_ops, std::slice::from_ref(&line), secs)
            {
                crate::query::QueryResult::Lines(v) => v.join("\n"),
                crate::query::QueryResult::Scalar(s) => crate::query::fmt_scalar(s),
                crate::query::QueryResult::Pairs(p) => p
                    .iter()
                    .map(|(k, v)| format!("{k}\t{v}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            check_line(
                idx,
                &line,
                stream.total,
                stream.rate(),
                &transformed,
                &[],
                &labels,
            );
            if !transformed.is_empty() {
                send_event(
                    "output",
                    json!({ "category": "stdout", "output": format!("{transformed}\n") }),
                );
            }
        }
        send_event("terminated", json!({}));
        send_event("exited", json!({ "exitCode": 0 }));
    });
    *executor_slot().lock().unwrap() = Some(handle);
}

/// Handle one DAP request, returning the immediate responses/events to send.
/// Asynchronous events (`stopped`/`output`/`terminated`) are emitted by the feed
/// thread. `_seq` is retained for signature compatibility; sequence numbers come
/// from the shared atomic so both threads stay monotonic.
pub fn handle(msg: &Value, _seq: &mut i64) -> Vec<Value> {
    let command = msg.get("command").and_then(Value::as_str).unwrap_or("");
    let req_seq = msg.get("seq").and_then(Value::as_i64).unwrap_or(0);
    let args = msg.get("arguments").cloned().unwrap_or(Value::Null);
    let resp = |body: Value| {
        json!({
            "seq": next_seq(), "type": "response", "request_seq": req_seq,
            "success": true, "command": command, "body": body,
        })
    };
    let event = |ev: &str, body: Value| json!({ "seq": next_seq(), "type": "event", "event": ev, "body": body });

    match command {
        "initialize" => vec![
            resp(json!({
                "supportsConfigurationDoneRequest": true,
                "supportsConditionalBreakpoints": true,
                "supportsEvaluateForHovers": true,
                "supportsTerminateRequest": true,
            })),
            event("initialized", json!({})),
        ],
        "configurationDone" | "setExceptionBreakpoints" => vec![resp(json!({}))],
        "setBreakpoints" => {
            let bps = compile_bps(&args);
            let verified: Vec<Value> = bps
                .iter()
                .map(|b| json!({ "verified": true, "line": b.line }))
                .collect();
            shared().state.lock().unwrap().bps = bps;
            vec![resp(json!({ "breakpoints": verified }))]
        }
        "launch" => {
            if args.get("stopOnEntry").and_then(Value::as_bool) == Some(true) {
                shared().state.lock().unwrap().stepping = true;
            }
            let program = args
                .get("program")
                .and_then(Value::as_str)
                .map(str::to_string);
            let input = args
                .get("input")
                .and_then(Value::as_str)
                .map(str::to_string);
            spawn_feed(program, input);
            vec![resp(json!({}))]
        }
        "threads" => vec![resp(json!({ "threads": [{ "id": 1, "name": "stream" }] }))],
        "stackTrace" => {
            let st = shared().state.lock().unwrap();
            let frames = stack_frames(&st);
            let n = frames.len();
            vec![resp(json!({ "stackFrames": frames, "totalFrames": n }))]
        }
        "scopes" => vec![resp(json!({ "scopes": [
            { "name": "Stream", "variablesReference": 1000, "expensive": false },
            { "name": "Controls", "variablesReference": 2000, "expensive": false },
            { "name": "Pipeline", "variablesReference": 3000, "expensive": false },
        ] }))],
        "variables" => {
            let vref = args
                .get("variablesReference")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let st = shared().state.lock().unwrap();
            vec![resp(json!({ "variables": variables_body(&st, vref) }))]
        }
        "evaluate" => {
            let expr = args
                .get("expression")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let st = shared().state.lock().unwrap();
            vec![resp(
                json!({ "result": evaluate_body(&st, &expr), "variablesReference": 0 }),
            )]
        }
        "continue" => {
            {
                let mut st = shared().state.lock().unwrap();
                st.resume = Some(Resume::Continue);
            }
            shared().cv.notify_all();
            vec![resp(json!({ "allThreadsContinued": true }))]
        }
        "next" | "stepIn" | "stepOut" => {
            {
                let mut st = shared().state.lock().unwrap();
                st.resume = Some(Resume::Step);
            }
            shared().cv.notify_all();
            vec![resp(json!({}))]
        }
        "disconnect" | "terminate" => {
            {
                let mut st = shared().state.lock().unwrap();
                st.terminated = true;
                st.resume = Some(Resume::Continue);
            }
            shared().cv.notify_all();
            if let Some(h) = executor_slot().lock().unwrap().take() {
                let _ = h.join();
            }
            vec![resp(json!({}))]
        }
        // Unknown requests succeed with an empty body (lenient, DAP-tolerant).
        _ => vec![resp(json!({}))],
    }
}

/// Run the DAP over stdio until EOF, `disconnect`, or `terminate`.
pub fn run() {
    let stdin = io::stdin();
    let mut r = stdin.lock();
    let mut seq: i64 = 0;
    while let Ok(Some(msg)) = read_message(&mut r) {
        let cmd = msg
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        for out in handle(&msg, &mut seq) {
            send(out);
        }
        if cmd == "disconnect" || cmd == "terminate" {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_state_defaults() {
        let st = DapState::default();
        assert!(st.bps.is_empty());
        assert!(!st.terminated);
        assert!(!st.stepping);
    }

    #[test]
    fn setbreakpoints_compiles_condition() {
        let bps = compile_bps(&json!({ "breakpoints": [{ "line": 3, "condition": "ERROR" }] }));
        assert_eq!(bps.len(), 1);
        assert_eq!(bps[0].line, 3);
        assert!(bps[0].cond.as_ref().unwrap().is_match("ERROR: boom"));
        // A bp with no condition breaks on every line (cond is None).
        let uncond = compile_bps(&json!({ "breakpoints": [{ "line": 1 }] }));
        assert!(uncond[0].cond.is_none());
    }

    #[test]
    fn variables_stream_scope_rows() {
        let mut st = DapState {
            matched_line: "hi".into(),
            transformed: "HI".into(),
            step_index: 2,
            total: 5,
            rate: 1.5,
            ..Default::default()
        };
        let names: Vec<String> = variables_body(&st, 1000)
            .iter()
            .map(|r| r["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["line", "out", "step", "total", "rate"]);
        // Controls scope reflects the snapshot's input values.
        st.controls = vec![("th".into(), "7".into())];
        assert_eq!(variables_body(&st, 2000)[0]["value"], "7");
    }

    #[test]
    fn evaluate_arith_over_line_and_control() {
        let mut st = DapState {
            matched_line: "42".into(),
            ..Default::default()
        };
        // Real expression evaluated against the paused line (x = line-as-number).
        assert_eq!(evaluate_body(&st, "x*2"), "84");
        // A control name resolves from the snapshot directly.
        st.controls = vec![("th".into(), "9".into())];
        assert_eq!(evaluate_body(&st, "th"), "9");
    }

    #[test]
    fn stack_frames_are_line_then_pipeline() {
        let st = DapState {
            step_index: 3,
            pipeline: vec!["Match".into(), "Upper".into()],
            ..Default::default()
        };
        let f = stack_frames(&st);
        assert_eq!(f.len(), 3); // line + 2 ops
        assert_eq!(f[0]["name"], "line 3");
        assert_eq!(f[1]["name"], "Match");
    }

    #[test]
    fn check_line_no_bp_no_step_returns() {
        // Empty bps + stepping false => fast path, no block, no panic.
        let sh = DapShared::new();
        check_line_on(&sh, 1, "line", 1, 0.0, "line", &[], &[]);
    }

    #[test]
    fn disconnect_releases_blocked_thread() {
        use std::sync::Arc;
        let sh = Arc::new(DapShared::new());
        sh.state.lock().unwrap().stepping = true; // pauses on the first line
        let sh2 = sh.clone();
        let t = std::thread::spawn(move || {
            check_line_on(&sh2, 1, "x", 1, 0.0, "x", &[], &[]);
        });
        // Repeatedly signal terminate + wake until the paused thread exits.
        while !t.is_finished() {
            {
                let mut st = sh.state.lock().unwrap();
                st.terminated = true;
                st.resume = Some(Resume::Continue);
            }
            sh.cv.notify_all();
            std::thread::yield_now();
        }
        t.join().unwrap();
    }
}
