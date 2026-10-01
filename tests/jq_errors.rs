//! jq's uncaught-error and `halt` model on arb's `out { … }` filter path.
//!
//! jq does not abort a stream at an uncaught error (`process` in jq's main.c):
//! the values the input produced BEFORE the error are printed, the error goes
//! to stderr, the NEXT input still runs, and the exit status is the LAST input's
//! — 5 when it raised, 0 otherwise. `halt`/`halt_error` end the whole run with
//! their own status and write their message unprefixed.
//!
//! arb used to stop at the first error with exit 5, dropping both the earlier
//! output of that input and every later input. Each expectation below was
//! measured against jq 1.8.2 (`jq -rc`); stderr keeps arb's `arb: jq: ` prefix
//! where jq writes `jq: error (at <stdin>:N): `. Headless and CI-safe: no jq is
//! needed to run them.

use std::io::Write;
use std::process::{Command, Stdio};

/// Run `out { in.json; FILTER }` over `input`; (stdout, stderr, exit status).
fn arb(filter: &str, input: &str) -> (String, String, i32) {
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
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[track_caller]
fn check(filter: &str, input: &str, stdout: &str, stderr: &str, code: i32) {
    assert_eq!(
        arb(filter, input),
        (stdout.to_string(), stderr.to_string(), code),
        "`{filter}` over {input:?}"
    );
}

/// The values before an error are kept and the next input runs; the status is
/// the last input's.
#[test]
fn an_error_ends_its_input_not_the_stream() {
    check(
        ".[] | 10 / .",
        "[1,0,2]\n[5]\n",
        "10\n2\n",
        "arb: jq: number (10) and number (0) cannot be divided because the divisor is zero\n",
        0,
    );
    check(
        r#"if . == 2 then error("e") else . end"#,
        "1\n2\n3\n",
        "1\n3\n",
        "arb: jq: e\n",
        0,
    );
    check(
        r#"if . == 2 then error("e") else . end"#,
        "1\n2\n",
        "1\n",
        "arb: jq: e\n",
        5,
    );
    // The single-stage fast paths (`.[]`, `.a`) follow the same model.
    check(
        ".[]",
        "[1]\n3\n[2]\n",
        "1\n2\n",
        "arb: jq: Cannot iterate over number (3)\n",
        0,
    );
    // The batch path (a program that reads `input`) too.
    check(
        "., (input | 1/.)",
        "1\n0\n2\n0\n",
        "1\n2\n",
        "arb: jq: number (1) and number (0) cannot be divided because the divisor is zero\n\
         arb: jq: number (1) and number (0) cannot be divided because the divisor is zero\n",
        5,
    );
}

/// A non-string error value is reported as main.c words it: `(not a string):`
/// and the value's compact JSON.
#[test]
fn a_non_string_error_is_reported_as_json() {
    check(
        r#"error({"a":.})"#,
        "1\n2\n",
        "",
        "arb: jq: (not a string): {\"a\":1}\narb: jq: (not a string): {\"a\":2}\n",
        5,
    );
    check("error", "null\n", "", "arb: jq: (not a string): null\n", 5);
}

/// `halt_error` writes a string raw with no newline, other values as JSON plus a
/// newline, `null` not at all — and ends the run with its status.
#[test]
fn halt_ends_the_run_with_its_status() {
    check(r#""x\(.)" | halt_error"#, "1\n2\n", "", "x1", 5);
    check(r#"{"a":.} | halt_error(3)"#, "1\n2\n", "", "{\"a\":1}\n", 3);
    check("halt_error(1)", "null\n", "", "", 1);
    check("if . == 2 then halt else . end", "1\n2\n3\n", "1\n", "", 0);
}
