//! Native jq-syntax front-end for `source { … }` bodies. A body command whose
//! first token is a jq literal (starts with `.`, or is a `select(…)`/`map(…)`
//! stage) is handed here verbatim (verb + args, space-joined) and translated to
//! a `Vec<QueryOp>` over arb's existing ops. This is a PRACTICAL subset — path,
//! iterate, index, pipe, select, map, and the builtins arb already implements —
//! not Turing-complete jq. Anything outside the subset is a clean error, never a
//! silent mis-translation.

use crate::jqval::Seg;
use crate::query::{FieldSel, QueryOp};

/// Translate a reconstructed jq command string into arb ops. Splits on top-level
/// `|` (jq pipe) and translates each stage in order.
pub fn translate(src: &str) -> Result<Vec<QueryOp>, String> {
    let mut ops = Vec::new();
    for stage in split_pipe(src)? {
        let stage = stage.trim();
        if stage.is_empty() {
            return Err(format!("jq: empty pipe stage in `{src}`"));
        }
        translate_stage(stage, &mut ops)?;
    }
    // A pipeline that ENDS by emitting its input line verbatim still owes jq's
    // string rendering: `jq -r` prints a top-level JSON string raw, so a line
    // reading `"hello"` is `hello`. Identity contributes no ops at all, and
    // `select(…)`/`values` re-emit `l.clone()`, so those three are exactly the
    // endings that reach the output un-rendered.
    //
    // Appended ONCE, at the END, and only for those endings — never per stage.
    // Both restrictions are load-bearing:
    //   * mid-pipeline it would feed the NEXT stage an unquoted line, and a
    //     non-JSON line is the one input the type checks cannot refuse — so
    //     `"abc" | keys`, which correctly raises `string ("abc") has no keys`
    //     today, would silently pass `abc` through with exit 0. That converts a
    //     hard error into an answer, the exact failure SPEC §8 forbids.
    //   * after a stage that already RENDERED (a path, `.[]`, `map(…)`) it would
    //     unquote a second time: `{"a":"\"q\""}` | `.a` renders `"q"`, whose
    //     quotes are DATA, and a second pass would strip them to `q`.
    // `map(…)` bodies build through `translate_stage` directly, so they are
    // untouched by this and keep rebuilding their array through `jq_array_json`.
    if ops.is_empty() || matches!(ops.last(), Some(QueryOp::NonNull | QueryOp::JqSelect(_))) {
        ops.push(QueryOp::JqRawString);
    }
    Ok(ops)
}

