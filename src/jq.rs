//! Native jq-syntax front-end for `source { … }` bodies. A body command whose
//! first token is a jq literal (starts with `.`, or is a `select(…)`/`map(…)`
//! stage) is handed here verbatim (verb + args, space-joined) and translated to
//! a `Vec<QueryOp>` over arb's existing ops. This is a PRACTICAL subset — path,
//! iterate, index, pipe, select, map, and the builtins arb already implements —
//! not Turing-complete jq. Anything outside the subset is a clean error, never a
//! silent mis-translation.

use crate::expr::Expr;
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
            ops.push(QueryOp::Each);
            return Ok(());
        }
        "keys" => {
            ops.push(QueryOp::Keys);
            return Ok(());
        }
        "values" => {
            ops.push(QueryOp::Vals);
            return Ok(());
        }
        "length" => {
            ops.push(QueryOp::Len);
            return Ok(());
        }
        "add" => {
            ops.push(QueryOp::Add);
            return Ok(());
        }
        "flatten" => {
            ops.push(QueryOp::Flatten);
            return Ok(());
        }
        "to_entries" => {
            ops.push(QueryOp::Entries);
            return Ok(());
        }
        _ => {}
    }
    if let Some(inner) = fn_call(s, "select") {
        ops.push(QueryOp::Where(parse_expr(inner)?));
        return Ok(());
    }
    if let Some(inner) = fn_call(s, "map") {
        // jq identity: `map(f)` == `[.[] | f]`. arb drops the array rewrap, so
        // `map(f)` == iterate-then-f over the elements.
        ops.push(QueryOp::Each);
        translate_stage(inner.trim(), ops)?;
        return Ok(());
    }
    if let Some(inner) = fn_call(s, "has") {
        let key = strip_quotes(inner.trim());
        if key.is_empty() {
            return Err(format!("jq: has() expects a key: `{s}`"));
        }
        ops.push(QueryOp::Has(key));
        return Ok(());
    }
    if s.starts_with('.') {
        if is_pure_path(s) {
            return translate_path(s, ops);
        }
        // A `.field`-bearing arithmetic body (typically inside `map(...)`), e.g.
        // `. * 2` or `.a + .b` -> a per-element Map.
        ops.push(QueryOp::Map(parse_expr(s)?));
        return Ok(());
    }
    Err(format!("jq: unsupported expression `{s}`"))
}

/// If `s` is exactly `name( … )`, return the inside; else None.
fn fn_call<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(name)?.trim_start();
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
    Some(inner)
}

/// Strip one layer of surrounding double quotes.
fn strip_quotes(s: &str) -> String {
    s.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(s)
        .to_string()
}

/// A stage is a pure path if every char is a path char (no operators/spaces).
/// `:` is excluded so array slices `.[1:3]` fall through to a clean error.
fn is_pure_path(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '[' | ']' | '"' | '_'))
}

/// Translate a pure path (`.a.b`, `.foo[]`, `.[0]`, `.["k"]`) into ops.
fn translate_path(s: &str, ops: &mut Vec<QueryOp>) -> Result<(), String> {
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    let mut key: Vec<String> = Vec::new();
    if cs.first() != Some(&'.') {
        return Err(format!("jq: path must start with `.`: `{s}`"));
    }
    let flush = |key: &mut Vec<String>, ops: &mut Vec<QueryOp>| {
        if !key.is_empty() {
            ops.push(QueryOp::Field(FieldSel::Key(std::mem::take(key))));
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
                    key.push(id);
                }
            }
            '[' => parse_bracket(&cs, &mut i, &mut key, ops, &flush, s)?,
            other => return Err(format!("jq: unexpected `{other}` in path `{s}`")),
        }
    }
    flush(&mut key, ops);
    Ok(())
}

/// Parse a `[]` (iterate), `[N]` (array index), or `["k"]` (string key) subscript
/// at `cs[i] == '['`.
fn parse_bracket(
    cs: &[char],
    i: &mut usize,
    key: &mut Vec<String>,
    ops: &mut Vec<QueryOp>,
    flush: &dyn Fn(&mut Vec<String>, &mut Vec<QueryOp>),
    s: &str,
) -> Result<(), String> {
    *i += 1; // consume '['
    match cs.get(*i) {
        Some(']') => {
            *i += 1;
            flush(key, ops);
            ops.push(QueryOp::Each);
            Ok(())
        }
        Some('"') => {
            *i += 1;
            let mut str_key = String::new();
            while *i < cs.len() && cs[*i] != '"' {
                str_key.push(cs[*i]);
                *i += 1;
            }
            if cs.get(*i) != Some(&'"') {
                return Err(format!("jq: unterminated `[\"…\"]` in `{s}`"));
            }
            *i += 1; // closing quote
            expect(cs, i, ']', s)?;
            key.push(str_key);
            Ok(())
        }
        Some(d) if d.is_ascii_digit() => {
            let n = take_digits(cs, i);
            expect(cs, i, ']', s)?;
            // A numeric path segment is an array index in arb's `walk`.
            key.push(n);
            Ok(())
        }
        _ => Err(format!(
            "jq: unsupported subscript in `{s}` (slices/negative indices are not supported)"
        )),
    }
}

