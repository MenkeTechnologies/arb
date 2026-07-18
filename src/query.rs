//! Query pipeline. A `source { … }` body compiles to a sequence of ops applied
//! to the live stream on each evaluation.
//!
//! M2a verbs: `in` (source marker), `match`/`grep RE`, `reject`/`grepv RE`,
//! `field N` (1-based whitespace column), `count`, `rate`. JSON/CSV field
//! extraction, `where(pred)`, and aggregation to tables land with later verbs.

use std::collections::{BTreeMap, HashSet};

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
    /// Line-list transforms. `Sort` supports numeric (`-n`) and reverse (`-r`).
    Sort {
        numeric: bool,
        reverse: bool,
    },
    Uniq,
    Rev,
    First,
    Last,
    Take(usize),
    Drop(usize),
    /// Per-line string transforms.
    Upper,
    Lower,
    Trim,
    /// Regex replace-all per line (`replace /RE/ TO`; TO may use `$1` captures).
    Replace(Regex, String),
    /// Collapse all lines into one, joined by a separator.
    Join(String),
    /// Keep only the Nth line (1-based).
    Nth(usize),
    /// Parse the accumulated stream as one HTML document and emit, per element
    /// matching the CSS selector, its text (or a named attribute if `attr` is
    /// set; elements lacking that attribute are dropped).
    Sel {
        css: String,
        attr: Option<String>,
    },
    /// Treat the stream as CSV: the first line is the header; each data row
    /// becomes a JSON object keyed by the header, so `field NAME` works.
    Csv,
    /// Same as `Csv` but tab-separated (TSV).
    Tsv,
    /// Reduce to a scalar computed by an arithmetic expression over the current
    /// line count (`x`), evaluated on the fusevm VM.
    Calc(Expr),
    /// keep lines containing a literal substring.
    Contains(String),
    /// keep lines starting with a literal prefix.
    Starts(String),
    /// keep lines ending with a literal suffix.
    Ends(String),
    /// drop empty / whitespace-only lines.
    Nonempty,
    /// keep only lines that parse as a number.
    Numeric,
    /// replace each line with its character count.
    Len,
    /// replace each line with its word count.
    Wc,
    /// absolute value of each numeric line.
    Abs,
    /// round each numeric line to the nearest integer.
    Round,
    /// prefix every line with a literal string.
    Prepend(String),
    /// suffix every line with a literal string.
    Append(String),
    /// split each line by DELIM, keep the Nth (1-based) field.
    Cut(String, usize),
    /// median of numeric lines.
    Median,
    /// population standard deviation of numeric lines.
    Stddev,
    /// 95th percentile (nearest-rank) of numeric lines.
    P95,
    /// max minus min of numeric lines.
    Range,
    /// product of numeric lines.
    Product,
    /// count of distinct lines.
    Distinct,
    /// keep every Nth line (1-based).
    Sample(usize),
    /// keep lines from index A to B inclusive (1-based).
    Slice(usize, usize),
    /// bucket numeric lines into N equal-width ranges -> (range, count) pairs.
    Bins(usize),
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
            QueryOp::Csv => cur = to_json_records(&cur, ','),
            QueryOp::Tsv => cur = to_json_records(&cur, '\t'),
            QueryOp::Sort { numeric, reverse } => {
                if *numeric {
                    cur.sort_by(|a, b| {
                        let na = a.trim().parse::<f64>().unwrap_or(f64::NAN);
                        let nb = b.trim().parse::<f64>().unwrap_or(f64::NAN);
                        na.partial_cmp(&nb).unwrap_or(std::cmp::Ordering::Equal)
                    });
                } else {
                    cur.sort();
                }
                if *reverse {
                    cur.reverse();
                }
            }
            QueryOp::Uniq => {
                let mut seen = HashSet::new();
                cur.retain(|l| seen.insert(l.clone()));
            }
            QueryOp::Rev => cur.reverse(),
            QueryOp::First => cur.truncate(1),
            QueryOp::Last => {
                if let Some(l) = cur.pop() {
                    cur = vec![l];
                } else {
                    cur.clear();
                }
            }
            QueryOp::Take(n) => cur.truncate(*n),
            QueryOp::Drop(n) => {
                cur.drain(0..(*n).min(cur.len()));
            }
            QueryOp::Upper => {
                for l in cur.iter_mut() {
                    *l = l.to_uppercase();
                }
            }
            QueryOp::Lower => {
                for l in cur.iter_mut() {
                    *l = l.to_lowercase();
                }
            }
            QueryOp::Trim => {
                for l in cur.iter_mut() {
                    *l = l.trim().to_string();
                }
            }
            QueryOp::Replace(re, to) => {
                for l in cur.iter_mut() {
                    *l = re.replace_all(l, to.as_str()).into_owned();
                }
            }
            QueryOp::Join(sep) => {
                cur = vec![cur.join(sep)];
            }
            QueryOp::Nth(n) => {
                cur = cur
                    .get(n.saturating_sub(1))
                    .cloned()
                    .map(|l| vec![l])
                    .unwrap_or_default();
            }
            QueryOp::Sel { css, attr } => {
                let doc = Html::parse_document(&cur.join("\n"));
                cur = match Selector::parse(css) {
                    Ok(sel) => doc
                        .select(&sel)
                        .filter_map(|el| match attr {
                            Some(a) => el.value().attr(a).map(str::to_string),
                            None => Some(el.text().collect::<String>().trim().to_string()),
                        })
                        .collect(),
                    Err(_) => Vec::new(),
                };
            }
            QueryOp::Contains(s) => cur.retain(|l| l.contains(s.as_str())),
            QueryOp::Starts(p) => cur.retain(|l| l.starts_with(p.as_str())),
            QueryOp::Ends(s) => cur.retain(|l| l.ends_with(s.as_str())),
            QueryOp::Nonempty => cur.retain(|l| !l.trim().is_empty()),
            QueryOp::Numeric => cur.retain(|l| l.trim().parse::<f64>().is_ok()),
            QueryOp::Len => {
                for l in cur.iter_mut() {
                    *l = l.chars().count().to_string();
                }
            }
            QueryOp::Wc => {
                for l in cur.iter_mut() {
                    *l = l.split_whitespace().count().to_string();
                }
            }
            QueryOp::Abs => {
                for l in cur.iter_mut() {
                    if let Ok(v) = l.trim().parse::<f64>() {
                        *l = fmt_num(v.abs());
                    }
                }
            }
            QueryOp::Round => {
                for l in cur.iter_mut() {
                    if let Ok(v) = l.trim().parse::<f64>() {
                        *l = fmt_num(v.round());
                    }
                }
            }
            QueryOp::Prepend(pre) => {
                for l in cur.iter_mut() {
                    *l = format!("{pre}{l}");
                }
            }
            QueryOp::Append(suf) => {
                for l in cur.iter_mut() {
                    l.push_str(suf);
                }
            }
            QueryOp::Cut(delim, n) => {
                for l in cur.iter_mut() {
                    *l = l
                        .split(delim.as_str())
                        .nth(n.saturating_sub(1))
                        .unwrap_or("")
                        .to_string();
                }
            }
            QueryOp::Median => {
                let mut ns = nums(&cur);
                ns.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let m = if ns.is_empty() {
                    0.0
                } else if ns.len() % 2 == 1 {
                    ns[ns.len() / 2]
                } else {
                    (ns[ns.len() / 2 - 1] + ns[ns.len() / 2]) / 2.0
                };
                return QueryResult::Scalar(m);
            }
            QueryOp::Stddev => {
                let ns = nums(&cur);
                let sd = if ns.is_empty() {
                    0.0
                } else {
                    let mean = ns.iter().sum::<f64>() / ns.len() as f64;
                    let var =
                        ns.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / ns.len() as f64;
                    var.sqrt()
                };
                return QueryResult::Scalar(sd);
            }
            QueryOp::P95 => {
                let mut ns = nums(&cur);
                ns.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let v = if ns.is_empty() {
                    0.0
                } else {
                    let rank = ((0.95 * ns.len() as f64).ceil() as usize).clamp(1, ns.len());
                    ns[rank - 1]
                };
                return QueryResult::Scalar(v);
            }
            QueryOp::Range => {
                let ns = nums(&cur);
                let r = if ns.is_empty() {
                    0.0
                } else {
                    ns.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
                        - ns.iter().cloned().fold(f64::INFINITY, f64::min)
                };
                return QueryResult::Scalar(r);
            }
            QueryOp::Product => return QueryResult::Scalar(nums(&cur).iter().product()),
            QueryOp::Distinct => {
                let set: std::collections::HashSet<&String> = cur.iter().collect();
                return QueryResult::Scalar(set.len() as f64);
            }
            QueryOp::Sample(n) => {
                if *n >= 1 {
                    let mut i = 0usize;
                    cur.retain(|_| {
                        i += 1;
                        i % *n == 0
                    });
                }
            }
            QueryOp::Slice(a, b) => {
                let lo = a.saturating_sub(1).min(cur.len());
                let hi = (*b).min(cur.len());
                cur = if lo < hi { cur[lo..hi].to_vec() } else { Vec::new() };
            }
            QueryOp::Bins(n) => {
                let vals = nums(&cur);
                let n = (*n).max(1);
                if vals.is_empty() {
                    return QueryResult::Pairs(Vec::new());
                }
                let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
                let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let width = ((max - min) / n as f64).max(f64::MIN_POSITIVE);
                let mut counts = vec![0u64; n];
                for v in &vals {
                    let idx = (((v - min) / width) as usize).min(n - 1);
                    counts[idx] += 1;
                }
                let pairs = counts
                    .iter()
                    .enumerate()
                    .map(|(i, &c)| {
                        let lo = min + i as f64 * width;
                        (format!("{}-{}", fmt_num(lo), fmt_num(lo + width)), c)
                    })
                    .collect();
                return QueryResult::Pairs(pairs);
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
            .or_else(|| {
                if path.len() == 1 {
                    logfmt_field(line, &path[0])
                } else {
                    None
                }
            })
            .unwrap_or_default(),
    }
}

