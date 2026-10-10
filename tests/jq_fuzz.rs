//! Seeded differential fuzz for arb's jq engine.
//!
//! A deterministic generator builds random JSON documents and random jq
//! programs from a grammar of the builtins and operators, runs each program
//! through arb and through the real `jq`, and byte-diffs the output. Every
//! program is run twice: bare, and wrapped in `try (…) catch .` so that error
//! MESSAGES are compared as well as error-vs-answer.
//!
//! The same seed always produces the same corpus, so a divergence reproduces
//! exactly. `ARB_FUZZ_SEED` and `ARB_FUZZ_CASES` override the defaults for a
//! longer local run. Skips (loudly) when `jq` is absent or not the 1.8 line.
//!
//! A program that reads the stream (`input`, `inputs`, `input_line_number`) is
//! run over one to three documents, and only in its `try` form: jq exits with
//! the status of the LAST document, so an uncaught error on an earlier one is
//! not a difference of answers.
//!
//! Every divergence found here is pinned, with the output jq measured, in
//! `jq_round2.rs`, which needs no `jq` to run.

mod common;

use common::{arb_run, jq_run, reference_ok};

/// xorshift64*: small, dependency-free and identical on every platform.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

const SCALARS: &[&str] = &[
    "null",
    "true",
    "false",
    "0",
    "1",
    "-1",
    "2",
    "3",
    "10",
    "-7",
    "1.5",
    "-0.25",
    "1e3",
    "100000000000000000000",
    "1.0",
    "0.1",
    "\"\"",
    "\"abc\"",
    "\"a b\"",
    "\"x,y\"",
    "\"A1b2\"",
    "\"é\"",
    "\"日本\"",
    "\"10\"",
    "\"a\\nb\"",
    "1e1000",
    "-1e1000",
    "0.00001",
    "1.7976931348623157e308",
    "5e-324",
    "3.0",
    "100000000000000000001",
    "9007199254740993",
    "-0",
    "0.1e-5",
    "\"\\u0000\"",
    "\"it's\"",
    "\"<&>\"",
    "\"aGk=\"",
    "[\"a\",1]",
];
const KEYS: &[&str] = &["a", "b", "c", "é", ""];

fn gen_json(r: &mut Rng, depth: usize) -> String {
    match if depth == 0 { 0 } else { r.below(5) } {
        0 | 1 => r.pick(SCALARS).to_string(),
        2 | 3 => {
            let items: Vec<String> = (0..r.below(5)).map(|_| gen_json(r, depth - 1)).collect();
            format!("[{}]", items.join(","))
        }
        _ => {
            let items: Vec<String> = (0..r.below(4))
                .map(|_| format!("\"{}\":{}", r.pick(KEYS), gen_json(r, depth - 1)))
                .collect();
            format!("{{{}}}", items.join(","))
        }
    }
}

