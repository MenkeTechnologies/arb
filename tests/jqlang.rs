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
const RECS: &[&str] = &[r#"[{"id":1,"n":"a","v":10},{"id":2,"n":"b","v":5},{"id":3,"n":"a","v":7}]"#];

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
        ("def fact: if . <= 1 then 1 else . * (. - 1 | fact) end; 5 | fact", OBJ),
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
        ("group_by(.n) | map({n: .[0].n, total: (map(.v) | add)})", RECS),
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
        ("[.c[] | try (if . == 2 then error(\"two\") else . end) catch \"caught\"]", OBJ),
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
        "0", "-0", "0.0", "1.50", "3.0", "0.10", "1e2", "1E+2", "12e3", "0.000001",
        "0.0000001", "5e-3", "100000000000000000000000", "1.7976931348623157e308",
        "-1.7976931348623157e308", "2.2250738585072014e-308", "9007199254740993",
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
        let big = a.parse::<f64>().is_ok_and(|v| v.abs() > 9_007_199_254_740_992.0);
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
    let out = arb_run(r#"[try test("[") catch "ERR"]"#, &[r#""a""#, r#""b""#, r#""c""#]);
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
/// only the jq CALL spelling reaches the jq engine. This is the rule that keeps
/// `stdlib/json.arb`'s `keys; tally` working.
#[test]
fn a_bare_shared_spelling_stays_the_native_verb() {
    // Native `keys`: one line per key. jq's `keys` is one sorted array.
    assert_eq!(
        arb_run("keys", &[r#"{"b":1,"a":2}"#]).unwrap(),
        vec!["a", "b"]
    );
    assert_eq!(
        arb_run(". | keys", &[r#"{"b":1,"a":2}"#]).unwrap(),
        vec![r#"["a","b"]"#]
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
