//! Query pipeline. A `source { … }` body compiles to a sequence of ops applied
//! to the live stream on each evaluation.
//!
//! M2a verbs: `in` (source marker), `match`/`grep RE`, `reject`/`grepv RE`,
//! `field N` (1-based whitespace column), `count`, `rate`. JSON/CSV field
//! extraction, `where(pred)`, and aggregation to tables land with later verbs.

use std::collections::BTreeMap;

use regex::Regex;
use scraper::{Html, Selector};
use serde_json::Value;

use crate::expr::Expr;

/// How `field` selects a value: a 1-based whitespace column, or a JSON key path.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldSel {
    Col(usize),
    Key(Vec<String>),
}

#[derive(Debug, Clone)]
pub enum QueryOp {
    /// Keep lines matching the pattern.
    Match(Regex),
    /// Drop lines matching the pattern.
    Reject(Regex),
    /// Replace each line with a selected field (whitespace column or JSON key path).
    Field(FieldSel),
    /// Flatten JSON-array lines into one line per element (jq `[]`); non-array
    /// lines pass through unchanged.
    Each,
    /// Keep lines whose numeric value (`x` = line parsed as a number) satisfies
    /// the predicate — compiled to fusevm and evaluated per line.
    Where(Expr),
    /// Replace each line with the value of an expression (field-aware; `x` =
    /// line-as-number), computed on the fusevm VM.
    Map(Expr),
    /// Reduce to the current line count.
    Count,
    /// Reduce to lines-per-second over the elapsed window.
    Rate,
    /// Group identical values and count them, sorted by count desc then key asc.
    Tally,
    /// Numeric reductions over lines parsed as numbers (non-numeric ignored).
    Sum,
    Min,
    Max,
    Avg,
    /// Flatten a JSON object's keys / values into one line each.
    Keys,
    Vals,
    /// Parse the accumulated stream as one HTML document and emit the text of
    /// each element matching the CSS selector (one line per match).
    Sel(String),
    /// Reduce to a scalar computed by an arithmetic expression over the current
    /// line count (`x`), evaluated on the fusevm VM.
    Calc(Expr),
}

/// The output of evaluating a pipeline: lines, a scalar, or grouped counts.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    Lines(Vec<String>),
    Scalar(f64),
    Pairs(Vec<(String, u64)>),
}

