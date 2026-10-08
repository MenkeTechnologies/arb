//! Differential tests for arb's jq engine: the SAME program through arb and
//! through the real `jq` binary, byte-diffed.
//!
//! arb's README and SPEC §8 claim the query engine is a `jq` SUPERSET. A superset
//! claim is only worth what checks it, and the only honest check is the reference
//! implementation's own answer. `scripts/jq_parity.sh` runs the broad corpus;
//! this file is the part that belongs in `cargo test`: every construct here is a
//! REGRESSION PIN for a divergence that was actually found and fixed while the
//! engine was written, plus the invariants that have no jq oracle.
//!
//! Headless and CI-safe. Every jq-backed test SKIPS (loudly) when `jq` is not on
//! PATH, or when the `jq` that is there is not the 1.8 line the expectations were
//! measured against — a different reference is a reason to skip, never a reason
//! to pass.

use arb::parser::parse;
use arb::query::{eval, QueryResult};
use std::io::Write;
use std::process::{Command, Stdio};

/// Run `filter` through arb's `out { in.json; … }` pipeline over `input` lines.
fn arb_run(filter: &str, input: &[&str]) -> Result<Vec<String>, String> {
    let src = format!("tail .x\nsource .x {{ in.json; {filter} }}");
    let spec = build_or(&src)?;
    let lines: Vec<String> = input.iter().map(|s| (*s).to_string()).collect();
    match eval(&spec, &lines, 1.0) {
        QueryResult::Lines(l) => Ok(l),
        QueryResult::Error(e) => Err(e),
        other => Err(format!("unexpected result shape: {other:?}")),
    }
}

fn build_or(src: &str) -> Result<Vec<arb::query::QueryOp>, String> {
    let cmds = parse(src).map_err(|e| e.to_string())?;
    let spec = arb::spec::build(&cmds).map_err(|e| e.to_string())?;
    Ok(spec.widgets[0]
        .source
        .as_ref()
        .ok_or("no source")?
        .pipeline
        .clone())
}

/// `jq -rc filter` over the same lines. `None` when jq refused (any non-zero
/// exit), which the caller compares against arb's own refusal.
fn jq_run(filter: &str, input: &[&str]) -> Option<Vec<String>> {
    let mut child = Command::new("jq")
        .args(["-rc", filter])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let payload = input.join("\n") + "\n";
    // A filter that stops early (`first`, `limit`, `break`) makes jq close its
    // stdin, so the write can fail with EPIPE — that is a normal outcome here,
    // not an error, and the output it already produced is still the answer.
    let _ = stdin.write_all(payload.as_bytes());
    drop(stdin);
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    Some(
        text.strip_suffix('\n')
            .unwrap_or(&text)
            .split('\n')
            .filter(|l| !(text.is_empty() && l.is_empty()))
            .map(str::to_string)
            .collect::<Vec<_>>()
            .into_iter()
            .filter(|l| !l.is_empty() || text.trim() != "")
            .collect(),
    )
}

/// Is a usable reference present? The expectations below were measured against
/// the jq 1.8 line; an older jq differs on `from_entries`, `ltrimstr` and number
/// literal rendering, so it is skipped rather than compared.
fn reference_ok() -> bool {
    let Ok(out) = Command::new("jq").arg("--version").output() else {
        eprintln!("SKIP: no `jq` on PATH — the differential probes need the reference");
        return false;
    };
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.starts_with("jq-1.8") {
        return true;
    }
    eprintln!("SKIP: reference is `{v}`, expectations were measured against jq-1.8");
    false
}

/// Byte-diff one probe. A construct BOTH engines refuse counts as agreement —
/// what is being checked is that arb never answers where jq raises.
#[track_caller]
fn same(filter: &str, input: &[&str]) {
    let ours = arb_run(filter, input);
    let theirs = jq_run(filter, input);
    match (ours, theirs) {
        (Ok(a), Some(b)) => assert_eq!(a, b, "`{filter}` over {input:?}"),
        (Err(_), None) => {}
        (Ok(a), None) => panic!("`{filter}` over {input:?}: jq REFUSED, arb answered {a:?}"),
        (Err(e), Some(b)) => panic!("`{filter}` over {input:?}: arb refused ({e}), jq gave {b:?}"),
    }
}

fn run_table(probes: &[(&str, &[&str])]) {
    if !reference_ok() {
        return;
    }
    for (filter, input) in probes {
        same(filter, input);
    }
}