const NULLARY: &[&str] = &[
    ".",
    ".a",
    ".b",
    ".[0]",
    ".[-1]",
    ".[1:]",
    ".[:2]",
    ".[]?",
    ".a?",
    "..",
    "length",
    "keys",
    "keys_unsorted",
    "type",
    "tostring",
    "tojson",
    "add",
    "sort",
    "reverse",
    "unique",
    "min",
    "max",
    "first",
    "last",
    "to_entries",
    "from_entries",
    "values",
    "nulls",
    "numbers",
    "strings",
    "arrays",
    "objects",
    "scalars",
    "iterables",
    "floor",
    "ceil",
    "round",
    "sqrt",
    "fabs",
    "not",
    "ascii_downcase",
    "ascii_upcase",
    "flatten",
    "any",
    "all",
    "tonumber",
    "utf8bytelength",
    "explode",
    "implode",
    "tostream",
    "paths",
    "transpose",
    "ltrimstr(\"a\")",
    "rtrimstr(\"c\")",
    "startswith(\"a\")",
    "endswith(\"c\")",
    "split(\",\")",
    "join(\",\")",
    "test(\"a\")",
    "indices(1)",
    "index(\"a\")",
    "has(\"a\")",
    "has(0)",
    "contains(\"a\")",
    "getpath([\"a\"])",
    "del(.[0])",
    "del(.a)",
    "to_entries[]?",
    "@json",
    "@text",
    "@csv",
    "@tsv",
    "@html",
    "@uri",
    "@sh",
    "@base64",
    "@base64d",
    "tojson|fromjson",
    "range(3)",
    "trim",
    "ltrim",
    "abs",
    "splits(\",\")",
    "env|type",
    "input_line_number",
    "infinite",
    "nan|isnan",
    "@json \"v=\\(.)\"",
    "walk(.)",
    "min_by(.)",
    "group_by(.)",
    "unique_by(length)",
    "tojson|length",
    "limit(2; .[]?)",
    "getpath([\"a\",\"b\"])",
    "paths(type == \"number\")",
    "splits(\"a\")",
    "ascii_downcase?",
    "[.[]?] | length",
    "to_entries | map(.key)",
    "with_entries(.)",
    "map_values(.)",
    "pick(.a)",
    "have_literal_numbers",
    "significand?",
    "tojson | ascii_downcase",
    // Generators and early exits.
    "limit(0; .[]?)",
    "[limit(1; .[]?, 9)]",
    "[limit(2; range(5))]",
    "[first(range(5;10))]",
    "[first(empty)]",
    "[range(0; 3)]",
    "[range(5; 0; -2)]",
    "[range(0; 1; 0.25)]",
    "[range(1, 2; 3, 4)]",
    "[range(0; 3; 0)]",
    "[limit(3; repeat(1))]",
    "until(true; .)",
    "[while(false; .)]",
    "[foreach (1, 2, 3) as $x (0; . + $x, . * 10; [$x, .])]",
    "[foreach (1, 2) as $x (0, 100; . + $x)]",
    "[foreach .[]? as $x (0; empty; .)]",
    "reduce (1, 2, 3) as $x (0; ., . + $x)",
    "[reduce (1, 2) as $x (0, 10; . + $x)]",
    "reduce .[]? as $x (0; empty)",
    "[label $f | .[]?, break $f]",
    "[label $a | label $b | 1, break $a, 2]",
    "[.[]? | label $l | if type == \"number\" then break $l else . end]",
    "[label $f | (1, break $f, 2), 3]",
    // error(null) and catch.
    "try error(null) catch .",
    "try error catch .",
    "[.[]? | try error(null) catch .]",
    "[try error(.)]",
    "(try error(null) catch .) | type",
    "error(null)?",
    "try (try error(null) catch error) catch .",
    // @format strings.
    "@base32",
    "@base32d",
    "[.[]? | @sh]",
    "[.[]? | try @sh catch .]",
    "[.[]? | try @csv catch .]",
    "@sh \"echo \\(.)\"",
    "@csv \"\\(.)\"",
    "@tsv \"\\(.)\"",
    "@uri \"u=\\(.)\"",
    "@html \"<\\(.)>\"",
    "@base64 \"\\(.)\"",
    "@base64d \"\\(.)\"",
    "@json \"j=\\(.)\"",
    // Strings.
    "ascii_downcase | explode | implode",
    "[.[]? | tonumber?] | implode",
    "try implode catch .",
    "trimstr(\"a\")",
    "ltrimstr(1)",
    "rtrimstr(\"\")",
    "splits(\"a+\")",
    "[splits(\"a+\"; \"g\")]",
    "split(\"a\"; null)",
    "split(\"a\"; \"gi\")",
    "split(\"\"; null)",
    // Regular expressions.
    "test(\"A\"; \"i\")",
    "test(\"a.c\"; \"s\")",
    "test(\"A B\"; \"xi\")",
    "test(\"a\"; \"q\")",
    "test(\"(\")",
    "test(\"(?=a)\")",
    "test(\"(a)\\\\1\")",
    "[match(\"a\"; \"g\") | .offset]",
    "[match(\"\"; \"g\") | .offset]",
    "[match(\"(?=a)\"; \"g\") | .offset]",
    "[match(\"a*\"; \"g\") | [.offset, .length]]",
    "[match(\"\\\\w+\"; \"g\") | .string]",
    "[match(\"(a)|(b)\"; \"g\") | .captures | map(.string)]",
    "[match(\"é\"; \"g\") | .offset]",
    "capture(\"(?<x>a)(?<y>b)?\")",
    "[capture(\"(?<x>[a-z])\"; \"g\")]",
    "[scan(\"a\")]",
    "[scan(\"(a)(b)?\")]",
    "[scan(\".\"; \"g\")]",
    "sub(\"a\"; \"b\")",
    "gsub(\"a\"; \"b\")",
    "gsub(\"\"; \"-\")",
    "gsub(\"^\"; \">\")",
    "gsub(\"(?<x>.)\"; \"\\(.x)\\(.x)\")",
    "[gsub(\"a\"; \"b\", \"c\")]",
    "gsub(\"a\"; \"b\"; \"i\")",
    "sub(\"\"; \"x\")",
    "ascii_downcase | test(\"é\")",
    // Paths.
    "getpath([])",
    "getpath([0])",
    "getpath([null])",
    "getpath([\"a\", 0, \"b\"])",
    "getpath([\"a\"], [\"b\"])",
    "[paths]",
    "[paths(type == \"number\")]",
    "[paths(..)]",
    "pick(.a)",
    "pick(.[0])",
    "pick(.a.b)",
    "pick(.a, .b)",
    "pick(empty)",
    "pick(.[1:])",
    "pick(first)",
    "del(.[0], .a)",
    "del(.[]?)",
    "del(..)",
    "delpaths([[0], [\"a\"]])",
    "delpaths([[\"a\", \"b\"]])",
    "delpaths([[]])",
    "delpaths([[null]])",
    "setpath([0]; 1)",
    "setpath([\"a\", \"b\"]; 1)",
    "setpath([]; 1)",
    "setpath([-1]; 1)",
    "setpath([{\"start\": 1, \"end\": null}]; [])",
    "to_entries",
    "[tostream]",
    "fromstream(tostream)",
    "[tostream] | fromstream(.[])",
    "[1 | truncate_stream([[0], 1], [[1, 0], 2], [[1, 0]], [[1]])]",
    "path(..)",
    "[path(..)]",
    "path(.a, .[0])",
    "[path(.[]?)]",
    // Number formatting.
    "1.0",
    "1e1000",
    "-1e1000",
    "100000000000000000000",
    "100000000000000000001",
    "[1.0, 1e2, 0.1e1] | tojson",
    "nan",
    "infinite",
    "-infinite",
    "[nan] | tojson",
    "[infinite, -infinite] | tojson",
    "nan | tostring",
    "nan < 1",
    "[nan, 1] | sort",
    "tojson | fromjson",
    ". + 0",
    ". * 1",
    "-.",
    "[.] | tojson",
    // Sorting on several keys.
    "sort_by(.a, .b)",
    "group_by(.a, .b)",
    "unique_by(.a, .b)",
    "min_by(.a, .b)",
    "max_by(.a, .b)",
    "sort_by(type, .)",
    "group_by(type, length)",
    // Location and input.
    "$__loc__",
    "$__loc__.line",
    "input",
    "[inputs]",
    "[., input]",
    "first(inputs)",
    "try input catch .",
    "[limit(1; inputs)]",
    "reduce inputs as $x (0; . + 1)",
    "isempty(inputs)",
    "input_line_number",
    // Path-mode corners.
    "[path(walk(.))]",
    "try path(walk(.)) catch .",
    "try path(map_values(.)) catch .",
    "try path(from_entries) catch .",
    "try path(sub(\"a\"; \"b\")) catch .",
    "try path(gsub(\"a\"; \"b\")) catch .",
    "try path(splits(\"a\")) catch .",
    "try path(length) catch .",
    "try path(floor) catch .",
    "try path(tojson | fromjson) catch .",
    "try path(.[0] | length) catch .",
    "try path(.[0] | floor) catch .",
    "try path(.[0] | tojson | fromjson) catch .",
    "try path(. as [$a] | $a) catch .",
    "try path(. as [$a, $b] | $a) catch .",
    "try path(. as {a: $x} | .a) catch .",
    "try path(.[0] as [$a] | $a) catch .",
    "try path(.a |= 1) catch .",
    "try path(. += 1) catch .",
    "try path(.[0] //= 1) catch .",
    "try [path(. as [$a] ?// $a | .)] catch .",
    "[. as [$a] ?// $a | $a]",
    "first(. as [$a] ?// $a | $a)",
];

