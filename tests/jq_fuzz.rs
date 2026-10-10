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
        if std::env::var_os("ARB_FUZZ_TRACE").is_some() {
            eprintln!("TRACE {program}  <<<  {doc}");
        }
        for filter in [
            format!(". | ({program})"),
            format!("try ({program}) catch ."),
        ] {
            let input = [doc.clone()];
            match (arb_run(&filter, &input), jq_run(&filter, &input)) {
                (Ok(a), Some(b)) if a.join("\n") == b.join("\n") => {}
                (Err(_), None) => {}
                (ours, theirs) => divergences.push(format!(
                    "filter: {filter}\n  input: {doc}\n  arb:   {ours:?}\n  jq:    {theirs:?}"
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