/// Split a CSV line on commas, trimming each field. (Quoted fields containing
/// commas are not yet handled.)
/// Parse a header + data rows of a delimited stream into JSON object strings
/// keyed by the header, so `field NAME` works over CSV/TSV.
fn to_json_records(lines: &[String], delim: char) -> Vec<String> {
    if lines.is_empty() {
        return Vec::new();
    }
    let header = split_delim(&lines[0], delim);
    lines[1..]
        .iter()
        .map(|row| {
            let vals = split_delim(row, delim);
            let mut obj = serde_json::Map::new();
            for (i, name) in header.iter().enumerate() {
                obj.insert(
                    name.clone(),
                    Value::String(vals.get(i).cloned().unwrap_or_default()),
                );
            }
            Value::Object(obj).to_string()
        })
        .collect()
}

fn split_delim(line: &str, delim: char) -> Vec<String> {
    line.split(delim).map(|s| s.trim().to_string()).collect()
}

/// Extract a `key=value` (logfmt) field from a line; strips surrounding quotes.
fn logfmt_field(line: &str, key: &str) -> Option<String> {
    line.split_whitespace().find_map(|tok| {
        tok.split_once('=')
            .filter(|(k, _)| *k == key)
            .map(|(_, v)| v.trim_matches('"').to_string())
    })
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
    if let Ok(Value::Object(mut m)) = serde_json::from_str::<Value>(line) {
        if let Some(v) = m.remove(name) {
            return value_to_f64(&v);
        }
    }
    logfmt_field(line, name)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(f64::NAN)
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