const UNARY: &[&str] = &[
    "map(%)",
    "select(%)",
    "[%]",
    "sort_by(%)",
    "group_by(%)",
    "unique_by(%)",
    "min_by(%)",
    "max_by(%)",
    "map_values(%)",
    "with_entries(%)",
    "walk(%)",
    "path(%)",
    "try %",
    "(%)?",
    "first(%)",
    "last(%)",
    "limit(2; %)",
    "[limit(3; %)]",
    "any(%)",
    "all(%)",
    "{a: %}",
    "{(\"k\"): %}",
    "[paths(%)]",
    "to_entries | map(%)",
    "del(%)",
    "to_entries[] | %",
    "[.[]? | %]",
    "tojson | %",
    "[%] | length",
    "-(%)",
    "not | %",
    "add(%)",
    "abs | %",
    "ltrimstr(%)",
    "splits(%)",
    "has(%)",
    "contains(%)",
    "inside(%)",
    "indices(%)",
    "getpath(%)",
    "label $l | (%, break $l)",
    "[foreach .[]? as $x (0; . + 1; %)]",
    "reduce .[]? as $x (null; %)",
    "env | %",
    ".. | %",
    "[.[]?] | %",
    "input_line_number | %",
    "tostring | %",
    "ascii_downcase | %",
    "if % then 1 else 2 end",
    "[splits(\",\")?] | %",
    "tojson | fromjson | %",
    "getpath(%)",
    "setpath(%; 1)",
    "setpath([\"a\"]; %)",
    "setpath([0]; %)",
    "delpaths(%)",
    "delpaths([%])",
    "pick(%)",
    "paths(%)",
    "path(%)",
    "[path(%)]",
    "try path(%) catch .",
    "isempty(%)",
    "first(%)",
    "[limit(2; %)]",
    "limit(0; %)",
    "[limit(-1; %)]",
    "try limit(-1; %) catch .",
    "until(true; %)",
    "[while(false; %)]",
    "[label $q | % | ., break $q]",
    "try error(%) catch .",
    "(try error(%) catch .) | tojson",
    "[.[]? | try error(%) catch .]",
    "% | @sh",
    "% | @csv",
    "% | @tsv",
    "% | @uri",
    "% | @html",
    "% | @base64",
    "% | @base64d",
    "% | @json",
    "% | @text",
    "% | try @sh catch .",
    "% | try @csv catch .",
    "% | tojson",
    "% | tostring",
    "% | fromjson?",
    "% | implode?",
    "% | explode?",
    "[%] | implode?",
    "@sh \"x\\(%)y\"",
    "@json \"x\\(%)y\"",
    "@csv \"\\(%)\"",
    "\"a\\(%)b\"",
    "rtrimstr(%)",
    "trimstr(%)",
    "startswith(%)",
    "endswith(%)",
    "split(%)",
    "split(%; null)",
    "join(%)",
    "test(%)",
    "test(\"a\"; %)",
    "match(%)",
    "[match(%; \"g\")] | length",
    "capture(%)",
    "[scan(%)]",
    "[scan(\"a\"; %)]",
    "sub(%; \"x\")",
    "gsub(\"a\"; %)",
    "gsub(%; \"x\")",
    "[splits(%)]",
    "sort_by(%, %)",
    "group_by(%, %)",
    "unique_by(%, %)",
    "[limit(5; range(%))]",
    "[limit(5; range(0; %))]",
    "[limit(5; range(%; 3))]",
    "[limit(3; range(0; 3; %))]",
    "reduce .[]? as $x (0; %)",
    "[foreach .[]? as $x (0; %; .)]",
    "[foreach .[]? as $x (0; .; %)]",
    "walk(%)",
    "try (% as $x | $x) catch .",
    "(% as [$a] | $a)?",
    "(% as {a: $a} | $a)?",
    "(% as [$a] ?// $a | $a)",
    "[% as [$a] ?// $a | $a]",
    "first(% as [$a] ?// $a | $a)",
    "path(% as [$a] | $a)?",
    "[paths(%)]",
    "to_entries | map(%)",
    "[.[]? | %] | tojson",
    "tojson | fromjson | %",
    "keys | %",
];

