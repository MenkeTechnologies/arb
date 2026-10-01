//! jq's input cursor seen from arb's `out { … }` filter path: `input`,
//! `inputs` and `input_line_number` share one position over the stream, the
//! way jq's parser does. Expectations measured against `jq -rc` 1.8.2; headless,
//! no jq needed.

use std::io::Write;
use std::process::{Command, Stdio};

/// `out { in.json; FILTER }` over `input`: stdout, asserting a clean exit.
fn stdout_of(filter: &str, input: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_arb"))
        .args(["-e", &format!("out {{ in.json; {filter} }}")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn arb");
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(input.as_bytes());
    drop(stdin);
    let out = child.wait_with_output().expect("arb output");
    assert!(out.status.success(), "`{filter}` failed: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The streaming path runs one line at a time; `input_line_number` still counts
/// the whole stream, and a line taken by `input` advances it too.
#[test]
fn input_line_number_counts_the_whole_stream() {
    assert_eq!(stdout_of("input_line_number", "1\n2\n3\n"), "1\n2\n3\n");
    assert_eq!(
        stdout_of(".[] | input_line_number", "[1,2]\n[3]\n"),
        "1\n1\n2\n"
    );
    assert_eq!(
        stdout_of("[., input, input_line_number]", "1\n2\n3\n4\n"),
        "[1,2,2]\n[3,4,4]\n"
    );
    assert_eq!(
        stdout_of(
            "[., input_line_number, ([inputs] | length), input_line_number]",
            "1\n2\n3\n"
        ),
        "[1,1,2,3]\n"
    );
}
