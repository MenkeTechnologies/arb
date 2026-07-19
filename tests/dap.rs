//! DAP process-level regression. The `arb --dap` adapter emits every transformed
//! line as an `output` event on stdout. A client that stops draining stdout lets
//! that pipe fill, wedging the feed thread mid-`write()` *while it holds the OUT
//! mutex* — so the `disconnect` response `send()` used to deadlock on that same
//! mutex and the process hung forever, leaking. The teardown watchdog bounds that
//! to ~200ms. This test reproduces the wedge (large input, stdout never read) and
//! asserts the process still exits promptly after `disconnect`.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn frame(v: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_vec(v).unwrap();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

#[test]
fn disconnect_with_undrained_stdout_exits_promptly() {
    let dir = std::env::temp_dir().join(format!("arb-dap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let prog = dir.join("d.arb");
    let data = dir.join("big.txt");
    std::fs::write(&prog, "out { in }\n").unwrap();
    // Enough lines to overflow the ~64KB stdout pipe buffer once un-drained.
    let big: String = (0..200_000).map(|i| format!("line {i}\n")).collect();
    std::fs::write(&data, big).unwrap();

    // stdout is piped but deliberately NEVER read -> the feed thread wedges.
    let mut child = Command::new(env!("CARGO_BIN_EXE_arb"))
        .arg("--dap")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn arb --dap");
    let mut stdin = child.stdin.take().unwrap();

    let mut seq = 0;
    let mut send = |cmd: &str, args: serde_json::Value| {
        seq += 1;
        let mut msg = serde_json::json!({ "seq": seq, "type": "request", "command": cmd });
        if !args.is_null() {
            msg["arguments"] = args;
        }
        stdin.write_all(&frame(&msg)).unwrap();
        stdin.flush().unwrap();
    };
    send("initialize", serde_json::Value::Null);
    send(
        "launch",
        serde_json::json!({ "program": prog.to_str().unwrap(), "input": data.to_str().unwrap() }),
    );
    // Give the feed thread time to fill the un-drained stdout pipe and block.
    std::thread::sleep(Duration::from_millis(1500));
    send("disconnect", serde_json::Value::Null);
    drop(stdin);

    // Poll for exit; the watchdog guarantees it within ~200ms of disconnect.
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            panic!("arb --dap hung after disconnect with un-drained stdout");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