const BINARY: &[&str] = &[
    "% | %",
    "%, %",
    "% + %",
    "% - %",
    "% * %",
    "% / %",
    "% % %",
    "% == %",
    "% != %",
    "% < %",
    "% >= %",
    "% and %",
    "% or %",
    "% // %",
    "if % then % else % end",
    "[%, %]",
    "{a: %, b: %}",
    "reduce .[]? as $x (%; %)",
    "(%) as $v | %",
    ". as [$p, $q] | %",
    ". as {a: $p} | %",
    "% |= %",
    "% = %",
    "% += %",
    "% //= %",
    "setpath([\"a\"]; %)",
    "limit(%; %)",
    "[range(%; %)]",
    "try % catch %",
    "first(%, %)",
    "[%] - [%]",
    "% as $x | % ",
    "if % then % end",
    "sort_by(%; %)",
    "to_entries | map(select(%) | %)",
    "[.[]? | select(%) | %]",
    "% | tostring | %",
    "(% | length) + (% | length)",
    "% | .[0:2]?",
    "[%] | index(%)",
];

fn gen_filter(r: &mut Rng, depth: usize) -> String {
    if depth == 0 {
        return r.pick(NULLARY).to_string();
    }
    let sub = |r: &mut Rng| gen_filter(r, depth - 1);
    match r.below(10) {
        0..=2 => r.pick(NULLARY).to_string(),
        3..=5 => {
            let inner = sub(r);
            r.pick(UNARY).replace('%', &format!("({inner})"))
        }
        _ => {
            // One freshly generated operand per `%` gap in the template.
            let mut parts = r.pick(BINARY).split('%');
            let mut out = parts.next().unwrap_or_default().to_string();
            for part in parts {
                out.push_str(&format!("({})", sub(r)));
                out.push_str(part);
            }
            out
        }
    }
}

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
fn random_programs_match_jq() {
    if !reference_ok() {
        return;
    }
    let mut rng = Rng(env_or("ARB_FUZZ_SEED", 0x00A2_B5EE_D5EE_D001));
    let cases = env_or("ARB_FUZZ_CASES", 250);
    let mut divergences = Vec::new();
    for _ in 0..cases {
        let doc = gen_json(&mut rng, 3);
        let program = gen_filter(&mut rng, 2);
        // jq's exit status is the LAST input's, and arb's harness aborts on the
        // first error, so a stream is only compared where every error is caught
        // in-program (the `try` form below) — and only for programs that read
        // the stream.
        let reads_stream = program.contains("input");
        let mut input = vec![doc.clone()];
        if reads_stream {
            for _ in 0..rng.below(3) {
                input.push(gen_json(&mut rng, 2));
            }
        }
        if std::env::var_os("ARB_FUZZ_TRACE").is_some() {
            eprintln!("TRACE {program}  <<<  {doc}");
        }
        let mut forms = vec![format!("try ({program}) catch .")];
        if !reads_stream {
            forms.insert(0, format!(". | ({program})"));
        }
        for filter in forms {
            match (arb_run(&filter, &input), jq_run(&filter, &input)) {
                (Ok(a), Some(b)) if a.join("\n") == b.join("\n") => {}
                (Err(_), None) => {}
                (ours, theirs) => divergences.push(format!(
                    "filter: {filter}\n  input: {}\n  arb:   {ours:?}\n  jq:    {theirs:?}",
                    input.join(" ;; ")
                )),
            }
        }
    }
    assert!(
        divergences.is_empty(),
        "{} divergence(s):\n{}",
        divergences.len(),
        divergences.join("\n")
    );
}