const OBJ: &[&str] = &[r#"{"a":1,"b":"x","c":[1,2,3],"d":{"e":5},"n":null,"t":true}"#];
const ARR: &[&str] = &["[3,1,2,10,-4]"];
const RECS: &[&str] =
    &[r#"[{"id":1,"n":"a","v":10},{"id":2,"n":"b","v":5},{"id":3,"n":"a","v":7}]"#];

/// The generator constructs a `Vec<QueryOp>` cannot express — the whole reason
/// the jq engine exists. Each of these was a hard error before it.
#[test]
fn generators_and_control_flow_match_jq() {
    run_table(&[
        (".a, .b", OBJ),
        ("[.a, .b]", OBJ),
        ("{x: .a, y: .b}", OBJ),
        ("{(.b): .a}", OBJ),
        ("[.c[] | select(. > 1)]", OBJ),
        ("if .t then \"yes\" else \"no\" end", OBJ),
        ("if .n then 1 elif .a then 2 else 3 end", OBJ),
        ("reduce (.c[]) as $x (0; . + $x)", OBJ),
        ("[foreach (.c[]) as $x (0; . + $x; [$x, .])]", OBJ),
        ("[limit(2; .c[])]", OBJ),
        ("first(.c[])", OBJ),
        ("last(.c[])", OBJ),
        ("[label $out | (.c[], break $out)]", OBJ),
        ("def f: . * 2; .c | map(f)", OBJ),
        ("def g(x): x + x; .a | g(.)", OBJ),
        ("def h($n): $n * 3; .a | h(.)", OBJ),
        (
            "def fact: if . <= 1 then 1 else . * (. - 1 | fact) end; 5 | fact",
            OBJ,
        ),
        (".a as $x | .d.e as $y | [$x, $y]", OBJ),
        (". as {a: $q} | $q", OBJ),
        (". as {$a, $b} | [$a, $b]", OBJ),
        (".c as [$p, $q] | [$p, $q]", OBJ),
        (". as [$a] ?// {$a} | $a", OBJ),
        ("[while(. < 100; . * 2)]", &["3"]),
        ("[.c[] | until(. > 5; . + 1)]", OBJ),
        ("[limit(3; repeat(1))]", OBJ),
        ("isempty(.c[])", OBJ),
        ("isempty(empty)", OBJ),
    ]);
}

/// jq's paths, `..`, and the whole assignment family — all built on `path`.
#[test]
fn paths_and_assignment_match_jq() {
    run_table(&[
        ("[..]", OBJ),
        ("[paths]", OBJ),
        ("[paths(numbers)]", OBJ),
        ("[path(.d.e)]", OBJ),
        ("getpath([\"d\",\"e\"])", OBJ),
        ("setpath([\"d\",\"f\"]; 9)", OBJ),
        ("delpaths([[\"a\"],[\"b\"]])", OBJ),
        ("del(.a)", OBJ),
        ("del(.a, .b)", OBJ),
        ("del(.c[0])", OBJ),
        (".a = 9", OBJ),
        (".z = 9", OBJ),
        (".a |= . + 1", OBJ),
        (".a += 5", OBJ),
        (".a -= 5", OBJ),
        (".a *= 5", OBJ),
        (".a /= 5", OBJ),
        (".zz //= 3", OBJ),
        (".c[1] = 99", OBJ),
        (".c[1:2] = [\"x\"]", OBJ),
        (".c[1:2] |= map(. * 10)", OBJ),
        ("(.a, .d.e) |= . + 100", OBJ),
        ("map_values(tostring)", OBJ),
        ("pick(.a, .d)", OBJ),
        ("walk(if type == \"number\" then . + 1 else . end)", OBJ),
        ("[tostream]", OBJ),
        ("[tostream] | fromstream(.[])", OBJ),
    ]);
}

/// The builtin library. Weighted toward the ones whose jq definition has a
/// corner most reimplementations miss.
#[test]
fn builtin_library_matches_jq() {
    run_table(&[
        (". | to_entries", OBJ),
        (". | to_entries | from_entries", OBJ),
        ("with_entries(.value |= tostring)", OBJ),
        ("keys_unsorted", OBJ),
        ("type", OBJ),
        ("tojson | fromjson", OBJ),
        ("tostring", OBJ),
        ("[.c[] | tostring | tonumber]", OBJ),
        ("[.[] | type] | unique", OBJ),
        (". | sort", ARR),
        ("sort_by(-.)", ARR),
        ("group_by(. % 2)", ARR),
        ("unique", ARR),
        ("min_by(.)", ARR),
        ("max_by(.)", ARR),
        (". | reverse", ARR),
        ("indices(1)", ARR),
        ("index(1)", ARR),
        ("rindex(1)", ARR),
        (". | .[]", &[r#"{"b":1,"a":2,"C":3}"#]),
        (". | add", &[r#"{"b":"x","a":"y"}"#]),
        (". | flatten", &[r#"{"b":[1],"a":[2]}"#]),
        ("[range(3)]", ARR),
        ("[range(2; 10; 3)]", ARR),
        ("[range(10; 0; -3)]", ARR),
        (". | flatten", &["[[1,[2]],3]"]),
        ("flatten(1)", &["[[1,[2]],3]"]),
        ("flatten(1)", &[r#"{"a":[1,[2]]}"#]),
        (". | add", ARR),
        ("any", &["[false,true]"]),
        ("all", &["[false,true]"]),
        ("any(. > 2)", ARR),
        ("all(. > 2)", ARR),
        ("transpose", &["[[1,2],[3,4]]"]),
        ("combinations", &["[[1,2],[3]]"]),
        ("[.[] | tojson]", ARR),
        ("join(\"-\")", &[r#"["a","b",null]"#]),
        ("sort_by(.v)", RECS),
        ("group_by(.n)", RECS),
        ("unique_by(.n)", RECS),
        ("min_by(.v)", RECS),
        ("max_by(.v)", RECS),
        ("INDEX(.id)", RECS),
        ("map(.n) | IN(\"a\")", RECS),
        (
            "group_by(.n) | map({n: .[0].n, total: (map(.v) | add)})",
            RECS,
        ),
        ("[.[] | with_entries(select(.key != \"id\"))]", RECS),
        ("$ENV | type", OBJ),
        ("env | has(\"PATH\")", OBJ),
        ("$__loc__", OBJ),
    ]);
}

/// String, regex and `@format` builtins.
#[test]
fn string_and_regex_builtins_match_jq() {
    const S: &[&str] = &[r#""Hello, World""#];
    run_table(&[
        (". | length", S),
        ("utf8bytelength", S),
        ("explode | implode", S),
        ("ascii_downcase", S),
        ("ascii_upcase", S),
        ("ltrimstr(\"Hello\")", S),
        ("rtrimstr(\"World\")", S),
        ("startswith(\"He\")", S),
        ("endswith(\"ld\")", S),
        ("test(\"wor\")", S),
        ("test(\"wor\"; \"i\")", S),
        ("[match(\"o\"; \"g\")]", S),
        ("capture(\"(?<x>W.rld)\")", S),
        ("[scan(\"[A-Z]\")]", S),
        ("split(\", \")", S),
        ("[splits(\", \")]", S),
        ("sub(\"World\"; \"There\")", S),
        ("gsub(\"[aeiou]\"; \"*\")", S),
        ("gsub(\"(?<c>[A-Z])\"; \"<\\(.c)>\")", S),
        ("indices(\"o\")", S),
        ("index(\"o\")", S),
        ("rindex(\"o\")", S),
        ("@base64", S),
        ("@base64 | @base64d", S),
        ("@uri", S),
        ("@html", S),
        ("@sh", S),
        ("@json", S),
        ("@text", S),
        ("@csv", &["[1,\"a\",null,true]"]),
        ("@tsv", &["[\"a\\tb\",\"c\"]"]),
        ("@base64 \"v=\\(.)\"", &["42"]),
        ("\"n=\\(.a) s=\\(.b)\"", OBJ),
        ("[\"\\(.c[])\"]", OBJ),
    ]);
}

/// Errors, `?`, `//` and `try` — where "arb must not answer where jq raises" is
/// the whole contract.
#[test]
fn error_paths_match_jq() {
    run_table(&[
        (".zz?", OBJ),
        (".zz // \"def\"", OBJ),
        (".n // \"def\"", OBJ),
        (".t // \"def\"", OBJ),
        ("try error(\"boom\") catch .", OBJ),
        ("try error({code: 7}) catch .code", OBJ),
        (
            "[.c[] | try (if . == 2 then error(\"two\") else . end) catch \"caught\"]",
            OBJ,
        ),
        ("[.c[] | (if . == 2 then error(\"two\") else . end)?]", OBJ),
        // Type errors: jq raises, so arb must too.
        (". + 3", OBJ),
        (". - 3", OBJ),
        (". * 3", OBJ),
        (". / 3", OBJ),
        (". % 3", OBJ),
        (".a / 0", OBJ),
        (".a % 0", OBJ),
        (". | keys", &["1"]),
        (". | length", &["true"]),
        (".[]", &["null"]),
        (".a", &["3"]),
        (".[1]", &[r#""hello""#]),
        ("implode", OBJ),
        ("tonumber", OBJ),
        ("from_entries", OBJ),
        ("ltrimstr(\"x\")", OBJ),
        ("has(\"a\")", &["[1,2]"]),
        ("contains(\"x\")", OBJ),
        ("@csv", OBJ),
    ]);
}

/// jq keeps the SOURCE literal of a number it never computed, and prints it in
/// decNumber's canonical form. Both halves are observable and neither survives an
/// `f64` round trip.
#[test]
fn number_literals_match_jq() {
    run_table(&[
        (". as $x | $x", &["1.50"]),
        (". as $x | $x", &["1e2"]),
        (". as $x | $x", &["12e3"]),
        (". as $x | $x", &["1.5e10"]),
        (". as $x | $x", &["0.000001"]),
        (". as $x | $x", &["0.0000001"]),
        (". as $x | $x", &["5e-3"]),
        (". as $x | $x", &["0e0"]),
        (". as $x | $x", &["0.10"]),
        (". as $x | $x", &["-0"]),
        (". as $x | $x", &["3.0"]),
        (". as $x | $x", &["100000000000000000000000"]),
        // ... and loses it the moment arithmetic touches the value.
        (". + 0", &["1.50"]),
        (". as $x | $x", &[r#"{"a":1.50,"b":1e2,"c":[1e-7]}"#]),
        // Computed numbers go through the double formatter instead.
        ("1e308 * 10", &["null"]),
        ("1 / 3", &["null"]),
        ("1e18 / 3", &["null"]),
        ("0.1 + 0.2", &["null"]),
    ]);
}

/// Object key ORDER is observable in jq and is not a set: `keys` sorts,
/// `keys_unsorted` and `to_entries` do not, and `+` appends a new key at the end.
#[test]
fn object_key_order_matches_jq() {
    const O: &[&str] = &[r#"{"b":1,"a":2,"C":3}"#];
    run_table(&[
        (". as $x | $x", O),
        (". | keys", O),
        (". | to_entries", O),
        ("keys_unsorted", O),
        ("to_entries", O),
        ("to_entries | from_entries", O),
        (". + {z: 9}", O),
        (". + {a: 9}", O),
        ("with_entries(.value += 1)", O),
        ("del(.a)", O),
        ("[paths]", O),
    ]);
}

/// `input`/`inputs` share ONE cursor with the outer loop, which is jq's model:
/// a document `inputs` consumed is not replayed as the next `.`.
#[test]
fn inputs_share_one_cursor_with_the_stream() {
    run_table(&[
        ("[., inputs]", &["1", "2", "3"]),
        (".", &["1", "2", "3"]),
        ("[., input]", &["1", "2", "3"]),
        ("input_line_number", &["1", "2", "3"]),
    ]);
}

/// A generated sweep over number LITERALS, in one pass, against the reference.
///
/// The hand-written probes above cover the boundaries that were reasoned about;
/// this covers the ones that were not. Four thousand literals spread across the
/// integer, fraction, sub-1 and extreme-exponent bands go through both engines
/// and must render identically — which is how `serde_json`'s float reader was
/// caught being an ULP off at `e+299` (`-6.306793e+299 | . + 0` came back as
/// `-6.306792999999999e+299`), 386 divergences that the small-magnitude corpus
/// could not see.
///
/// The COMPUTED path is checked separately and to a stated tolerance: jq's own
/// arithmetic loses up to an ULP for an integer above 2^53 — measured, `jq` says
/// `(-516424571754902561 + 0) == -516424571754902500` is `true` when the
/// correctly-rounded double is `…600` — so arb is allowed to differ there and
/// nowhere else.
#[test]
fn generated_number_literals_render_like_jq() {
    if !reference_ok() {
        return;
    }
    let mut lits: Vec<String> = Vec::new();
    // A deterministic spread; no RNG, so a failure is reproducible by eye.
    for e in -12i32..=12 {
        for m in [1u64, 3, 7, 15, 125, 1024, 65537, 999_999] {
            lits.push(format!("{m}e{e}"));
            lits.push(format!("{m}.{m}e{e}"));
        }
    }
    for d in 0..18 {
        lits.push(format!("1{}", "0".repeat(d)));
        lits.push(format!("{}1", "9".repeat(d)));
        lits.push(format!("0.{}1", "0".repeat(d)));
        lits.push(format!("1.{}", "5".repeat(d + 1)));
    }
    for extra in [
        "0",
        "-0",
        "0.0",
        "1.50",
        "3.0",
        "0.10",
        "1e2",
        "1E+2",
        "12e3",
        "0.000001",
        "0.0000001",
        "5e-3",
        "100000000000000000000000",
        "1.7976931348623157e308",
        "-1.7976931348623157e308",
        "2.2250738585072014e-308",
        "9007199254740993",
    ] {
        lits.push(extra.to_string());
    }
    let refs: Vec<&str> = lits.iter().map(String::as_str).collect();

    let ours = arb_run(". as $x | $x", &refs).expect("arb read every literal");
    let theirs = jq_run(". as $x | $x", &refs).expect("jq read every literal");
    assert_eq!(ours.len(), refs.len(), "one output per literal");
    for ((a, b), src) in ours.iter().zip(&theirs).zip(&refs) {
        assert_eq!(a, b, "literal `{src}` rendered differently");
    }

    // The computed path: identical except where jq's own double conversion is
    // lossy, which is only ever an integer above 2^53.
    let ours = arb_run(". + 0", &refs).expect("arb computed every literal");
    let theirs = jq_run(". + 0", &refs).expect("jq computed every literal");
    for ((a, b), src) in ours.iter().zip(&theirs).zip(&refs) {
        if a == b {
            continue;
        }
        let big = a
            .parse::<f64>()
            .is_ok_and(|v| v.abs() > 9_007_199_254_740_992.0);
        assert!(
            big,
            "`{src} + 0`: arb {a}, jq {b} — only jq's >2^53 ULP loss may differ"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Invariants with no jq oracle — these need no reference binary.
// ─────────────────────────────────────────────────────────────────────────────

/// A regex builtin runs once per RECORD, so the compiled engine is memoised. The
/// memo must not turn an invalid pattern into a one-time error: jq raises every
/// time it evaluates one, and so must arb.
#[test]
fn an_invalid_pattern_raises_on_every_record() {
    let out = arb_run(
        r#"[try test("[") catch "ERR"]"#,
        &[r#""a""#, r#""b""#, r#""c""#],
    );
    assert_eq!(
        out.unwrap(),
        vec!["[\"ERR\"]", "[\"ERR\"]", "[\"ERR\"]"],
        "a cached compile FAILURE must be re-raised, not swallowed after the first"
    );
}

/// The memo key must carry the FLAGS as well as the pattern text; keyed on the
/// pattern alone, the second call here would reuse the case-sensitive engine.
#[test]
fn regex_memo_keys_on_flags_not_just_the_pattern() {
    let out = arb_run(r#"[test("ab"), test("ab"; "i"), test("ab")]"#, &[r#""AB""#]);
    assert_eq!(out.unwrap(), vec!["[false,true,false]"]);
}

/// SPEC §8's context rule: a bare alphanumeric word is arb's NATIVE verb, and
/// only the jq CALL spelling reaches the jq engine  EXCEPT that no native verb
/// may claim a spelling jq already defines, or the superset contract breaks on
/// that word. `keys` was the one that did; the native line-per-key verb is
/// spelled `names` now and `stdlib/json.arb` runs `names; tally`.
#[test]
fn a_bare_shared_spelling_stays_the_native_verb() {
    // `keys` in EVERY spelling is jq's sorted array. `jq -rc keys` on this
    // input prints `["a","b"]`; the bare word used to print `a` then `b`.
    for spelling in ["keys", ". | keys", "[.] | .[0] | keys"] {
        assert_eq!(
            arb_run(spelling, &[r#"{"b":1,"a":2}"#]).unwrap(),
            vec![r#"["a","b"]"#],
            "`{spelling}` must answer as jq"
        );
    }
    // The native verb keeps the line-per-key shape under its own name.
    assert_eq!(
        arb_run("names", &[r#"{"b":1,"a":2}"#]).unwrap(),
        vec!["a", "b"]
    );
    // Native `sort_by FIELD` (space) vs jq `sort_by(f)` (call).
    let recs = &[r#"{"v":2}"#, r#"{"v":1}"#];
    assert_eq!(
        arb_run("sort_by v", recs).unwrap(),
        vec![r#"{"v":1}"#, r#"{"v":2}"#]
    );
    assert_eq!(
        arb_run("[.] | sort_by(.v)", recs).unwrap(),
        vec![r#"[{"v":2}]"#, r#"[{"v":1}]"#]
    );
}

/// A name arb has no verb for and jq does not define either must still be an arb
/// diagnostic, not a jq one — a typo'd arb verb is far likelier than a jq
/// program, and `unknown verb` is what points at the real mistake.
#[test]
fn an_unknown_word_is_still_an_arb_unknown_verb() {
    let e = build_or("tail .x\nsource .x { in; bogus }").unwrap_err();
    assert!(
        e.contains("unknown verb"),
        "expected arb's own diagnostic, got: {e}"
    );
}

/// The two places arb deliberately answers where jq 1.8 does not, pinned here so
/// neither can be lost silently. Both are SUPERSET directions — arb defines a
/// builtin jq once had and later dropped — and neither shadows a jq answer.
#[test]
fn arb_defines_two_builtins_jq_18_dropped() {
    // `leaf_paths` was jq's through 1.7 and is `paths(scalars)`.
    assert_eq!(
        arb_run("[leaf_paths]", &[r#"{"a":1,"b":[2]}"#]).unwrap(),
        vec![r#"[["a"],["b",0]]"#]
    );
    // `toarray` wraps a non-array; jq 1.8.2 reports it undefined.
    assert_eq!(arb_run("toarray", &["1"]).unwrap(), vec!["[1]"]);
    assert_eq!(arb_run("toarray", &["[1]"]).unwrap(), vec!["[1]"]);
}

/// A non-JSON line has no jq reading at all (jq refuses the whole input), so arb
/// keeps its line-stream behaviour: the line is jq's STRING.
#[test]
fn a_non_json_line_is_jqs_string() {
    assert_eq!(arb_run(". * 2", &["abc"]).unwrap(), vec!["abcabc"]);
    assert_eq!(arb_run("ascii_upcase", &["abc"]).unwrap(), vec!["ABC"]);
    assert_eq!(arb_run("length", &["abc"]).unwrap(), vec!["3"]);
}

/// A probe with jq 1.8.2's answer RECORDED: `Some(lines)` for an answer, `None`
/// for a refusal. The recorded answer is asserted unconditionally, so a CI box
/// without `jq` still catches a regression; when the 1.8 reference IS present
/// the probe is also byte-diffed live, so a stale recording cannot hide.
type Pinned<'a> = (&'a str, &'a [&'a str], Option<&'a [&'a str]>);

fn run_pinned(probes: &[Pinned]) {
    for (filter, input, want) in probes {
        match (arb_run(filter, input), want) {
            (Ok(got), Some(w)) => assert_eq!(got, *w, "`{filter}` over {input:?}"),
            (Err(_), None) => {}
            (Ok(got), None) => {
                panic!("`{filter}` over {input:?}: jq 1.8 refuses, arb gave {got:?}")
            }
            (Err(e), Some(w)) => {
                panic!("`{filter}` over {input:?}: arb refused ({e}), jq 1.8 gives {w:?}")
            }
        }
    }
    if reference_ok() {
        for (filter, input, _) in probes {
            same(filter, input);
        }
    }
}

/// `path |= empty` deletes EVERY path whose update produced nothing. jq's
/// `_modify` collects them and deletes once at the end; deleting as it went
/// shifted each later array index down, so every other element survived.
#[test]
fn update_to_empty_deletes_every_matched_path() {
    run_pinned(&[
        (".[] |= empty", &["[1,2,3]"], Some(&["[]"])),
        (
            "(.[] | select(. >= 2)) |= empty",
            &["[1,5,3,0,7]"],
            Some(&["[1,0]"]),
        ),
        (
            "(.[] | select(. > 2)) |= empty",
            &["[1,2,3,4]"],
            Some(&["[1,2]"]),
        ),
        (".a[] |= empty", &[r#"{"a":[1,2]}"#], Some(&[r#"{"a":[]}"#])),
        ("map_values(empty)", &["[1,2]"], Some(&["[]"])),
        (".[] |= empty", &[r#"{"a":1,"b":2}"#], Some(&["{}"])),
        (
            ".[] |= (if . > 1 then empty else . * 10 end)",
            &["[1,2,1,3]"],
            Some(&["[10,10]"]),
        ),
    ]);
}

/// `a // b` does NOT swallow an error raised by `a` — jq 1.8 propagates it, and
/// `(a)? // b` is the spelling that suppresses. Swallowing it also hid real
/// errors inside builtins defined with `//`: `join` with a non-string
/// separator answered nothing instead of failing.
#[test]
fn alternative_operator_propagates_errors() {
    run_pinned(&[
        (".a // 3", &["1"], None),
        ("[(1, error(\"x\")) // 3]", &["1"], None),
        ("[error(\"x\") // 3]", &["1"], None),
        ("(.a)? // 3", &["1"], Some(&["3"])),
        (
            "try (error(\"x\") // 1) catch \"caught\"",
            &["1"],
            Some(&["caught"]),
        ),
        ("[(null, false) // 3]", &["1"], Some(&["[3]"])),
        ("[(null, 1, false, 2) // 3]", &["1"], Some(&["[1,2]"])),
        ("[.a.b // 3]", &["{}"], Some(&["[3]"])),
        ("join(.)", &[r#"["0","1"]"#], None),
    ]);
}

/// Interpolation is a chain of `+` whose RIGHT operand is evaluated outermost,
/// so with several generator interpolations the RIGHTMOST varies slowest —
/// the same order `(1,2) + (10,20)` produces.
#[test]
fn string_interpolation_varies_the_rightmost_generator_slowest() {
    run_pinned(&[
        (
            r#""\(1,2)-\(3,4)""#,
            &["null"],
            Some(&["1-3", "2-3", "1-4", "2-4"]),
        ),
        (
            r#""\(1,2)\(3,4)\(5,6)""#,
            &["null"],
            Some(&["135", "235", "145", "245", "136", "236", "146", "246"]),
        ),
        (
            r#"@json "a\(1,2)b\(3,4)""#,
            &["null"],
            Some(&["a1b3", "a2b3", "a1b4", "a2b4"]),
        ),
        ("[(1,2) + (10,20)]", &["null"], Some(&["[11,12,21,22]"])),
        (r#""x\(.a)y""#, &[r#"{"a":[1]}"#], Some(&["x[1]y"])),
    ]);
}

/// `@urid` (jq 1.8) is the inverse of `@uri`. It was not a format name at all,
/// so a leading `@urid` fell through to the xpath front-end as an attribute
/// step and answered NOTHING with a zero exit.
#[test]
fn urid_decodes_percent_escapes_and_refuses_bad_ones() {
    run_pinned(&[
        ("@urid", &[r#""%C3%A9%2b""#], Some(&["é+"])),
        ("@urid", &[r#""a+b%20""#], Some(&["a+b "])),
        ("@urid", &[r#""%6a%71""#], Some(&["jq"])),
        (
            "@uri | @urid",
            &[r#""héllo wörld/?&""#],
            Some(&["héllo wörld/?&"]),
        ),
        (r#"@urid "<\(.)>""#, &[r#""%41""#], Some(&["<A>"])),
        ("@urid", &[r#""%4""#], None),
        ("@urid", &[r#""%zz""#], None),
        ("@urid", &[r#""%ff""#], None),
    ]);
}

/// jq 1.8 builtin semantics that arb's prelude still had in their 1.6/1.7
/// shape: each row is a place the two engines answered differently.
#[test]
fn builtins_follow_jq_18_semantics() {
    run_pinned(&[
        // `add(f)` reduces `f` itself, not `.[] | f`.
        ("add(.[])", &[r#"{"a":1,"b":2}"#], Some(&["3"])),
        ("add(1, 2)", &["null"], Some(&["3"])),
        ("add(empty)", &["null"], Some(&["null"])),
        // `ltrimstr`/`rtrimstr` error on a non-string argument.
        ("ltrimstr(1)", &[r#""a""#], None),
        ("rtrimstr(1)", &[r#""a""#], None),
        ("ltrimstr(\"a\")", &[r#""abc""#], Some(&["bc"])),
        // `last(empty)` yields nothing, like `first(empty)`.
        ("[last(empty)]", &["null"], Some(&["[]"])),
        ("last(1, 2)", &["null"], Some(&["2"])),
        ("[last(null)]", &["null"], Some(&["[null]"])),
        // `nth` past the end is empty, not the last element.
        ("[nth(5; 1, 2)]", &["null"], Some(&["[]"])),
        ("nth(1; 1, 2, 3)", &["null"], Some(&["2"])),
        // Negative counts are errors.
        ("[limit(-1; 1, 2)]", &["null"], None),
        ("[skip(-1; 1, 2)]", &["null"], None),
        // `split("")` splits between characters.
        (r#"split("")"#, &[r#""héy""#], Some(&[r#"["h","é","y"]"#])),
        (r#". / """#, &[r#""ab""#], Some(&[r#"["a","b"]"#])),
        (r#"split("")"#, &[r#""""#], Some(&["[]"])),
        // `tonumber` no longer trims whitespace.
        ("tonumber", &[r#"" 1 ""#], None),
        ("tonumber", &[r#""1 ""#], None),
        ("tonumber", &[r#""1.5""#], Some(&["1.5"])),
        // `paths(f)` runs `f` on every node `..` visits, the root included.
        ("[paths(.a)]", &["1"], None),
        (
            "[paths(type == \"number\")]",
            &[r#"{"a":1,"b":[2]}"#],
            Some(&[r#"[["a"],["b",0]]"#]),
        ),
    ]);
}

/// jq 1.8's scanner reads a number as `([0-9]+(\.[0-9]*)?|\.[0-9]+)` plus an
/// exponent, maximal munch: `.5` is 0.5 and `1.` is 1. arb read `.5` as a field
/// named `5` and stopped `1.` before its point. `1.foo` is the literal `1.`
/// then `foo`, which both engines refuse.
#[test]
fn a_number_may_start_or_end_with_its_point() {
    run_table(&[
        ("[.1, .5e1, 1., 1.e2, 2.50]", &["null"]),
        (".a + .5", &[r#"{"a":1}"#]),
        (". * .5", &["3"]),
        ("[range(1;3)] | .[1.]", &["null"]),
        ("1.foo", &["null"]),
    ]);
}

/// Four builtins brought to jq 1.8.2's answers, messages included (a caught
/// error's text is a value, so `try … catch .` byte-diffs it):
///  * `abs` is `if . < 0 then -. else . end`, so `null` and booleans -- which
///    sort below every number -- raise instead of passing through;
///  * a slice truncates a fractional start and rounds a fractional end UP, the
///    same when reading, assigning and deleting it;
///  * `implode` writes U+FFFD for a code point that is no Unicode scalar value,
///    and names a non-number element in its error;
///  * assigning to a string slice, or slicing a scalar, raises jq's words.
#[test]
fn abs_slices_and_implode_answer_as_jq_does() {
    run_table(&[
        (
            "[.[] | try abs catch .]",
            &[r#"[null,true,false,"a",[],{},-2,-0]"#],
        ),
        (".[1.2:3.5], .[:2.1], .[-2.5:]", &["[1,2,3,4,5]"]),
        (".[1.5:3.5]", &[r#""abcdef""#]),
        (".[1.2:3.5] = [\"x\"]", &["[1,2,3,4,5]"]),
        ("del(.[0.5:1.5])", &["[1,2,3,4,5]"]),
        ("implode | explode", &["[65,1114112,-1,55296,56320,65.7]"]),
        ("try implode catch .", &[r#"["a"]"#]),
        ("try implode catch .", &["[null]"]),
        (r#"try (.[1:] = "x") catch ."#, &[r#""abc""#]),
        ("try (.[1:2] = [1]) catch .", &["{}"]),
        ("try (.[1:] = [1]) catch .", &["5"]),
    ]);
}

/// `tonumber` keeps the string as the number's literal, as jq 1.8 does, so it
/// prints in decNumber's canonical form: `"1.000"` is `1.000`, `"1e2"` is
/// `1E+2`, `"-0"` is `-0`. It printed the double (`1`, `100`, `0`).
#[test]
fn tonumber_keeps_the_literal() {
    run_table(&[
        (
            "map(tonumber)",
            &[r#"["1.000","1e2","0.10","-0","1.5e300","5.",".5","+1"]"#],
        ),
        (
            "tonumber, (tonumber + 1), (tonumber | tostring)",
            &[r#""1.000""#],
        ),
        ("tonumber", &[r#""123456789012345678901234567890""#]),
    ]);
}

/// jq compares a NaN as `null` against a number, so it sits below every
/// number, itself included: `nan < nan` and `nan != nan` are true, `[1,nan] |
/// min` is the NaN. arb's `partial_cmp` fallback made two NaNs EQUAL and a NaN
/// equal to every number. Sorting still gets a total order.
#[test]
fn nan_compares_below_every_number_itself_included() {
    run_table(&[
        (
            "[nan < nan, nan > nan, nan == nan, nan != nan, nan < 1, nan >= 1, nan < -infinite]",
            &["null"],
        ),
        (
            "[nan < null, nan > null, [nan] == [nan], {a: nan} == {a: nan}]",
            &["null"],
        ),
        (
            "[nan, 1, nan, -1] | sort, unique, (min | isnan), max",
            &["null"],
        ),
        ("[nan, nan, 1] | group_by(.) | length", &["null"]),
        ("[{a: nan}, {a: 1}] | sort_by(.a) | map(.a)", &["null"]),
    ]);
}

/// `join` stringifies only booleans and numbers, as jq 1.8's definition does;
/// an array or object element is ADDED to the string and so raises. arb ran
/// every non-string through `tojson` and joined `[[1]]` to `"[1]"`.
#[test]
fn join_refuses_an_array_or_object_element() {
    run_table(&[
        (r#"try join(",") catch ."#, &["[[1]]"]),
        (r#"try join(",") catch ."#, &[r#"["a",{"b":1}]"#]),
        (r#"join("-")"#, &[r#"[true,1.5,null,"x",1.000]"#]),
    ]);
}

/// `fromjson` reads the non-finite literals jq's reader takes -- `nan`, `inf`,
/// `infinity`, any case, signed -- and still refuses `nan1`. It refused all of
/// them.
#[test]
fn fromjson_reads_non_finite_literals() {
    run_table(&[
        (
            r#"["nan", "[nan,1]", "{\"a\":NaN}", "-nan", "Infinity", "[-Infinity]", "inf", "nan1"] | map(try fromjson catch "err")"#,
            &["null"],
        ),
        (r#""nan" | fromjson | isnan"#, &["null"]),
    ]);
}

/// `@base64d` as jq 1.8.2's `f_format` decodes: it stops at the first `=`,
/// refuses any byte outside the alphabet (a newline included) with the input
/// named, reports one leftover character as `trailing base64 byte found`, and
/// repairs bad UTF-8 the way `jv_string_sized` does -- a sequence cut off by
/// the end takes the rest with it, so `null | @base64d` is two U+FFFD, not
/// two and an `e`.
#[test]
fn base64d_decodes_and_refuses_as_jq_does() {
    run_table(&[
        (
            r#"map(try @base64d catch .)"#,
            &[r#"["YW=Jj","YWJj\n","YW Jj","Y","hello","!!!","YQ=","YWJj===="]"#],
        ),
        ("@base64d | explode", &["null"]),
        (
            r#"map(@base64d | explode)"#,
            &[r#"["/w==","wKA=","7aCA","4oKs"]"#],
        ),
    ]);
}

/// Two spellings that reached the wrong engine. `.1` is jq's number `0.1`, and
/// `.a.1` a syntax error, because a jq identifier cannot start with a digit --
/// the path layer read both as a key named `1` and answered `null` and `7`.
/// `@NAME "…"` is a format string whatever the name, and jq decides at run
/// time whether it knows the format -- `@foo "lit"` is `"lit"` -- where arb
/// sent every name outside the nine it knows to the xpath front-end, which
/// refused a string after an attribute step.
#[test]
fn a_dot_digit_is_a_number_and_any_format_string_is_jq() {
    let obj: &[&str] = &[r#"{"a":{"1":7},"a.1":3}"#];
    run_table(&[
        (".1", obj),
        (".5 * 2", obj),
        (".a.1", obj),
        (r#".["a.1"]"#, obj),
        (r#".a["1"]"#, obj),
        (r#"@foo "lit""#, obj),
        (r#"@foo  "x""#, obj),
        (r#"[@foo "lit", 1]"#, obj),
        (r#"@foo "a\(.)b""#, obj),
    ]);
}

/// A builtin whose parameter is a jq VALUE runs once per value its argument
/// yields. C builtins (`has`, `split/1`, `setpath`, libm) vary their LAST
/// argument slowest; `range`, a `def` with `$` parameters, varies its FIRST
/// slowest. arb took only the first value of every argument, and an empty
/// argument raised `argument produced no value` where jq yields nothing.
#[test]
fn value_parameters_enumerate_every_combination() {
    let nil: &[&str] = &["null"];
    run_table(&[
        (r#". | {"a":1} | [has("a","b")]"#, nil),
        (r#". | "a,b;c" | [split(",",";")]"#, nil),
        (r#". | {"a":1} | [setpath(["a"],["b"]; 5,6)]"#, nil),
        (". | [pow(2,3;2,4)]", nil),
        (". | [fma(1,2;3;4,5)]", nil),
        (". | [range(1,2;3,4)]", nil),
        (". | [range(0;4,6;2,3)]", nil),
        (r#". | [0 | strftime("%Y","%m")]"#, nil),
        (r#". | [{"a":1} | has(empty)]"#, nil),
        (r#". | [[1,2] | contains([1],[3])]"#, nil),
    ]);
}

/// The translator's container stages (`.[]`, `map`, `to_entries`, `flatten`,
/// `add`) read lines through serde, which re-sorted an object's keys and
/// reprinted every number literal from its double: `map(.)` over
/// `{"b":1.50,"a":2.0}` was `[2.0,1.5]` where jq prints `[1.50,2.0]`.
#[test]
fn container_stages_keep_key_order_and_number_literals() {
    let obj: &[&str] = &[r#"{"b":1.50,"a":2.0}"#];
    let arr: &[&str] = &["[1.50,2.0,1E1000,100000000000000000001]"];
    run_table(&[
        (".[]", obj),
        ("map(.)", obj),
        (". | to_entries", obj),
        (".[]", arr),
        ("map(.)", arr),
        (". | flatten", arr),
        (". | add", &["[1.50]"]),
        (". | add", &[r#"[[1.50],[2.0]]"#]),
    ]);
}

/// jq 1.8 negates a number that still carries its literal as a decNumber, so
/// the literal survives the minus and a zero stays unsigned.
#[test]
fn negation_keeps_the_number_literal() {
    run_table(&[
        (
            ". | [-0, -0.0, (-1.50|-.), -1E1000, -100000000000000000001]",
            &["null"],
        ),
        (". | map(-.)", &["[0.0, -0.0, 0, 1.0, 13911860366432393]"]),
    ]);
}

/// jq's `f_match` resumes at a match's END (one character past an empty one)
/// while `start <= length`, so an empty match right after a non-empty one, and
/// at the very end, both count.
#[test]
fn global_regex_takes_empty_matches_after_a_match() {
    let s: &[&str] = &[r#""baaab""#];
    run_table(&[
        (r#". | gsub("a*";"X")"#, s),
        (r#". | [match("a*";"g") | [.offset,.length]]"#, s),
        (r#". | [splits("a*")]"#, s),
        (r#". | [match("(?<n>)(x)?";"g")]"#, &[r#""ab""#]),
    ]);
}

/// `{$name: P}` binds `$name` to `.name` and ALSO destructures that value with
/// `P`. arb used the variable's current VALUE as the key.
#[test]
fn object_pattern_variable_key_is_its_name() {
    let obj: &[&str] = &[r#"{"a":[1,2],"k":7}"#];
    run_table(&[
        (r#". | "a" as $k | . as {$k:$v} | [$k,$v]"#, obj),
        (". as {$a: [$x, $y]} | [$a,$x,$y]", obj),
    ]);
}

/// `fromjson` is jq's own reader (`jv_parse.c`), not strict JSON: it takes
/// `01`, `.5`, `Infinity` and `-sNaN`, refuses `nan1` and `1e`, and words every
/// refusal with a line and column (`Unfinished JSON term at EOF at line 1,
/// column 4`). arb's strict reader answered the first group with errors and the
/// second with messages jq never prints.
#[test]
fn fromjson_accepts_and_refuses_as_jq_reader_does() {
    run_table(&[
        (
            "map(try fromjson catch .)",
            &[r#"["01","1.",".5","+1","1.e5","-0.0","Infinity","-sNaN","nan1","1e","0x1"]"#],
        ),
        (
            "map(try fromjson catch .)",
            &[r#"["[1,2","1 2","{\"a\"}","[1,]","[1:2]",",","tru e","{\"a\":[}","[1,2]]","  "]"#],
        ),
        (
            "map(try fromjson catch .)",
            &[r#"["\"\\uD800\"","\"\\uDC00\"","\"\\q\"","{\"a\":1,\"a\":2}","\"a\tb\""]"#],
        ),
    ]);
}

/// jq 1.8.2 is a decNumber build: two number LITERALS compare as decimals, so
/// integers past 2^53 that share a double still sort and compare apart; a
/// COMPUTED negative zero prints `-0` while a negated literal zero is `0`.
#[test]
fn literals_compare_as_decimals_and_zero_keeps_jq_sign() {
    run_table(&[
        (
            ". | sort",
            &["[100000000000000000003,100000000000000000001,-100000000000000000001,1E1000,-1E1000,0,-0,0.0]"],
        ),
        (
            ".[0] == .[1], .[0] > .[1], (.[0] == .[1] + 0), unique",
            &["[100000000000000000001,100000000000000000000]"],
        ),
        (
            "map(-.), map(. * -1), (.[2] | -.), [(1 - 1) | -.]",
            &["[0, 5, -0]"],
        ),
        ("try ({} % 1) catch ., have_decnum", &["null"]),
    ]);
}

/// parser.y: `"try" Expr "catch" Expr` binds tighter than every binary
/// operator, so each side is one Term — and `'-' Term` is a Term.
#[test]
fn try_and_catch_bodies_may_be_negations() {
    run_table(&[
        ("try -. catch .", &["\"foo\""]),
        ("try -.? catch .", &["\"foo\""]),
        ("try -1", &["null"]),
        ("try error catch -.", &["3"]),
        ("[try -.[] catch .]", &["[1,\"a\",2]"]),
        ("try -. + 1 catch .", &["5"]),
    ]);
}

/// parser.y: `BINDING ':' DictExpr` keys the entry by the variable's VALUE;
/// only the bare `BINDING` form keys by its name.
#[test]
fn object_variable_key_with_a_value_uses_the_variable_value() {
    run_table(&[
        (
            "1 as $x | \"2\" as $y | \"3\" as $z | { $x, as, $y: 4, ($z): 5, if: 6, foo: 7 }",
            &["{\"as\":8}"],
        ),
        ("\"k\" as $k | {$k: 1, $k}", &["null"]),
        ("1 as $y | {$y: 2}", &["null"]),
    ]);
}

/// `gen_builtin_list` lists with `block_list_funcs(builtins, 1)`, which omits
/// every name starting with `_`.
#[test]
fn builtins_omits_underscore_names() {
    run_table(&[("builtins | any(.[:1] == \"_\")", &["null"])]);
}

/// `jv_delpaths` sorts the paths and `jv_dels` removes every key of one array
/// at once, against the ORIGINAL indices: a negative index and a slice name
/// the positions they named before anything was removed, and a NaN index
/// deletes nothing.
#[test]
fn delpaths_removes_keys_against_original_indices() {
    const TEN: &[&str] = &["[0,1,2,3,4,5,6,7,8,9]"];
    run_table(&[
        ("del(.[1], .[-6], .[2], .[-3:9])", TEN),
        ("del(.[nan])", TEN),
        ("del(.[nan,nan])", TEN),
        ("del(.[-1,9], .[1.7], .[-1.5])", TEN),
        ("del(.[2:4], .[3:5])", TEN),
        ("delpaths([[5],[0],[-1]])", TEN),
        ("try delpaths([[\"a\"],[0]]) catch .", &["{\"a\":1}"]),
    ]);
}

/// parser.y gives `?` two meanings: right after an index step it is
/// INDEX_OPT/EACH_OPT, which suppresses only that step's own error — an error
/// in the base or the key still raises — while `Term '?'` anywhere else is a
/// whole `try`.
#[test]
fn a_question_mark_after_an_index_suppresses_only_that_step() {
    const A1: &[&str] = &["{\"a\":1}"];
    run_table(&[
        ("try (.[error(\"x\")]?) catch \"caught\"", A1),
        ("try (\"x\" | .a.b?) catch \"c\"", A1),
        ("try (\"x\" | .a[]?) catch \"c\"", A1),
        ("try (.a[error(\"y\")]?) catch .", A1),
        ("try (\"x\" | (.a.b)?) catch \"c\"", A1),
        ("try (\"x\" | .a?.b) catch \"c\"", A1),
        ("[.[]?.x?], [.a[1:]?], [path(.a?, .x[]?)], .a??", A1),
    ]);
}

/// `jv_dump_string_trunc` cuts a 30-BYTE dump at byte 25 (26 without an
/// opening delimiter), backed up to a UTF-8 character start, then appends
/// `...` and the closing delimiter — so a multi-byte string loses whole
/// characters and a long number gets no delimiter.
#[test]
fn error_values_truncate_by_bytes_as_jv_dump_string_trunc() {
    run_table(&[
        (
            "\"x\" * range(0; 12; 2) + \"☆\" * 8 | try -. catch .",
            &["null"],
        ),
        (
            "try (. + \"x\") catch .",
            &["123456789012345678901234567890"],
        ),
        ("try (\"é\" * 20 | -.) catch .", &["null"]),
    ]);
}

/// A non-path filter inside `path(…)` does not refuse at once: its value
/// travels on with the path broken (src/execute.c's `path_intact`), and the
/// refusal names the step that next needed the path — an index, an
/// iteration, or the end of `path`. A value identical to the one at the path
/// keeps the path intact.
#[test]
fn a_broken_path_refuses_where_the_path_is_next_needed() {
    const AB: &[&str] = &["{\"a\":[{\"b\":0}]}"];
    run_table(&[
        ("try path(.a | map(select(.b == 0)) | .[0]) catch .", AB),
        ("try path(.a | map(select(.b == 0)) | .c) catch .", AB),
        ("try path(.a | map(select(.b == 0)) | .[]) catch .", AB),
        (
            "try ((map(select(.a == 1))[].b) = 10) catch .",
            &["[{\"a\":0},{\"a\":1}]"],
        ),
        (
            "try ((map(select(.a == 1))[].a) |= .+1) catch .",
            &["[{\"a\":0},{\"a\":1}]"],
        ),
        ("[try path(.a, (1|.b)) catch .]", AB),
        ("try path([range(100)]) catch .", AB),
        ("try path(\"abc\" | .[0:1]) catch .", AB),
        ("try path(1 | .a?) catch .", AB),
        ("try path(1 | ..) catch .", AB),
        ("[path(1 | select(false) | .a)]", AB),
        (
            "[path(.a[0].b | tostring)], [path(.a[0] | true)]",
            &["{\"a\":[{\"b\":\"s\"}]}"],
        ),
    ]);
}

/// jq tracks a path through `label`, `reduce`, `foreach` and `try … catch`
/// as well (gen_label/gen_reduce/gen_foreach/gen_try), which is what makes
/// the builtin.jq definitions of `limit`, `skip` and `nth` path expressions.
/// `last(f)` (gen_last_1) backtracks every output, so its value travels with
/// the state it began in.
#[test]
fn label_reduce_foreach_and_try_are_tracked_as_paths() {
    const AB: &[&str] = &["{\"a\":{\"b\":0},\"b\":2}"];
    run_table(&[
        (
            "[path(label $f | .a, break $f)], [path(label $f | .a, .b)]",
            AB,
        ),
        (
            "[path(limit(1; .a, .b))], [path(nth(1; .a, .b))], [path(skip(1; .a, .b))]",
            AB,
        ),
        ("del(limit(1; .[]))", AB),
        ("[path(foreach (.a, .b) as $x (0; . + 1; $x))]", AB),
        ("try [path(reduce (1, 2) as $x (.; .a))] catch .", AB),
        ("try [path(reduce 1 as $x (.; .a))] catch .", AB),
        ("[path(try .a catch .b)]", AB),
        ("try [path(try error(\"x\") catch .b)] catch .", AB),
        ("[path(last(.))], (try [path(last(.a, .b))] catch .)", AB),
        ("try [path(last(1 | .a))] catch .", AB),
    ]);
}

/// EACH on `null` refuses in a path expression exactly as it does on a value
/// (`Cannot iterate over null`); only `.[]?` (EACH_OPT) yields nothing.
#[test]
fn iterating_null_in_a_path_refuses() {
    run_table(&[
        ("try [path(.[])] catch .", &["null"]),
        ("try (.[] |= 1) catch .", &["null"]),
        ("try del(.[]) catch .", &["null"]),
        ("try (.a[] = 1) catch .", &["{}"]),
        ("[path(.[]?)], [paths]", &["null"]),
    ]);
}

/// jq reads its input with `jv_parse.c`, which takes `nan`/`NaN`/`Infinity`,
/// `01`, `.5`, `+1` and a leading byte-order mark as JSON — so such a line is
/// a value, not text.
#[test]
fn input_lines_are_read_by_jq_s_own_parser() {
    run_table(&[
        (".", &["[1,NaN,nan,Infinity,-Infinity,-NaN]"]),
        (".[] = 1", &["[1,null,Infinity,-Infinity,NaN,-NaN]"]),
        ("tojson | fromjson", &["{\"a\":nan}"]),
        ("[., type]", &[".5", "01", "+1", "1."]),
        (".", &["\u{feff}\"byte order mark\""]),
    ]);
}

/// `f_string_implode` refuses a NaN code point as it refuses a non-number.
#[test]
fn implode_refuses_a_nan_code_point() {
    run_table(&[("map(try implode catch .)", &["[123,[\"a\"],[nan]]"])]);
}

/// parser.y's `ArrayPats` is one or more `Pattern`s, so an empty array
/// pattern is a syntax error wherever a pattern goes.
#[test]
fn an_empty_array_pattern_is_a_syntax_error() {
    run_table(&[
        (". as [] | null", &["[1]"]),
        ("reduce . as [] (0; .)", &["[1]"]),
        (". as [$a] ?// [] | 1", &["[1]"]),
        (". as [$a, [$b]] | [$a, $b]", &["[1,[2]]"]),
    ]);
}

/// f_match's "Empty capture" branch builds a group that matched the empty
/// string inside a non-empty match with `offset, string, length` key order.
#[test]
fn an_empty_capture_keeps_f_match_key_order() {
    run_table(&[
        ("\"a\",\"b\",\"c\" | match(\"(?<x>a?)?b?\")", &["null"]),
        ("[match(\"(a*)b\"; \"g\")]", &["\"bab\""]),
    ]);
}

/// `jv_setpath` reads the child with `jv_get` first, then `jv_set` refuses a
/// key it cannot store (`Cannot update field at array index of array`)
/// before the rest of the path is set.
#[test]
fn setpath_refuses_an_unstorable_key_as_jv_set_does() {
    run_table(&[
        ("try [\"OK\", setpath([[1]]; 1)] catch [\"KO\", .]", &["[]"]),
        ("try setpath([[1], \"a\"]; 1) catch .", &["[]"]),
        ("try (.[[0]] = 1) catch .", &["[]"]),
        ("try setpath([true]; 1) catch .", &["[]"]),
    ]);
}

/// `jv_get` reads `null` as `null` only under a string, number or slice key;
/// a null, boolean or array key refuses — and `jv_getpath` applies it at
/// every step, so a `null` midway does not end the walk early.
#[test]
fn indexing_null_refuses_a_null_boolean_or_array_key() {
    run_table(&[
        ("try (null | .[null]) catch .", &["null"]),
        ("try (null | path(.[null])) catch .", &["null"]),
        ("try getpath([\"a\", true]) catch .", &["{\"a\":null}"]),
        ("try getpath([\"a\", [1]]) catch .", &["{\"a\":null}"]),
        (
            "getpath([\"a\", \"b\"]), getpath([\"a\", {\"start\": 0}])",
            &["{\"a\":null}"],
        ),
    ]);
}

/// `f_string_indexes` refuses a non-string input as "cannot be searched, as it
/// is not a string" and a non-string needle as "is not a string".
#[test]
fn strindices_refuses_with_f_string_indexes_wording() {
    run_table(&[
        ("try _strindices(\"abc\") catch .", &["123"]),
        ("try _strindices(123) catch .", &["\"abc\""]),
        ("_strindices(\"a\")", &["\"banana\""]),
    ]);
}