/// Split on `|` that is not inside `(` `)`, `[` `]`, or a double-quoted string.
fn split_pipe(src: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut in_str = false;
    for c in src.chars() {
        match c {
            '"' => {
                in_str = !in_str;
                cur.push(c);
            }
            '(' | '[' if !in_str => {
                depth += 1;
                cur.push(c);
            }
            ')' | ']' if !in_str => {
                depth -= 1;
                cur.push(c);
            }
            '|' if !in_str && depth == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if in_str {
        return Err(format!("jq: unterminated string in `{src}`"));
    }
    out.push(cur);
    Ok(out)
}

/// Translate one pipe-free jq stage, appending its ops.
fn translate_stage(s: &str, ops: &mut Vec<QueryOp>) -> Result<(), String> {
    match s {
        "." => return Ok(()), // identity
        ".[]" => {
            ops.push(QueryOp::JqEach);
            return Ok(());
        }
        "keys" => {
            // jq returns ONE sorted array; arb's native `keys` verb is the
            // line-per-key spelling that `stdlib/json.arb` pipes into `tally`.
            // Reaching THIS arm means the stage came through the jq literal
            // front-end (`. | keys`, `map(keys)`), where jq's shape is the
            // contract — same context gating as `to_entries` vs `entries`.
            ops.push(QueryOp::JqKeys);
            return Ok(());
        }
        "values" => {
            ops.push(QueryOp::NonNull);
            return Ok(());
        }
        "length" => {
            // The STRICT length: jq refuses a boolean, where the native `length`
            // verb falls back to the raw line's character count.
            ops.push(QueryOp::JqLen);
            return Ok(());
        }
        "add" => {
            // jq's fold-from-null `add`, not the native verb: `[]` is `null` here
            // and "" there (SPEC §8), and a mixed-type array raises.
            ops.push(QueryOp::JqAdd);
            return Ok(());
        }
        "flatten" => {
            // jq returns ONE array of leaves; the native `flatten` verb is the
            // line-per-leaf spelling. Same context gating as `keys`/`to_entries`.
            ops.push(QueryOp::JqFlatten);
            return Ok(());
        }
        "to_entries" => {
            // jq returns ONE array; arb's native `entries` verb is the
            // line-per-key spelling. This front-end answers as jq does.
            ops.push(QueryOp::JqEntries);
            return Ok(());
        }
        _ => {}
    }
    if let Some(inner) = fn_call(s, "select") {
        ops.push(QueryOp::JqSelect(parse_expr(inner)?));
        return Ok(());
    }
    if let Some(inner) = fn_call(s, "map") {
        // jq identity: `map(f)` == `[.[] | f]` — including the array rewrap and
        // the per-input scope. Dropping the rewrap (which arb used to do) makes
        // every `map` diverge from jq on shape AND merges two input lines' results
        // into one flat stream. The iterate-WITHOUT-rewrap reading is spelled
        // `.[] | f`, which is jq's own spelling for it and which arb still accepts.
        let mut body = Vec::new();
        for stage in split_pipe(inner)? {
            let stage = stage.trim();
            if stage.is_empty() {
                return Err(format!("jq: empty pipe stage in `{s}`"));
            }
            translate_stage(stage, &mut body)?;
        }
        ops.push(QueryOp::JqMap(body));
        return Ok(());
    }
    if let Some(inner) = fn_call(s, "has") {
        // jq's argument is a VALUE — `has("k")` on an object, `has(0)` on an
        // array — so it is parsed as one rather than string-stripped.
        let arg = inner.trim();
        let key = match crate::jqval::parse(arg) {
            Ok(crate::jqval::Expr::Lit(
                v @ (serde_json::Value::String(_) | serde_json::Value::Number(_)),
            )) => v,
            _ => return Err(format!("jq: has() expects a string or number key: `{s}`")),
        };
        ops.push(QueryOp::HasKey(key));
        return Ok(());
    }
    if s.starts_with('.') && is_pure_path(s) {
        return translate_path(s, ops);
    }
    // Everything else is a jq VALUE expression — `. * 2`, `.a + .b`, `. > 1`,
    // `1 + .a`. `jqval` parses it, and refuses every construct outside the
    // documented subset (`reduce`, `paths`, `//`, `..`, `$ENV`, a bare `not`),
    // which is how the hard-error half of SPEC §8 is honoured here.
    ops.push(QueryOp::JqCalc(parse_expr(s)?));
    Ok(())
}

/// If `s` is exactly `name( … )`, return the inside; else None.
fn fn_call<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(name)?.trim_start();
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
    Some(inner)
}

/// A stage is a pure path if every char OUTSIDE a `[…]` subscript is a path char.
/// Inside a subscript anything is allowed, because that is where a quoted object
/// key (`.["a b"]` — spaces and punctuation are legal in a JSON key), a slice
/// (`.[1:3]`) and a negative index (`.[-1]`) all live. A flat whitelist over the
/// whole string rejected keys holding any char it did not list and then re-routed
/// the stage to the arithmetic parser, which failed on the `[`.
fn is_pure_path(s: &str) -> bool {
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            _ if depth > 0 => {}
            _ if c.is_ascii_alphanumeric() || matches!(c, '.' | '"' | '_') => {}
            _ => return false,
        }
        if depth < 0 {
            return false;
        }
    }
    depth == 0
}