/// Divergences the fuzzer (or its path-mode probe) found, pinned against the
/// real `jq` so a regression names the exact construct.
#[test]
fn fuzz_found_divergences_stay_fixed() {
    if !reference_ok() {
        return;
    }
    let probes: &[(&str, &str)] = &[
        // `jv_has`: null has no key of any type, and says so quietly.
        ("try (has(\"a\")) catch .", "null"),
        ("try (has(0)) catch .", "null"),
        ("try (has([])) catch .", "null"),
        // Array patterns bind from the LAST element, so the refusal names the
        // last index and a later generator is the outer loop.
        ("try (. as [$p, $q] | 0) catch .", "1"),
        ("try (. as [$a, $b, $c] | 0) catch .", "1"),
        ("try (. as [[$a], [$b]] | 0) catch .", "[[1],1]"),
        (
            ". as [{(\"a\",\"b\"): $x}, {(\"a\",\"b\"): $y}] | [$x, $y]",
            r#"[{"a":1,"b":2},{"a":3,"b":4}]"#,
        ),
        // Object patterns stay left to right.
        (
            ". as {(\"a\",\"b\"): $x, (\"a\",\"b\"): $y} | [$x, $y]",
            r#"{"a":1,"b":2}"#,
        ),
        // A builtin that returns its input unchanged keeps the path intact.
        ("path(tostring)", "\"Ab\""),
        ("path(@text)", "\"Ab\""),
        ("path(trim)", "\"Ab\""),
        ("path(ltrim)", "\"Ab\""),
        ("path(rtrim)", "\"Ab\""),
        ("try path(trim) catch .", "\" Ab \""),
        // `ascii_downcase`/`ascii_upcase` are `explode | map(…) | implode`.
        ("try path(ascii_downcase) catch .", "\"Ab\""),
        ("try path(ascii_upcase) catch .", "\"\""),
        ("path(ascii_downcase?)", "\"Ab\""),
        ("try path(ascii_downcase) catch .", "5"),
        // `any`/`all` stop at the first deciding element, so later elements
        // are never evaluated.
        ("try any(floor) catch .", "[1,\"a\"]"),
        ("try all(floor) catch .", "[1,\"a\"]"),
        ("any", "[1,\"a\"]"),
        ("all", "[1,null]"),
        // `transpose` is `[range(0; (map(length)|max) // 0) as $j | map(.[$j])]`.
        ("try transpose catch .", "{\"a\":[[false,\"abc\"],null]}"),
        ("try transpose catch .", "{}"),
        ("try transpose catch .", "[\"ab\"]"),
        ("transpose", "[[1],[2,3]]"),
        // An array literal's body runs with path tracking on, so `.[]` over a
        // value that is no longer at the path refuses.
        ("try path(to_entries | map(.)) catch .", "[1]"),
        ("try del(to_entries | map(.)) catch .", "{}"),
        // `f_json_parse` words a non-string input as `only strings can be parsed`.
        ("try fromjson catch .", "5"),
        ("try fromjson catch .", "[1]"),
        ("try fromjson catch .", "null"),
    ];
    for (filter, doc) in probes {
        let input = [doc.to_string()];
        let wrapped = format!(". | ({filter})");
        match (arb_run(&wrapped, &input), jq_run(&wrapped, &input)) {
            (Ok(a), Some(b)) => assert_eq!(a, b, "`{filter}` over {doc}"),
            (Err(_), None) => {}
            (ours, theirs) => panic!("`{filter}` over {doc}: arb {ours:?}, jq {theirs:?}"),
        }
    }
}
