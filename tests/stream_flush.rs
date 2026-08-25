//! `arb` as a pipe filter must be fast on a BULK stream and prompt on a LIVE one.
//!
//! `io::stdout().lock()` is a `LineWriter`, so writing straight to it costs one
//! `write(2)` per output line — on a 2.4M-line stream that was the single
//! largest cost in a `sample` profile (756 of 2434 samples) and the reason `jq`,
//! which block-buffers when its stdout is redirected, was ahead on every filter
//! measured. `cli::LineOut` wraps it in a `BufWriter` to fix that.
//!
//! The obvious way to write that wrapper breaks live streaming:
//! `tail -f … | arb | consumer` would stall until 256KB accumulated. So the
//! flush is decided per line by whether the READER still holds buffered input.
//! This file pins the half of that contract a benchmark cannot see — that a line
//! written to a still-open stdin reaches the consumer straight away.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// One line in, one line out, WITHOUT closing stdin.
///
/// If the output were merely `BufWriter`ed, nothing would arrive until the
/// buffer filled or the process exited, and this would time out. The generous
/// budget is deliberate: the property under test is "does not wait for EOF",
/// not a latency figure, so a loaded CI box cannot make it flaky.
#[test]
fn a_line_reaches_the_consumer_before_stdin_closes() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_arb"))
        .args(["-e", "out { in.json; .a }"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn arb");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        for _ in 0..3 {
            let mut l = String::new();
            if r.read_line(&mut l).unwrap_or(0) == 0 {
                break;
            }
            if tx.send(l.trim_end().to_string()).is_err() {
                break;
            }
        }
    });

    // Three lines, each written and then waited on individually. stdin stays
    // OPEN throughout, so nothing here can be explained by an EOF flush.
    for i in 1..=3 {
        writeln!(stdin, "{{\"a\":{i}}}").unwrap();
        stdin.flush().unwrap();
        let got = rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|e| panic!("line {i} never arrived with stdin still open: {e}"));
        assert_eq!(got, i.to_string());
    }

    drop(stdin);
    let _ = child.wait();
}

/// The bulk half: a stream the reader can supply faster than the writer drains
/// must still come out COMPLETE and in order. Buffering that loses or reorders
/// the tail is the failure this catches — the final flush is easy to forget,
/// and it is the last 256KB of any redirected run.
#[test]
fn a_bulk_stream_comes_out_complete_and_in_order() {
    const N: usize = 50_000;
    let input: String = (0..N).map(|i| format!("{{\"a\":{i}}}\n")).collect();

    let mut child = Command::new(env!("CARGO_BIN_EXE_arb"))
        .args(["-e", "out { in.json; .a }"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn arb");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    // Feed from a thread: 50k lines overflow the pipe buffer, so writing them
    // from this thread while nothing drains stdout would deadlock.
    std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });

    let got: Vec<String> = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .collect();
    let _ = child.wait();

    assert_eq!(got.len(), N, "every line must survive the buffer");
    assert_eq!(got.first().map(String::as_str), Some("0"));
    assert_eq!(
        got.last().map(String::as_str),
        Some("49999"),
        "tail flushed"
    );
    assert!(
        got.iter().enumerate().all(|(i, l)| l == &i.to_string()),
        "order must be the input's"
    );
}