/// Translate a pure path (`.a.b`, `.foo[]`, `.[0]`, `.["k"]`) into ops.
fn translate_path(s: &str, ops: &mut Vec<QueryOp>) -> Result<(), String> {
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    let mut key: Vec<Seg> = Vec::new();
    if cs.first() != Some(&'.') {
        return Err(format!("jq: path must start with `.`: `{s}`"));
    }
    let flush = |key: &mut Vec<Seg>, ops: &mut Vec<QueryOp>| {
        if !key.is_empty() {
            ops.push(QueryOp::Field(FieldSel::JqKey(std::mem::take(key))));
        }
    };
    while i < cs.len() {
        match cs[i] {
            '.' => {
                i += 1;
                if cs.get(i) == Some(&'[') {
                    parse_bracket(&cs, &mut i, &mut key, ops, &flush, s)?;
                } else {
                    let id = take_ident(&cs, &mut i);
                    if id.is_empty() {
                        return Err(format!("jq: expected a key after `.` in `{s}`"));
                    }
                    key.push(Seg::Key(id));
                }
            }
            '[' => parse_bracket(&cs, &mut i, &mut key, ops, &flush, s)?,
            other => return Err(format!("jq: unexpected `{other}` in path `{s}`")),
        }
    }
    flush(&mut key, ops);
    Ok(())
}

/// Parse a subscript at `cs[i] == '['`: `[]` (iterate), `["k"]` (string key),
/// `[N]`/`[-N]` (array index, negative from the end), or `[a:b]` (slice, either
/// bound optional/negative).
fn parse_bracket(
    cs: &[char],
    i: &mut usize,
    key: &mut Vec<Seg>,
    ops: &mut Vec<QueryOp>,
    flush: &dyn Fn(&mut Vec<Seg>, &mut Vec<QueryOp>),
    s: &str,
) -> Result<(), String> {
    *i += 1; // consume '['
    let start = *i;
    while *i < cs.len() && cs[*i] != ']' {
        *i += 1;
    }
    if cs.get(*i) != Some(&']') {
        return Err(format!("jq: unterminated `[` in `{s}`"));
    }
    let content: String = cs[start..*i].iter().collect();
    *i += 1; // consume ']'
    let c = content.trim();
    // `[]` — iterate the array/object.
    if c.is_empty() {
        flush(key, ops);
        ops.push(QueryOp::JqEach);
        return Ok(());
    }
    // `["key"]` — a quoted object key (checked before `:` so `["a:b"]` is a key).
    if let Some(k) = c.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        key.push(Seg::Key(k.to_string()));
        return Ok(());
    }
    // `[a:b]` — a slice; it applies to the value the pending path points at.
    if let Some((a, b)) = c.split_once(':') {
        flush(key, ops);
        ops.push(QueryOp::JsonSlice(
            parse_opt_int(a, s)?,
            parse_opt_int(b, s)?,
        ));
        return Ok(());
    }
    // `[N]` / `[-N]` — an array INDEX, kept distinct from a quoted key so that
    // `[1,2] | .["0"]` refuses the way jq does instead of reading element 0.
    if let Ok(n) = c.parse::<i64>() {
        key.push(Seg::Index(n));
        Ok(())
    } else {
        Err(format!("jq: unsupported subscript `[{content}]` in `{s}`"))
    }
}

/// Parse an optional slice bound: empty → `None`, else an `i64` (may be negative).
fn parse_opt_int(s: &str, whole: &str) -> Result<Option<i64>, String> {
    let s = s.trim();
    if s.is_empty() {
        Ok(None)
    } else {
        s.parse::<i64>()
            .map(Some)
            .map_err(|_| format!("jq: bad slice bound `{s}` in `{whole}`"))
    }
}

fn take_ident(cs: &[char], i: &mut usize) -> String {
    let start = *i;
    while *i < cs.len() && (cs[*i].is_ascii_alphanumeric() || cs[*i] == '_') {
        *i += 1;
    }
    cs[start..*i].iter().collect()
}

