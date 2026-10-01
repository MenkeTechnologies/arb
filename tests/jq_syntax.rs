//! jq source text that has to survive arb's lexer intact to reach the engine.
//!
//! A jq program inside `out { … }` is one verbatim atom, but `;` is both jq's
//! definition/argument separator and arb's command terminator. Each case below
//! was cut in half by the lexer before it was fixed. Expectations were measured
//! against jq 1.8.2 (`jq -rc`); headless, no jq needed.

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
fn answers(filter: &str, input: &str, stdout: &str) {
    assert_eq!(
        arb(filter, input),
        (stdout.to_string(), String::new(), 0),
        "`{filter}` over {input:?}"
    );
}

/// jq's grammar takes a `def` wherever a term may start, not only at the front
/// of the program. The lexer only kept the `;` of a LEADING `def`, so a `def`
/// after a pipe was cut at its `;`.
#[test]
fn a_def_after_a_pipe_is_one_program() {
    answers(". | def s: .[0]; s", "[1,2]\n", "1\n");
    answers(".[] | def f: . * 2; f", "[1,2]\n", "2\n4\n");
    answers("1 as $x | def f: $x; f", "null\n", "1\n");
    answers(".[0] as $a | def f: $a; f", "[1,2]\n", "1\n");
    // `def` as a key or inside a string is not the keyword.
    answers(r#"{"def": 1} | .def"#, "null\n", "1\n");
    answers(r#"[.[] | tostring] | join("def ")"#, "[1,2]\n", "1def 2\n");
}

/// A program may open with `null`/`true`/`false` or a negated term; it was
/// lexed as an arb command and cut at the first `;`.
#[test]
fn literal_and_negation_led_programs_are_one_atom() {
    answers("null | setpath([0]; 5)", "null\n", "[5]\n");
    answers("true as $x | setpath([0]; $x)", "null\n", "[true]\n");
    answers("- 1 | [., 1] | setpath([0]; 3)", "null\n", "[3,1]\n");
    answers("-.a", "{\"a\":2}\n", "-2\n");
}

/// jq 1.8 binds `Expr "as" Patterns | Query`: the source of `as` is the whole
/// operator expression, not just the term before it.
#[test]
fn as_binds_a_whole_expression() {
    answers("1 + 2 as $x | $x * 10", "null\n", "30\n");
    answers("1 // 2 as $x | $x + 10", "null\n", "11\n");
    answers("1 == 1 as $x | 5", "null\n", "5\n");
    answers("-1 as $x | [limit(1; $x, 2)]", "null\n", "[-1]\n");
    answers("[1, 2 as $x | $x, 3]", "null\n", "[1,2,3]\n");
    answers("reduce 1 + 2 as $x (0; . + $x)", "null\n", "3\n");
}

/// A quoted subscript is a key only when it is ONE plain string literal, read
/// with its escapes; `.["a","b"]` generates two lookups and an interpolated key
/// is computed, as jq does.
#[test]
fn quoted_subscripts_follow_jq() {
    let doc = "{\"a\":[1],\"b\":2,\"a\\\"b\":3,\"k\":\"b\"}\n";
    answers(r#".["a","b"]"#, doc, "[1]\n2\n");
    answers(r#".["a\"b"]"#, doc, "3\n");
    answers(r#".["\(.k)"]"#, doc, "2\n");
}

/// jq's `DictPairs` may be empty after a comma, so `{a: 1,}` is legal.
#[test]
fn an_object_takes_a_trailing_comma() {
    answers("{a: 1, b: 2,}", "null\n", "{\"a\":1,\"b\":2}\n");
    answers("[{a,}]", "{\"a\":7}\n", "[{\"a\":7}]\n");
}