fn expect(cs: &[char], i: &mut usize, ch: char, s: &str) -> Result<(), String> {
    if cs.get(*i) == Some(&ch) {
        *i += 1;
        Ok(())
    } else {
        Err(format!("jq: expected `{ch}` in `{s}`"))
    }
}

fn take_ident(cs: &[char], i: &mut usize) -> String {
    let start = *i;
    while *i < cs.len() && (cs[*i].is_ascii_alphanumeric() || cs[*i] == '_') {
        *i += 1;
    }
    cs[start..*i].iter().collect()
}

fn take_digits(cs: &[char], i: &mut usize) -> String {
    let start = *i;
    while *i < cs.len() && cs[*i].is_ascii_digit() {
        *i += 1;
    }
    cs[start..*i].iter().collect()
}

/// Parse a jq expression body (a `select(...)` predicate or a `map(...)` /
/// arithmetic stage) into an arb `Expr`: rewrite each leading `.field` to an arb
/// field bareword, then defer to arb's expression parser. Nested field paths
/// (`.a.b`) inside an expression are unsupported and error.
fn parse_expr(src: &str) -> Result<Expr, String> {
    let rewritten = rewrite_fields(src)?;
    crate::expr::parse(&rewritten).map_err(|e| format!("jq: {e}"))
}

/// Rewrite jq `.field` refs to arb barewords. `.` followed by an identifier and
/// NOT preceded by an alphanumeric is a field ref (drop the dot). `.` before a
/// digit stays (decimal / range). `.a.b` (nested) errors.
fn rewrite_fields(src: &str) -> Result<String, String> {
    let cs: Vec<char> = src.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c == '.' {
            let next = cs.get(i + 1).copied();
            let prev_alnum = out
                .chars()
                .last()
                .map(|p| p.is_alphanumeric() || p == '_')
                .unwrap_or(false);
            if matches!(next, Some(d) if d.is_ascii_alphabetic() || d == '_') {
                if prev_alnum {
                    return Err(format!(
                        "jq: nested field path (.a.b) inside select/map is unsupported: `{src}`"
                    ));
                }
                i += 1; // drop the '.'
                while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '_') {
                    out.push(cs[i]);
                    i += 1;
                }
                if cs.get(i) == Some(&'.')
                    && matches!(cs.get(i + 1), Some(d) if d.is_ascii_alphabetic() || *d == '_')
                {
                    return Err(format!(
                        "jq: nested field path (.a.b) inside select/map is unsupported: `{src}`"
                    ));
                }
                continue;
            }
            // `.` before a digit is a decimal (`.5`); `..` is a range; a bare `.`
            // (identity of the current element) becomes arb's line scalar `x`.
            if matches!(next, Some(d) if d.is_ascii_digit()) || next == Some('.') {
                out.push('.');
            } else {
                out.push('x');
            }
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    Ok(out)
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
    fn map_field() {
        // map(.price) == .[] | .price
        assert_eq!(
            run("map(.price)", &[r#"[{"price":3},{"price":4}]"#]),
            vec!["3", "4"]
        );
    }

    #[test]
    fn map_arith() {
        assert_eq!(run("map(. * 2)", &["[1,2,3]"]), vec!["2", "4", "6"]);
    }

    #[test]
    fn has_key() {
        let out = run(r#"has("id")"#, &[r#"{"id":1}"#, r#"{"x":2}"#]);
        assert_eq!(out, vec![r#"{"id":1}"#]);
    }

    #[test]
    fn builtins() {
        assert_eq!(run("keys", &[r#"{"a":1,"b":2}"#]), vec!["a", "b"]);
        assert_eq!(run("add", &["[1,2,3]"]), vec!["6"]);
        assert_eq!(run(".[] | length", &[r#"["ab","cde"]"#]), vec!["2", "3"]);
    }

    #[test]
    fn unsupported_errors_cleanly() {
        assert!(translate(".a.b as $x").is_err());
        assert!(translate(".[1:3]").is_err()); // slice
        assert!(translate(".foo // 0").is_err()); // alternative
        assert!(translate(".foo?").is_err()); // optional
        assert!(translate("reduce .[] as $x (0; . + $x)").is_err());
        assert!(translate(r#"select(.status == "ok")"#).is_err()); // string compare
        assert!(translate("select(.a.b > 1)").is_err()); // nested in expr
    }
}