/// Parse a jq expression body (a `select(...)` predicate or a `map(...)` /
/// arithmetic stage) into a jq VALUE expression.
///
/// This used to rewrite `.field` into an arb bareword and hand the result to
/// `crate::expr`, arb's f64 evaluator. That model cannot express jq: a compare
/// came back as 1/0 instead of `true`/`false`, `0` counted as falsy in `select`,
/// `==` was numeric rather than type-strict, `"x" + "y"` was NaN, and a type
/// error answered `null` instead of raising. See `crate::jqval`.
fn parse_expr(src: &str) -> Result<crate::jqval::Expr, String> {
    crate::jqval::parse(src.trim()).map_err(|e| format!("jq: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{eval, QueryResult};

    fn run(jq: &str, lines: &[&str]) -> Vec<String> {
        let ops = translate(jq).unwrap();
        match eval(
            &ops,
            &lines.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            1.0,
        ) {
            QueryResult::Lines(v) => v,
            other => panic!("expected lines, got {other:?}"),
        }
    }

    #[test]
    fn identity() {
        assert_eq!(run(".", &["a", "b"]), vec!["a", "b"]);
    }

    #[test]
    fn key_path() {
        assert_eq!(run(".foo.bar", &[r#"{"foo":{"bar":7}}"#]), vec!["7"]);
    }

    #[test]
    fn iterate_root() {
        assert_eq!(run(".[]", &["[1,2,3]"]), vec!["1", "2", "3"]);
    }

    #[test]
    fn iterate_object_yields_values() {
        // jq `.[]` over an object iterates its VALUES (not passes it through).
        assert_eq!(run(".[]", &[r#"{"a":1,"b":2}"#]), vec!["1", "2"]);
        // `map(f)` over an object applies f to each value and still returns an
        // ARRAY (`jq -rc 'map(.+1)'` on this input prints `[2,3]`).
        assert_eq!(run("map(.+1)", &[r#"{"a":1,"b":2}"#]), vec!["[2,3]"]);
    }

    #[test]
    fn field_then_iterate() {
        assert_eq!(run(".items[]", &[r#"{"items":[1,2]}"#]), vec!["1", "2"]);
    }

    #[test]
    fn array_index() {
        assert_eq!(run(".[1]", &[r#"["a","b","c"]"#]), vec!["b"]);
        assert_eq!(run(".foo[0]", &[r#"{"foo":["x","y"]}"#]), vec!["x"]);
    }

    #[test]
    fn bracket_string_key() {
        assert_eq!(run(r#".["foo"]"#, &[r#"{"foo":9}"#]), vec!["9"]);
    }

    #[test]
    fn pipe_two_ops() {
        assert_eq!(run(".foo | .bar", &[r#"{"foo":{"bar":5}}"#]), vec!["5"]);
    }

    #[test]
    fn select_numeric() {
        let out = run(
            "select(.amount > 100)",
            &[r#"{"amount":50}"#, r#"{"amount":150}"#],
        );
        assert_eq!(out, vec![r#"{"amount":150}"#]);
    }

    #[test]
    fn iterate_then_select() {
        let out = run(".[] | select(.n >= 2)", &[r#"[{"n":1},{"n":2},{"n":3}]"#]);
        assert_eq!(out, vec![r#"{"n":2}"#, r#"{"n":3}"#]);
    }

    #[test]
    fn map_returns_an_array_like_jq() {
        // jq's `map(f)` is `[.[] | f]` — the array rewrap is part of the builtin.
        // Verified against the reference: `jq -rc 'map(.price)'` on this input
        // prints `[3,4]`, and `jq -rc 'map(. * 2)'` on `[1,2,3]` prints `[2,4,6]`.
        // arb used to drop the rewrap and emit a line per element, which diverged
        // on EVERY map probe in scripts/jq_parity.sh.
        assert_eq!(
            run("map(.price)", &[r#"[{"price":3},{"price":4}]"#]),
            vec!["[3,4]"]
        );
        assert_eq!(run("map(. * 2)", &["[1,2,3]"]), vec!["[2,4,6]"]);
        // The iterate-without-rewrap reading is jq's own `.[] | f`, unchanged.
        assert_eq!(
            run(".[] | .price", &[r#"[{"price":3},{"price":4}]"#]),
            vec!["3", "4"]
        );
    }

    #[test]
    fn map_is_scoped_per_input_line() {
        // `map` runs inside ONE input; two inputs stay two arrays. A rewrap-less
        // implementation merged them into a single flat stream, so this is the
        // assertion that pins the scope, not just the shape.
        assert_eq!(
            run("map(. * 10)", &["[1,2]", "[3]"]),
            vec!["[10,20]", "[30]"]
        );
        // A reducer inside `map` reduces WITHIN each element list.
        assert_eq!(run("map(add)", &["[[1,2],[3,4]]"]), vec!["[3,7]"]);
    }

    #[test]
    fn has_key() {
        // jq `has(k)` is a per-input BOOLEAN test, not a filter (real jq prints
        // `true` then `false` for these two inputs).
        let out = run(r#"has("id")"#, &[r#"{"id":1}"#, r#"{"x":2}"#]);
        assert_eq!(out, vec!["true", "false"]);
    }

    #[test]
    fn values_is_select_non_null() {
        // jq `values` == `select(. != null)`: drop nulls, pass the rest through
        // unchanged — NOT object-value iteration.
        let out = run("values", &[r#"{"a":1,"b":2}"#, "null", "5"]);
        assert_eq!(out, vec![r#"{"a":1,"b":2}"#, "5"]);
    }

    #[test]
    fn flatten_is_recursive() {
        // jq `flatten` flattens ALL nesting levels AND returns one array:
        // `jq -rc 'flatten'` on this input prints exactly `[1,2,3,4]`. In jq
        // context arb answers with that array; the native `flatten` verb keeps the
        // line-per-leaf shape (see `crate::query`).
        assert_eq!(run("flatten", &["[1,[2,[3,[4]]]]"]), vec!["[1,2,3,4]"]);
    }

    #[test]
    fn builtins() {
        // In JQ CONTEXT `keys` is jq's builtin and returns one sorted array —
        // `jq -rc 'keys'` on this input prints `["a","b"]`. Reaching this function
        // at all means the stage arrived through the jq literal front-end; the
        // native `keys` VERB is a separate op and keeps its line-per-key shape
        // (pinned by `crate::query`'s tests and by stdlib/json.arb's own test).
        assert_eq!(run("keys", &[r#"{"a":1,"b":2}"#]), vec![r#"["a","b"]"#]);
        // An array's keys are its indices.
        assert_eq!(run("keys", &["[9,8]"]), vec!["[0,1]"]);
        assert_eq!(run("add", &["[1,2,3]"]), vec!["6"]);
        assert_eq!(run(".[] | length", &[r#"["ab","cde"]"#]), vec!["2", "3"]);
    }

    #[test]
    fn select_orders_strings() {
        // SPEC §8: "a compare may test strings as well as numbers". Only `==`/`!=`
        // were routed to the text comparator; `<`/`<=`/`>`/`>=` fell through to the
        // numeric VM, which reads a non-numeric field as NaN — and every NaN
        // compare is false, so the filter silently dropped EVERY row instead of
        // erroring. jq orders strings by codepoint: `jq -rc 'select(.s < "abd")'`
        // on `{"s":"abc"}` prints the record, and `select(.s > "abd")` prints
        // nothing.
        let ins = [r#"{"s":"abc"}"#];
        assert_eq!(run(r#"select(.s < "abd")"#, &ins), vec![r#"{"s":"abc"}"#]);
        assert!(run(r#"select(.s > "abd")"#, &ins).is_empty());
        assert_eq!(run(r#"select(.s >= "abc")"#, &ins), vec![r#"{"s":"abc"}"#]);
        assert!(run(r#"select(.s <= "abb")"#, &ins).is_empty());
    }

    #[test]
    fn slices_and_negative_indices() {
        // Array slice, either bound optional or negative.
        assert_eq!(run(".[1:3]", &["[10,20,30,40,50]"]), vec!["[20,30]"]);
        assert_eq!(run(".[:2]", &["[10,20,30]"]), vec!["[10,20]"]);
        assert_eq!(run(".[2:]", &["[10,20,30,40]"]), vec!["[30,40]"]);
        assert_eq!(run(".[-2:]", &["[10,20,30,40]"]), vec!["[30,40]"]);
        assert_eq!(run(".foo[1:3]", &[r#"{"foo":[1,2,3,4]}"#]), vec!["[2,3]"]);
        // String slice.
        assert_eq!(run(".[1:3]", &[r#""hello""#]), vec!["el"]);
        // Negative index → element from the end.
        assert_eq!(run(".[-1]", &[r#"["a","b","c"]"#]), vec!["c"]);
        assert_eq!(run(".foo[-1]", &[r#"{"foo":[1,2,3]}"#]), vec!["3"]);
    }

    #[test]
    fn length_is_json_aware() {
        assert_eq!(run("length", &["[1,2,3]"]), vec!["3"]); // array elements
        assert_eq!(run("length", &[r#"{"a":1,"b":2}"#]), vec!["2"]); // object keys
        assert_eq!(run("length", &[r#""hello""#]), vec!["5"]); // string chars
        assert_eq!(run("length", &["-7"]), vec!["7"]); // |number|
        assert_eq!(run("length", &["null"]), vec!["0"]);
    }

    #[test]
    fn unsupported_errors_cleanly() {
        assert!(translate(".a.b as $x").is_err());
        assert!(translate(".[1:x]").is_err()); // bad slice bound
        assert!(translate(".foo // 0").is_err()); // alternative
        assert!(translate(".foo?").is_err()); // optional
        assert!(translate("reduce .[] as $x (0; . + $x)").is_err());
        assert!(translate("paths").is_err());
        assert!(translate("first(.[])").is_err());
        assert!(translate("$ENV.HOME").is_err());
        assert!(translate(".[] | not").is_err());
    }

    #[test]
    fn nested_field_path_inside_select_now_answers_like_jq() {
        // This used to be a documented arb LIMITATION (`jq: nested field path
        // (.a.b) inside select/map is unsupported`) because the f64 expression
        // parser had no path node. `jqval` does, and jq accepts it, so the
        // refusal was a gap in the claimed subset rather than a boundary of it.
        assert_eq!(
            run("select(.a.b > 1)", &[r#"{"a":{"b":2}}"#]),
            vec![r#"{"a":{"b":2}}"#]
        );
        assert!(run("select(.a.b > 5)", &[r#"{"a":{"b":2}}"#]).is_empty());
        assert_eq!(run("map(.a.b)", &[r#"[{"a":{"b":2}}]"#]), vec!["[2]"]);
    }

    #[test]
    fn select_string_compare() {
        // `select(.k == "v")` is a COMPARE, which the spec documents as in-subset.
        // It used to be rejected outright, and — reached through a `source` body,
        // where the reconstruction dropped the quotes — silently compared against
        // a bareword and matched nothing. Real jq keeps only the matching record.
        let inputs = [r#"{"status":"ok"}"#, r#"{"status":"bad"}"#];
        assert_eq!(
            run(r#"select(.status == "ok")"#, &inputs),
            vec![r#"{"status":"ok"}"#]
        );
        assert_eq!(
            run(r#"select(.status != "ok")"#, &inputs),
            vec![r#"{"status":"bad"}"#]
        );
    }

    #[test]
    fn nulls_render_like_jq() {
        // jq makes no distinction between an explicit null, an absent key and an
        // out-of-range index: all three are the literal `null`. Rendering them as
        // "" instead loses the OUTPUT ITSELF for `.[]` — `[1,null,2]` has to stay
        // three values, not two values and a blank.
        assert_eq!(run(".a", &[r#"{"a":null}"#]), vec!["null"]);
        assert_eq!(run(".b", &[r#"{"a":1}"#]), vec!["null"]);
        assert_eq!(run(".a.b", &[r#"{"a":{"b":null}}"#]), vec!["null"]);
        assert_eq!(run(".[5]", &["[1,2]"]), vec!["null"]);
        assert_eq!(run(".[-4]", &[r#"["a"]"#]), vec!["null"]);
        assert_eq!(run(".[]", &["[1,null,2]"]), vec!["1", "null", "2"]);
        assert_eq!(run(".a[0]", &[r#"{"a":[null]}"#]), vec!["null"]);
        // A present, non-null value is untouched by the null path.
        assert_eq!(run(".a", &[r#"{"a":"x"}"#]), vec!["x"]);
        assert_eq!(run(".a", &[r#"{"a":0}"#]), vec!["0"]);
    }

    #[test]
    fn bracket_key_with_spaces() {
        // A JSON key may hold any character; `.["a b"]` is the only way to reach
        // one that is not a bare identifier. The old whole-string path whitelist
        // rejected the space and re-routed the stage to the arithmetic parser.
        assert_eq!(run(r#".["a b"]"#, &[r#"{"a b":1}"#]), vec!["1"]);
        assert_eq!(run(r#".["a-b"]"#, &[r#"{"a-b":"v"}"#]), vec!["v"]);
    }
}