/// Evaluate `ops` against `lines`. `elapsed_secs` feeds `rate`.
pub fn eval(ops: &[QueryOp], lines: &[String], elapsed_secs: f64) -> QueryResult {
    let mut cur: Vec<String> = lines.to_vec();
    for op in ops {
        match op {
            QueryOp::Match(re) => cur.retain(|l| re.is_match(l)),
            QueryOp::Reject(re) => cur.retain(|l| !re.is_match(l)),
            QueryOp::Field(sel) => {
                for l in cur.iter_mut() {
                    *l = extract_field(l, sel);
                }
            }
            QueryOp::Each => {
                let mut out = Vec::with_capacity(cur.len());
                for l in &cur {
                    match serde_json::from_str::<Value>(l) {
                        Ok(Value::Array(arr)) => {
                            out.extend(arr.iter().map(json_to_string));
                        }
                        _ => out.push(l.clone()),
                    }
                }
                cur = out;
            }
            QueryOp::Where(e) => {
                cur.retain(|l| {
                    let x = l.trim().parse::<f64>().unwrap_or(f64::NAN);
                    let resolve = |name: &str| field_num(l, name);
                    crate::expr::eval_pred_ctx(e, x, &resolve).unwrap_or(false)
                });
            }
            QueryOp::Map(e) => {
                for l in cur.iter_mut() {
                    let v = {
                        let x = l.trim().parse::<f64>().unwrap_or(f64::NAN);
                        let resolve = |name: &str| field_num(l, name);
                        crate::expr::eval_ctx(e, x, &resolve).unwrap_or(f64::NAN)
                    };
                    *l = fmt_num(v);
                }
            }
            QueryOp::Count => return QueryResult::Scalar(cur.len() as f64),
            QueryOp::Rate => {
                return QueryResult::Scalar(cur.len() as f64 / elapsed_secs.max(0.001));
            }
            QueryOp::Tally => {
                let mut counts: BTreeMap<String, u64> = BTreeMap::new();
                for l in &cur {
                    *counts.entry(l.clone()).or_insert(0) += 1;
                }
                let mut pairs: Vec<(String, u64)> = counts.into_iter().collect();
                pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                return QueryResult::Pairs(pairs);
            }
            QueryOp::Calc(e) => {
                let x = cur.len() as f64;
                return QueryResult::Scalar(crate::expr::eval(e, x).unwrap_or(0.0));
            }
            QueryOp::Sum => return QueryResult::Scalar(nums(&cur).iter().sum()),
            QueryOp::Min => {
                let m = nums(&cur).into_iter().fold(f64::INFINITY, f64::min);
                return QueryResult::Scalar(if m.is_finite() { m } else { 0.0 });
            }
            QueryOp::Max => {
                let m = nums(&cur).into_iter().fold(f64::NEG_INFINITY, f64::max);
                return QueryResult::Scalar(if m.is_finite() { m } else { 0.0 });
            }
            QueryOp::Avg => {
                let ns = nums(&cur);
                let a = if ns.is_empty() {
                    0.0
                } else {
                    ns.iter().sum::<f64>() / ns.len() as f64
                };
                return QueryResult::Scalar(a);
            }
            QueryOp::Keys => {
                let mut out = Vec::new();
                for l in &cur {
                    match serde_json::from_str::<Value>(l) {
                        Ok(Value::Object(m)) => out.extend(m.keys().cloned()),
                        _ => out.push(l.clone()),
                    }
                }
                cur = out;
            }
            QueryOp::Vals => {
                let mut out = Vec::new();
                for l in &cur {
                    match serde_json::from_str::<Value>(l) {
                        Ok(Value::Object(m)) => out.extend(m.values().map(json_to_string)),
                        _ => out.push(l.clone()),
                    }
                }
                cur = out;
            }
            QueryOp::Sel(css) => {
                let doc = Html::parse_document(&cur.join("\n"));
                cur = match Selector::parse(css) {
                    Ok(sel) => doc
                        .select(&sel)
                        .map(|el| el.text().collect::<String>().trim().to_string())
                        .collect(),
                    Err(_) => Vec::new(),
                };
            }
        }
    }
    QueryResult::Lines(cur)
}

/// Extract a field from a line per the selector.
fn extract_field(line: &str, sel: &FieldSel) -> String {
    match sel {
        FieldSel::Col(n) => nth_col(line, *n).to_string(),
        FieldSel::Key(path) => serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|v| walk(v, path))
            .map(|v| json_to_string(&v))
            .unwrap_or_default(),
    }
}

/// The 1-based whitespace column `n` of `line` ("" if absent; 0 = whole line).
fn nth_col(line: &str, n: usize) -> &str {
    if n == 0 {
        return line;
    }
    line.split_whitespace().nth(n - 1).unwrap_or("")
}

/// Walk a JSON key/array-index path, consuming the value.
fn walk(mut cur: Value, path: &[String]) -> Option<Value> {
    for key in path {
        cur = match cur {
            Value::Object(mut m) => m.remove(key)?,
            Value::Array(mut a) => {
                let i = key.parse::<usize>().ok()?;
                if i < a.len() {
                    a.swap_remove(i)
                } else {
                    return None;
                }
            }
            _ => return None,
        };
    }
    Some(cur)
}

/// Parse the numeric lines of a slice, ignoring non-numeric ones.
fn nums(lines: &[String]) -> Vec<f64> {
    lines
        .iter()
        .filter_map(|l| l.trim().parse::<f64>().ok())
        .collect()
}

/// Format a computed number: integers without a decimal point, else default.
fn fmt_num(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Resolve a JSON field of `line` to a number for expression evaluation
/// (missing / non-numeric / non-JSON -> NaN, which fails numeric predicates).
fn field_num(line: &str, name: &str) -> f64 {
    match serde_json::from_str::<Value>(line) {
        Ok(Value::Object(mut m)) => m
            .remove(name)
            .as_ref()
            .map(value_to_f64)
            .unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

fn value_to_f64(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => s.trim().parse().unwrap_or(f64::NAN),
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        _ => f64::NAN,
    }
}

/// Render a JSON scalar as a plain string; containers as compact JSON.
fn json_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
