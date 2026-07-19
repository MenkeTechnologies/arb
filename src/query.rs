//! Query pipeline. A `source { … }` body compiles to a sequence of ops applied
//! to the live stream on each evaluation.
//!
//! M2a verbs: `in` (source marker), `match`/`grep RE`, `reject`/`grepv RE`,
//! `field N` (1-based whitespace column), `count`, `rate`. JSON/CSV field
//! extraction, `where(pred)`, and aggregation to tables land with later verbs.

use std::collections::{BTreeMap, HashSet};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};

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
    /// Project multiple whitespace columns (1-based), space-joined, keeping the
    /// given order — `fields 1 3` on "a b c d" -> "a c". For columnar input.
    Fields(Vec<usize>),
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
    /// Native `vals` verb: expand a JSON object's values, one per line.
    Vals,
    /// jq `values` == `select(. != null)`: drop JSON-null lines, pass every other
    /// line through unchanged. (NOT object-value iteration — that is `vals`/`.[]`.)
    NonNull,
    /// Project a JSON object down to the named keys (jq `{a,b,c}` /
    /// `pick(.a,.b,.c)`), preserving the listed order. Non-object lines pass
    /// through unchanged; missing keys are dropped.
    Pick(Vec<String>),
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
    /// Recursive descent (xpath `//TAG`): parse the accumulated stream as one
    /// HTML document and emit the outer HTML of every element matching the
    /// selector, one per line — so `attr`/`text` can then pull from each.
    Find(String),
    /// From element fragments (one per line, as emitted by `find`), emit the
    /// named attribute of each (xpath `@NAME`, css attr); drop lines whose first
    /// element lacks it.
    Attr(String),
    /// From element fragments, emit each element's inner text; non-element lines
    /// pass through unchanged.
    Text,
    /// Stable-sort JSON-record lines by FIELD; numeric if every field value
    /// parses as a number, else lexicographic on the field's string value.
    /// Non-object lines sink after the sorted records in input order.
    SortBy(String),
    /// Keep the first record for each distinct value of FIELD, preserving input
    /// order. JSON-object lines dedup by that field's value; other lines dedup by
    /// the whole line.
    UniqueBy(String),
    /// Group JSON records by the value of FIELD and count each group, returning
    /// value -> count pairs sorted by count desc then value asc; non-object lines
    /// are counted under their whole-line text. Reducer (early return).
    CountBy(String),
    /// Group lines by the value of FIELD (jq `group_by`): emit one JSON-array
    /// line per distinct value, each array holding that group's members in input
    /// order; groups are ordered by key ascending. Object lines group by the
    /// field's value, other lines by their whole-line text.
    GroupBy(String),
    /// Reducer: emit the single JSON record whose numeric FIELD is smallest (records with a missing/non-numeric FIELD are ignored; empty input yields no lines).
    MinBy(String),
    /// Reducer: emit the single record whose numeric FIELD is the largest.
    MaxBy(String),
    /// Native `has KEY` verb: retain only JSON-object lines that contain KEY; all
    /// other lines (missing key, non-object, unparseable) are dropped.
    Has(String),
    /// jq `has(KEY)`: emit `true`/`false` per input line — `true` iff the line is a
    /// JSON object containing KEY, else `false`. A per-input boolean, not a filter.
    HasKey(String),
    /// jq to_entries: expand each JSON object line into one `{"key":<k>,"value":<v>}` line per key (BTreeMap key order); non-object lines pass through.
    Entries,
    /// jq `flatten`: recursively flatten a JSON-array line to its non-array leaves,
    /// emitting one leaf per line (matching jq's full-depth flatten, unlike `each`
    /// which descends a single level). Non-array lines pass through unchanged.
    Flatten,
    /// jq `add`: reduce a JSON array line to a single value — sum numeric
    /// arrays (fmt_num), concatenate non-numeric arrays via their string
    /// values, empty array -> "". Non-array lines pass through unchanged.
    Add,
    /// Keep only lines that parse as a number strictly greater than `N`.
    /// Lines that do not parse as `f64` are dropped.
    Over(f64),
    /// Keep only numeric lines whose value is strictly less than N; drop non-numeric lines.
    Under(f64),
    /// Keep numeric lines x where lo <= x <= hi (inclusive); non-numeric lines are dropped.
    Between(f64, f64),
    /// Prefix each line with its 1-based index and a tab: "1\t<line>".
    Enumerate,
    /// Split each line on whitespace and emit one word per line (flatten); empty lines produce nothing.
    Words,
    /// Collapse runs of adjacent identical lines to a single line (classic uniq), leaving non-adjacent repeats intact.
    Dedup,
    /// Keep only the last N lines (complement of `take`, which keeps the first N). N>=len keeps all.
    Tailn(usize),
    /// Right-pad each line with spaces to a minimum visible width N (no truncation if the line is already longer).
    Pad(usize),
    /// Left-pad each line with spaces to a minimum width of N (lines already >= N are unchanged).
    Lpad(usize),
    /// Retain lines whose FIELD (json key or 1-based whitespace column) matches the regex.
    Grepf(String, regex::Regex),
    /// Reverse the Unicode scalar characters of each line (chars().rev()).
    Flip,
    /// Path basename: the part after the last `/` (the whole line if none).
    Basename,
    /// Path dirname: the part before the last `/` (`.` if none).
    Dirname,
    /// Group a numeric line's integer part with thousands separators
    /// (`1234567` -> `1,234,567`); non-numeric lines pass through.
    Commafy,
    /// Humanize a byte count (1024-based): `1536` -> `1.5 KB`; non-numeric passes through.
    Bytes,
    /// Humanize a duration in seconds: `3661` -> `1h 1m`; non-numeric passes through.
    Duration,
    /// Placeholder for `apply .name`: at render time it is replaced by the query
    /// pipeline typed into the `input .name` widget (the megafilter/map binding).
    /// Left in a pipeline unsubstituted it is a no-op.
    Apply(String),
    /// Treat the stream as CSV: the first line is the header; each data row
    /// becomes a JSON object keyed by the header, so `field NAME` works.
    Csv,
    B64,
    /// Base64-decode each line (STANDARD alphabet) into a UTF-8 string; lines that fail to
    /// base64-decode or whose bytes aren't valid UTF-8 pass through unchanged.
    B64d,
    /// Lowercase hex-encode each line, two hex digits per UTF-8 byte.
    Hex,
    /// Decode a hex string to UTF-8 text by parsing byte pairs; on any error (odd length, non-hex digit, invalid UTF-8) the line is left unchanged.
    Unhex,
    /// Percent-encode each line, escaping every non-alphanumeric byte (RFC 3986 style).
    Urlenc,
    /// Percent-decode each line to UTF-8 (utf8_percent_decode); lines whose decoded bytes are not valid UTF-8 pass through unchanged.
    Urldec,
    /// Emit the first regex match per line (capture group 1 if the pattern captures, else the whole match); drop lines with no match.
    Extract(Regex),
    /// Explode each line by the literal string DELIM into multiple lines (one part per line).
    /// One-to-many: unlike `cut` (one field) or `words` (whitespace), every split segment becomes its own line.
    Split(String),
    /// Character substring [A,B) 0-based, clamped to the line length (B may exceed len; A>B yields empty).
    Substr(usize, usize),
    /// Explode each line into one output line per Unicode scalar (character); one line -> many.
    Chars,
    /// Title-case each line: uppercase the first letter of each whitespace-separated word, lowercase the rest, rejoin with single spaces.
    Title,
    /// Replace each line with its content repeated N times, concatenated.
    Repeat(usize),
    /// Set key K to the string value V (Value::String(V)) in each JSON object line; non-object / unparseable lines pass through unchanged.
    Set(String, String),
    /// Remove key K from each JSON object line (jq `del(.K)`); non-object lines pass through.
    Del(String),
    /// Rename JSON object key OLD to NEW in each object, preserving the value; no-op if OLD absent. Non-object lines pass through.
    Rename(String, String),
    /// Set string key K to V only when K is absent from the JSON object (jq `//=` for a missing key). Present keys keep their value; non-object / unparseable lines pass through unchanged. Key order is normalized on mutation.
    Default(String, String),
    /// Reduce all JSON object lines into a single object (later keys overwrite earlier); non-object lines are ignored. Emits one JSON object line, or none if no objects were seen.
    Merge,
    /// Floor each numeric line to the nearest lower integer (fmt_num(x.floor())); non-numeric lines pass through unchanged.
    Floor,
    /// Round each numeric line up to the nearest integer (ceil); non-numeric lines pass through unchanged.
    Ceil,
    /// Clamp each numeric line into the inclusive range [LO, HI]; non-numeric lines pass through unchanged.
    Clamp(f64, f64),
    /// Same as `Csv` but tab-separated (TSV).
    Tsv,
    /// Parse the accumulated stream as a YAML document (or `---`-separated
    /// multi-document) and emit each document as a JSON line, so the JSON verbs
    /// (`field`/`pick`/`keys`/`each`) work over it (the yq leg).
    Yaml,
    /// Parse the accumulated stream as one TOML document and emit it as a JSON
    /// object line.
    Toml,
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
    /// jq `length` (JSON-aware): array element count / object key count / string
    /// char count / |number| / null=0; a non-JSON line falls back to char count.
    JsonLen,
    /// jq slice `.[a:b]` over a JSON array line (a/b may be negative — counted
    /// from the end — or omitted); a string line is char-sliced; other lines pass
    /// through unchanged.
    JsonSlice(Option<i64>, Option<i64>),
    /// replace each line with its word count.
    Wc,
    /// absolute value of each numeric line.
    Abs,
    /// round each numeric line to the nearest integer.
    Round,
    /// consecutive differences of the numeric series (n values → n-1 deltas) —
    /// turns a monotonic counter into a per-step rate-of-change.
    Delta,
    /// running (cumulative) total of the numeric series.
    Cumsum,
    /// simple moving average over a trailing window of N (length-preserving; the
    /// first points average a shorter, growing window). Smooths a noisy series.
    Sma(usize),
    /// exponentially-weighted moving average, smoothing factor `alpha` in (0,1] —
    /// higher alpha tracks faster, lower is smoother. `s0 = x0`.
    Ewma(f64),
    /// prefix every line with a literal string.
    Prepend(String),
    /// suffix every line with a literal string.
    Append(String),
    /// split each line by DELIM, keep the Nth (1-based) field.
    Cut(String, usize),
    /// median of numeric lines.
    Median,
    /// Nth percentile (0–100) of the numeric values, linear interpolation between
    /// closest ranks (numpy default). `percentile 99` / `p99` for latency tails.
    Percentile(f64),
    /// population standard deviation of numeric lines.
    Stddev,
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
    /// keep only the Nth line (1-based); out-of-range yields no lines.
    Index(usize),
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
            QueryOp::Fields(cols) => {
                for l in cur.iter_mut() {
                    *l = cols
                        .iter()
                        .map(|&n| nth_col(l, n))
                        .collect::<Vec<_>>()
                        .join(" ");
                }
            }
            QueryOp::Each => {
                let mut out = Vec::with_capacity(cur.len());
                for l in &cur {
                    match serde_json::from_str::<Value>(l) {
                        Ok(Value::Array(arr)) => {
                            out.extend(arr.iter().map(json_to_string));
                        }
                        // jq `.[]` over an object iterates its VALUES.
                        Ok(Value::Object(m)) => {
                            out.extend(m.values().map(json_to_string));
                        }
                        _ => out.push(l.clone()),
                    }
                }
                cur = out;
            }
            QueryOp::Where(e) => {
                // A string predicate (`match(.q)` / `field in .lv`) can't run on
                // the numeric VM — route it to the Rust evaluator; purely numeric
                // predicates stay on fusevm.
                if crate::expr::expr_has_str(e) {
                    cur.retain(|l| eval_where(e, l));
                } else {
                    cur.retain(|l| {
                        let x = l.trim().parse::<f64>().unwrap_or(f64::NAN);
                        let resolve = |name: &str| field_num(l, name);
                        crate::expr::eval_pred_ctx(e, x, &resolve).unwrap_or(false)
                    });
                }
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
            QueryOp::NonNull => {
                // jq `values` == `select(. != null)`: keep every non-null input
                // unchanged, drop only the lines that parse to JSON `null`.
                cur.retain(|l| !matches!(serde_json::from_str::<Value>(l), Ok(Value::Null)));
            }
            QueryOp::Pick(keys) => {
                for l in cur.iter_mut() {
                    if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(l) {
                        // Build in pick order — serde_json::Map is a BTreeMap and
                        // would re-sort the keys, losing the requested order.
                        let parts: Vec<String> = keys
                            .iter()
                            .filter_map(|k| {
                                m.get(k)
                                    .map(|v| format!("{}:{}", Value::String(k.clone()), v))
                            })
                            .collect();
                        *l = format!("{{{}}}", parts.join(","));
                    }
                }
            }
            QueryOp::Csv => cur = to_json_records(&cur, ','),
            QueryOp::Tsv => cur = to_json_records(&cur, '\t'),
            QueryOp::Yaml => cur = yaml_to_json(&cur),
            QueryOp::Toml => cur = toml_to_json(&cur),
            QueryOp::Sort { numeric, reverse } => {
                if *numeric {
                    // Like Unix `sort -n`: order by each line's LEADING numeric
                    // token (the first whitespace-delimited field), so mixed rows
                    // such as `2.1 claude` sort by 2.1 — not the whole line, which
                    // would parse as NaN and leave the order untouched.
                    let key = |s: &str| {
                        s.split_whitespace()
                            .next()
                            .and_then(|t| t.parse::<f64>().ok())
                            .unwrap_or(f64::NAN)
                    };
                    cur.sort_by(|a, b| {
                        key(a)
                            .partial_cmp(&key(b))
                            .unwrap_or(std::cmp::Ordering::Equal)
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
            QueryOp::Find(css) => {
                let doc = Html::parse_document(&cur.join("\n"));
                cur = match Selector::parse(css) {
                    Ok(sel) => doc.select(&sel).map(|el| el.html()).collect(),
                    Err(_) => Vec::new(),
                };
            }
            QueryOp::Attr(name) => {
                let mut out = Vec::with_capacity(cur.len());
                for l in &cur {
                    let frag = Html::parse_fragment(l);
                    if let Some(v) = first_element(&frag).and_then(|e| e.value().attr(name)) {
                        out.push(v.to_string());
                    }
                }
                cur = out;
            }
            QueryOp::Text => {
                for l in cur.iter_mut() {
                    let frag = Html::parse_fragment(l);
                    if let Some(e) = first_element(&frag) {
                        // xpath `text()` is the element's DIRECT child text nodes,
                        // not all descendant text — `e.text()` would also fold in a
                        // child element's text (e.g. `<a>1<b>X</b>2</a>` -> `1X2`).
                        *l = e
                            .children()
                            .filter_map(|c| c.value().as_text().map(|t| t.to_string()))
                            .collect::<String>()
                            .trim()
                            .to_string();
                    }
                }
            }
            QueryOp::SortBy(field) => {
                // Split object lines (carrying their field value) from the rest;
                // non-objects keep their relative input order and sink to the end.
                let mut objs: Vec<(String, String)> = Vec::new();
                let mut rest: Vec<String> = Vec::new();
                for l in cur.drain(..) {
                    if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&l) {
                        let key = m.get(field).map(json_to_string).unwrap_or_default();
                        objs.push((key, l));
                    } else {
                        rest.push(l);
                    }
                }
                let all_numeric =
                    !objs.is_empty() && objs.iter().all(|(k, _)| k.trim().parse::<f64>().is_ok());
                if all_numeric {
                    // slice::sort_by is stable — equal keys preserve input order.
                    objs.sort_by(|a, b| {
                        let na = a.0.trim().parse::<f64>().unwrap_or(f64::NAN);
                        let nb = b.0.trim().parse::<f64>().unwrap_or(f64::NAN);
                        na.partial_cmp(&nb).unwrap_or(std::cmp::Ordering::Equal)
                    });
                } else {
                    objs.sort_by(|a, b| a.0.cmp(&b.0));
                }
                cur = objs.into_iter().map(|(_, l)| l).collect();
                cur.extend(rest);
            }
            QueryOp::UniqueBy(field) => {
                let mut seen: HashSet<String> = HashSet::new();
                cur.retain(|l| {
                    let key = match serde_json::from_str::<Value>(l) {
                        Ok(Value::Object(m)) => {
                            m.get(field).map(json_to_string).unwrap_or_default()
                        }
                        _ => l.clone(),
                    };
                    seen.insert(key)
                });
            }
            QueryOp::CountBy(field) => {
                let mut counts: BTreeMap<String, u64> = BTreeMap::new();
                for l in &cur {
                    let key = match serde_json::from_str::<Value>(l) {
                        Ok(Value::Object(m)) => {
                            m.get(field).map(json_to_string).unwrap_or_default()
                        }
                        _ => l.clone(),
                    };
                    *counts.entry(key).or_insert(0) += 1;
                }
                let mut pairs: Vec<(String, u64)> = counts.into_iter().collect();
                pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                return QueryResult::Pairs(pairs);
            }
            QueryOp::GroupBy(field) => {
                // jq group_by: one array per distinct key, groups sorted by key.
                let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
                for l in &cur {
                    let (key, val) = match serde_json::from_str::<Value>(l) {
                        Ok(v @ Value::Object(_)) => {
                            let k = v.get(field).map(json_to_string).unwrap_or_default();
                            (k, v)
                        }
                        Ok(v) => (l.clone(), v),
                        Err(_) => (l.clone(), Value::String(l.clone())),
                    };
                    groups.entry(key).or_default().push(val);
                }
                cur = groups
                    .into_values()
                    .map(|g| Value::Array(g).to_string())
                    .collect();
            }
            QueryOp::MinBy(field) => {
                let best = cur
                    .iter()
                    .filter(|l| !field_num(l, field).is_nan())
                    .min_by(|a, b| {
                        field_num(a, field)
                            .partial_cmp(&field_num(b, field))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                return QueryResult::Lines(best.into_iter().cloned().collect());
            }
            QueryOp::MaxBy(field) => {
                // Ignore lines whose FIELD is absent/non-numeric (field_num -> NaN),
                // then keep the record with the greatest value. On ties the last
                // maximal record wins (std max_by semantics). Empty input -> no lines.
                let best = cur
                    .iter()
                    .filter(|l| !field_num(l, field).is_nan())
                    .max_by(|a, b| {
                        field_num(a, field)
                            .partial_cmp(&field_num(b, field))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .cloned();
                return QueryResult::Lines(best.into_iter().collect());
            }
            QueryOp::Has(key) => {
                cur.retain(|l| {
                    matches!(
                        serde_json::from_str::<Value>(l),
                        Ok(Value::Object(ref m)) if m.contains_key(key)
                    )
                });
            }
            QueryOp::HasKey(key) => {
                // jq `has`: a per-input boolean test, NOT a filter — every input
                // yields exactly one `true`/`false` line.
                for l in cur.iter_mut() {
                    let present = matches!(
                        serde_json::from_str::<Value>(l),
                        Ok(Value::Object(ref m)) if m.contains_key(key)
                    );
                    *l = if present { "true" } else { "false" }.to_string();
                }
            }
            QueryOp::Entries => {
                let mut out = Vec::with_capacity(cur.len());
                for l in &cur {
                    match serde_json::from_str::<Value>(l) {
                        Ok(Value::Object(m)) => {
                            for (k, v) in &m {
                                out.push(format!(
                                    "{{\"key\":{},\"value\":{}}}",
                                    Value::String(k.clone()),
                                    v
                                ));
                            }
                        }
                        _ => out.push(l.clone()),
                    }
                }
                cur = out;
            }
            QueryOp::Flatten => {
                // jq `flatten` fully flattens all nesting levels; recurse into
                // every sub-array and emit only the non-array leaves.
                fn push_leaves(v: &Value, out: &mut Vec<String>) {
                    match v {
                        Value::Array(a) => a.iter().for_each(|e| push_leaves(e, out)),
                        other => out.push(json_to_string(other)),
                    }
                }
                let mut out = Vec::with_capacity(cur.len());
                for l in &cur {
                    match serde_json::from_str::<Value>(l) {
                        Ok(Value::Array(arr)) => arr.iter().for_each(|e| push_leaves(e, &mut out)),
                        _ => out.push(l.clone()),
                    }
                }
                cur = out;
            }
            QueryOp::Add => {
                for l in cur.iter_mut() {
                    if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(l) {
                        if arr.is_empty() {
                            *l = String::new();
                        } else if arr.iter().all(Value::is_number) {
                            let sum: f64 = arr.iter().filter_map(Value::as_f64).sum();
                            *l = fmt_num(sum);
                        } else {
                            *l = arr.iter().map(json_to_string).collect::<String>();
                        }
                    }
                }
            }
            QueryOp::Over(n) => {
                cur.retain(|l| l.trim().parse::<f64>().map(|v| v > *n).unwrap_or(false));
            }
            QueryOp::Under(n) => {
                cur.retain(|l| l.trim().parse::<f64>().map(|v| v < *n).unwrap_or(false));
            }
            QueryOp::Between(lo, hi) => {
                cur.retain(|l| {
                    let x = l.trim().parse::<f64>().unwrap_or(f64::NAN);
                    x >= *lo && x <= *hi
                });
            }
            QueryOp::Enumerate => {
                for (i, l) in cur.iter_mut().enumerate() {
                    *l = format!("{}\t{}", i + 1, l);
                }
            }
            QueryOp::Words => {
                let mut out = Vec::with_capacity(cur.len());
                for l in &cur {
                    out.extend(l.split_whitespace().map(str::to_string));
                }
                cur = out;
            }
            QueryOp::Dedup => {
                cur.dedup();
            }
            QueryOp::Tailn(n) => {
                let len = cur.len();
                if *n < len {
                    cur.drain(0..len - *n);
                }
            }
            QueryOp::Pad(n) => {
                // `format!` width is a u16 internally: a width >= 65536 panics
                // ("Formatting argument out of range"). Cap it — no real pane is
                // that wide and the widget layer clips lines to the pane anyway.
                let n = (*n).min(u16::MAX as usize);
                for l in cur.iter_mut() {
                    *l = format!("{:<width$}", l, width = n);
                }
            }
            QueryOp::Lpad(width) => {
                let w = (*width).min(u16::MAX as usize);
                for l in cur.iter_mut() {
                    *l = format!("{:>width$}", l, width = w);
                }
            }
            QueryOp::Grepf(field, re) => {
                cur.retain(|l| {
                    let val = if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(l) {
                        m.get(field).map(json_to_string).unwrap_or_default()
                    } else if let Ok(idx) = field.parse::<usize>() {
                        l.split_whitespace()
                            .nth(idx.saturating_sub(1))
                            .unwrap_or("")
                            .to_string()
                    } else {
                        String::new()
                    };
                    re.is_match(&val)
                });
            }
            QueryOp::Flip => {
                for l in cur.iter_mut() {
                    *l = l.chars().rev().collect();
                }
            }
            QueryOp::Basename => {
                for l in cur.iter_mut() {
                    let t = l.trim_end_matches('/');
                    *l = t.rsplit('/').next().unwrap_or(t).to_string();
                }
            }
            QueryOp::Dirname => {
                for l in cur.iter_mut() {
                    let t = l.trim_end_matches('/');
                    *l = match t.rsplit_once('/') {
                        Some(("", _)) => "/".to_string(),
                        Some((dir, _)) => dir.to_string(),
                        None => ".".to_string(),
                    };
                }
            }
            QueryOp::Commafy => {
                for l in cur.iter_mut() {
                    *l = commafy(l);
                }
            }
            QueryOp::Bytes => {
                for l in cur.iter_mut() {
                    if let Ok(v) = l.trim().parse::<f64>() {
                        *l = humanize_bytes(v);
                    }
                }
            }
            QueryOp::Duration => {
                for l in cur.iter_mut() {
                    if let Ok(v) = l.trim().parse::<f64>() {
                        *l = humanize_duration(v);
                    }
                }
            }
            // Resolved to the input widget's pipeline before eval; a no-op if it
            // survives (empty input, or eval reached without substitution).
            QueryOp::Apply(_) => {}
            QueryOp::B64 => {
                for l in cur.iter_mut() {
                    *l = STANDARD.encode(l.as_bytes());
                }
            }
            QueryOp::B64d => {
                for l in cur.iter_mut() {
                    if let Some(s) = STANDARD
                        .decode(l.as_bytes())
                        .ok()
                        .and_then(|b| String::from_utf8(b).ok())
                    {
                        *l = s;
                    }
                }
            }
            QueryOp::Hex => {
                for l in cur.iter_mut() {
                    *l = l.bytes().map(|b| format!("{:02x}", b)).collect();
                }
            }
            QueryOp::Unhex => {
                for l in cur.iter_mut() {
                    let chars: Vec<char> = l.chars().collect();
                    if chars.is_empty() || !chars.len().is_multiple_of(2) {
                        continue;
                    }
                    let mut bytes = Vec::with_capacity(chars.len() / 2);
                    let mut ok = true;
                    let mut i = 0;
                    while i < chars.len() {
                        let pair: String = chars[i..i + 2].iter().collect();
                        match u8::from_str_radix(&pair, 16) {
                            Ok(b) => bytes.push(b),
                            Err(_) => {
                                ok = false;
                                break;
                            }
                        }
                        i += 2;
                    }
                    if ok {
                        if let Ok(decoded) = String::from_utf8(bytes) {
                            *l = decoded;
                        }
                    }
                }
            }
            QueryOp::Urlenc => {
                for l in cur.iter_mut() {
                    *l = utf8_percent_encode(l, NON_ALPHANUMERIC).to_string();
                }
            }
            QueryOp::Urldec => {
                for l in cur.iter_mut() {
                    if let Ok(decoded) = percent_decode_str(l).decode_utf8() {
                        *l = decoded.into_owned();
                    }
                }
            }
            QueryOp::Extract(re) => {
                cur = cur
                    .iter()
                    .filter_map(|l| {
                        re.captures(l).map(|caps| {
                            caps.get(1)
                                .unwrap_or_else(|| caps.get(0).unwrap())
                                .as_str()
                                .to_string()
                        })
                    })
                    .collect();
            }
            QueryOp::Split(delim) => {
                let mut out: Vec<String> = Vec::with_capacity(cur.len());
                for l in cur.iter() {
                    for part in l.split(delim.as_str()) {
                        out.push(part.to_string());
                    }
                }
                cur = out;
            }
            QueryOp::Substr(a, b) => {
                for l in cur.iter_mut() {
                    *l = l.chars().skip(*a).take(b.saturating_sub(*a)).collect();
                }
            }
            QueryOp::Chars => {
                let mut out: Vec<String> = Vec::new();
                for l in cur.iter() {
                    for ch in l.chars() {
                        out.push(ch.to_string());
                    }
                }
                cur = out;
            }
            QueryOp::Title => {
                for l in cur.iter_mut() {
                    *l = l
                        .split_whitespace()
                        .map(|w| {
                            let mut cs = w.chars();
                            match cs.next() {
                                Some(f) => {
                                    f.to_uppercase().collect::<String>()
                                        + &cs.as_str().to_lowercase()
                                }
                                None => String::new(),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                }
            }
            QueryOp::Repeat(n) => {
                for l in cur.iter_mut() {
                    *l = l.repeat(*n);
                }
            }
            QueryOp::Set(key, val) => {
                for l in cur.iter_mut() {
                    if let Ok(Value::Object(mut m)) = serde_json::from_str::<Value>(l) {
                        m.insert(key.clone(), Value::String(val.clone()));
                        *l = Value::Object(m).to_string();
                    }
                }
            }
            QueryOp::Del(key) => {
                for l in cur.iter_mut() {
                    if let Ok(Value::Object(mut m)) = serde_json::from_str::<Value>(l) {
                        m.remove(key);
                        *l = Value::Object(m).to_string();
                    }
                }
            }
            QueryOp::Rename(old, new) => {
                for l in cur.iter_mut() {
                    if let Ok(Value::Object(mut m)) = serde_json::from_str::<Value>(l) {
                        if let Some(v) = m.remove(old) {
                            m.insert(new.clone(), v);
                            *l = Value::Object(m).to_string();
                        }
                    }
                }
            }
            QueryOp::Default(key, val) => {
                for l in cur.iter_mut() {
                    if let Ok(Value::Object(mut m)) = serde_json::from_str::<Value>(l) {
                        m.entry(key.clone()).or_insert(Value::String(val.clone()));
                        *l = Value::Object(m).to_string();
                    }
                }
            }
            QueryOp::Merge => {
                let mut acc = serde_json::Map::new();
                let mut saw_object = false;
                for l in cur.iter() {
                    if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(l) {
                        saw_object = true;
                        for (k, v) in m {
                            acc.insert(k, v);
                        }
                    }
                }
                return if saw_object {
                    QueryResult::Lines(vec![Value::Object(acc).to_string()])
                } else {
                    QueryResult::Lines(vec![])
                };
            }
            QueryOp::Floor => {
                for l in cur.iter_mut() {
                    if let Ok(x) = l.parse::<f64>() {
                        *l = fmt_num(x.floor());
                    }
                }
            }
            QueryOp::Ceil => {
                for l in cur.iter_mut() {
                    if let Ok(x) = l.trim().parse::<f64>() {
                        *l = fmt_num(x.ceil());
                    }
                }
            }
            QueryOp::Clamp(lo, hi) => {
                for l in cur.iter_mut() {
                    if let Ok(x) = l.trim().parse::<f64>() {
                        *l = fmt_num(x.clamp(*lo, *hi));
                    }
                }
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
            QueryOp::JsonLen => {
                for l in cur.iter_mut() {
                    *l = match serde_json::from_str::<Value>(l) {
                        Ok(Value::Array(a)) => a.len().to_string(),
                        Ok(Value::Object(m)) => m.len().to_string(),
                        Ok(Value::String(s)) => s.chars().count().to_string(),
                        Ok(Value::Number(n)) => fmt_num(n.as_f64().unwrap_or(0.0).abs()),
                        Ok(Value::Null) => "0".to_string(),
                        // jq errors on a bool length; arb has no per-line error
                        // channel, so fall back to the raw line's char count.
                        _ => l.chars().count().to_string(),
                    };
                }
            }
            QueryOp::JsonSlice(a, b) => {
                for l in cur.iter_mut() {
                    match serde_json::from_str::<Value>(l) {
                        Ok(Value::Array(arr)) => {
                            let (lo, hi) = slice_bounds(*a, *b, arr.len());
                            *l = Value::Array(arr[lo..hi].to_vec()).to_string();
                        }
                        Ok(Value::String(s)) => {
                            let chars: Vec<char> = s.chars().collect();
                            let (lo, hi) = slice_bounds(*a, *b, chars.len());
                            *l = chars[lo..hi].iter().collect();
                        }
                        _ => {} // non-array/string lines pass through
                    }
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
            QueryOp::Delta => {
                let ns = nums(&cur);
                cur = ns.windows(2).map(|w| fmt_num(w[1] - w[0])).collect();
            }
            QueryOp::Cumsum => {
                let ns = nums(&cur);
                let mut acc = 0.0;
                cur = ns
                    .iter()
                    .map(|&v| {
                        acc += v;
                        fmt_num(acc)
                    })
                    .collect();
            }
            QueryOp::Sma(n) => {
                let ns = nums(&cur);
                let w = (*n).max(1);
                cur = (0..ns.len())
                    .map(|i| {
                        let lo = i + 1 - (i + 1).min(w);
                        let win = &ns[lo..=i];
                        fmt_num(win.iter().sum::<f64>() / win.len() as f64)
                    })
                    .collect();
            }
            QueryOp::Ewma(alpha) => {
                let a = alpha.clamp(0.0, 1.0);
                let ns = nums(&cur);
                let mut s = 0.0;
                cur = ns
                    .iter()
                    .enumerate()
                    .map(|(i, &v)| {
                        s = if i == 0 { v } else { a * v + (1.0 - a) * s };
                        fmt_num(s)
                    })
                    .collect();
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
            QueryOp::Percentile(p) => {
                let mut ns = nums(&cur);
                ns.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let v = if ns.is_empty() {
                    0.0
                } else if ns.len() == 1 {
                    ns[0]
                } else {
                    // Linear interpolation between ranks (numpy default), so `p50`
                    // equals `median` and `p90` of 1..10 is 9.1, matching the docs.
                    let frac = p.clamp(0.0, 100.0) / 100.0;
                    let pos = frac * (ns.len() - 1) as f64;
                    let lo = pos.floor() as usize;
                    let hi = pos.ceil() as usize;
                    ns[lo] + (ns[hi] - ns[lo]) * (pos - lo as f64)
                };
                return QueryResult::Scalar(v);
            }
            QueryOp::Stddev => {
                let ns = nums(&cur);
                let sd = if ns.is_empty() {
                    0.0
                } else {
                    let mean = ns.iter().sum::<f64>() / ns.len() as f64;
                    let var = ns.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / ns.len() as f64;
                    var.sqrt()
                };
                return QueryResult::Scalar(sd);
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
                        i.is_multiple_of(*n)
                    });
                }
            }
            QueryOp::Slice(a, b) => {
                let lo = a.saturating_sub(1).min(cur.len());
                let hi = (*b).min(cur.len());
                cur = if lo < hi {
                    cur[lo..hi].to_vec()
                } else {
                    Vec::new()
                };
            }
            QueryOp::Index(n) => {
                cur = n
                    .checked_sub(1)
                    .and_then(|i| cur.get(i).cloned())
                    .into_iter()
                    .collect();
            }
            QueryOp::Bins(n) => {
                let vals = nums(&cur);
                // Bound the bucket count: it allocates `vec![0u64; n]` and builds
                // n formatted pairs, so an over-large N (e.g. 1e8) balloons memory
                // and hangs. No histogram display needs more than this many bars.
                let n = (*n).clamp(1, 65_536);
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

/// True if every op processes each line independently, so the pipeline can be
/// applied per-line and its results emitted incrementally (streaming). Reducers,
/// sorts/reorderers, and whole-document ops (`sel`/`csv`/`tsv`) are not.
/// Split lines into table `(headers, rows)` for the `table` widget. Each line is
/// split on whitespace into cells; `cols` (a comma-separated list, from `-cols`)
/// names the header row when present. Renderers pad short rows to the column
/// count. Shared by the ratatui TUI and the web dashboard so both agree.
pub fn table_data(lines: &[String], cols: Option<&str>) -> (Vec<String>, Vec<Vec<String>>) {
    let headers: Vec<String> = cols
        .map(|c| {
            c.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let rows: Vec<Vec<String>> = lines
        .iter()
        .map(|l| l.split_whitespace().map(str::to_string).collect())
        .collect();
    (headers, rows)
}

/// Parse lines as a numeric series for the `spark` widget — each line's first
/// whitespace token that parses as a number; non-numeric lines are skipped.
pub fn numeric_series(lines: &[String]) -> Vec<f64> {
    lines
        .iter()
        .filter_map(|l| {
            l.split_whitespace()
                .next()
                .and_then(|t| t.parse::<f64>().ok())
        })
        .collect()
}

/// Render a numeric series as a unicode sparkline (`▁▂▃▄▅▆▇█`), scaled between
/// the series min and max. Shared by the TUI and web so both draw the same shape.
/// A flat series renders as the lowest tick; an empty series is the empty string.
pub fn sparkline(values: &[f64]) -> String {
    const TICKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.is_empty() {
        return String::new();
    }
    let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let range = max - min;
    values
        .iter()
        .map(|&v| {
            let idx = if range <= 0.0 {
                0
            } else {
                (((v - min) / range) * 7.0).round() as usize
            };
            TICKS[idx.min(7)]
        })
        .collect()
}

/// The number of columns a table needs to hold `headers` and `rows` (at least 1).
pub fn table_ncols(headers: &[String], rows: &[Vec<String>]) -> usize {
    rows.iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .max(headers.len())
        .max(1)
}

pub fn is_line_streamable(ops: &[QueryOp]) -> bool {
    ops.iter().all(|op| {
        matches!(
            op,
            QueryOp::Match(_)
                | QueryOp::Reject(_)
                | QueryOp::Field(_)
                | QueryOp::Fields(_)
                | QueryOp::Each
                | QueryOp::Keys
                | QueryOp::Vals
                | QueryOp::NonNull
                | QueryOp::Pick(_)
                | QueryOp::Where(_)
                | QueryOp::Map(_)
                | QueryOp::Contains(_)
                | QueryOp::Starts(_)
                | QueryOp::Ends(_)
                | QueryOp::Nonempty
                | QueryOp::Numeric
                | QueryOp::Len
                | QueryOp::JsonLen
                | QueryOp::JsonSlice(_, _)
                | QueryOp::Wc
                | QueryOp::Abs
                | QueryOp::Round
                | QueryOp::Prepend(_)
                | QueryOp::Append(_)
                | QueryOp::Cut(_, _)
                | QueryOp::Upper
                | QueryOp::Lower
                | QueryOp::Trim
                | QueryOp::Replace(_, _)
                | QueryOp::B64
                | QueryOp::B64d
                | QueryOp::Hex
                | QueryOp::Unhex
                | QueryOp::Urlenc
                | QueryOp::Urldec
                | QueryOp::Extract(_)
                | QueryOp::Substr(_, _)
                | QueryOp::Title
                | QueryOp::Repeat(_)
                | QueryOp::Set(_, _)
                | QueryOp::Del(_)
                | QueryOp::Rename(_, _)
                | QueryOp::Default(_, _)
                | QueryOp::Floor
                | QueryOp::Ceil
                | QueryOp::Clamp(_, _)
                | QueryOp::Has(_)
                | QueryOp::HasKey(_)
                | QueryOp::Entries
                | QueryOp::Add
                | QueryOp::Over(_)
                | QueryOp::Under(_)
                | QueryOp::Between(_, _)
                | QueryOp::Pad(_)
                | QueryOp::Lpad(_)
                | QueryOp::Grepf(_, _)
                | QueryOp::Flip
                | QueryOp::Basename
                | QueryOp::Dirname
                | QueryOp::Commafy
        )
    })
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

/// Parse the stream as a YAML document (or multi-document) and emit each document
/// as a compact JSON line. Uses serde_yaml's document deserializer so every valid
/// YAML `---` document-start marker splits — including `--- # comment` and a
/// trailing-space `--- ` (a naive `split("\n---\n")` misses those and then feeds
/// the whole multi-doc stream to a single-doc parse that errors, dropping it all).
fn yaml_to_json(lines: &[String]) -> Vec<String> {
    use serde::Deserialize;
    let doc = lines.join("\n");
    serde_yaml::Deserializer::from_str(&doc)
        .filter_map(|de| Value::deserialize(de).ok())
        .filter(|v| !v.is_null())
        .map(|v| v.to_string())
        .collect()
}

/// Parse the stream as one TOML document and emit it as a JSON object line
/// (empty if it does not parse). Goes through `toml::Value` and converts
/// explicitly so a TOML datetime becomes a clean scalar string — deserializing
/// straight into `serde_json::Value` instead leaks the toml crate's internal
/// `{"$__toml_private_datetime": …}` tagging map into the output.
fn toml_to_json(lines: &[String]) -> Vec<String> {
    match toml::from_str::<toml::Value>(&lines.join("\n")) {
        Ok(v) => vec![toml_value_to_json(&v).to_string()],
        Err(_) => Vec::new(),
    }
}

/// Convert a `toml::Value` to a `serde_json::Value`, rendering datetimes as their
/// string form (RFC-3339 / date / time) rather than a tagged object.
fn toml_value_to_json(v: &toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::Number((*i).into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(dt) => Value::String(dt.to_string()),
        toml::Value::Array(a) => Value::Array(a.iter().map(toml_value_to_json).collect()),
        toml::Value::Table(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (k.clone(), toml_value_to_json(v)))
                .collect(),
        ),
    }
}

/// Reassemble physical lines into logical CSV/TSV records. A quoted field may
/// contain newlines (RFC 4180), so a line whose double-quotes are unbalanced is
/// still inside a quoted field and continues on the next line until they balance
/// — otherwise a multi-line field is torn into corrupt phantom rows. A doubled
/// `""` (an escaped quote) contributes two quotes, so parity tracks correctly.
fn join_quoted_records(lines: &[String]) -> Vec<String> {
    let mut records = Vec::new();
    let mut buf = String::new();
    for line in lines {
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(line);
        if buf.matches('"').count().is_multiple_of(2) {
            records.push(std::mem::take(&mut buf));
        }
    }
    if !buf.is_empty() {
        records.push(buf); // trailing unbalanced record: emit what we have
    }
    records
}

/// Parse a header + data rows of a delimited stream into JSON object strings
/// keyed by the header, so `field NAME` works over CSV/TSV.
fn to_json_records(lines: &[String], delim: char) -> Vec<String> {
    if lines.is_empty() {
        return Vec::new();
    }
    let records = join_quoted_records(lines);
    let Some(header_row) = records.first() else {
        return Vec::new();
    };
    let header = split_delim(header_row, delim);
    records[1..]
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

/// Split one delimited line into fields per RFC 4180 (line-oriented: no embedded
/// newlines). A field may be double-quoted; a quoted field may contain the
/// delimiter, and a doubled `""` inside it is one literal `"`. Unquoted fields
/// are trimmed (preserving prior behavior); quoted fields are returned verbatim.
fn split_delim(line: &str, delim: char) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut quoted = false; // this field opened with a quote
    let mut chars = line.chars().peekable();

    // Unquoted fields are trimmed; a quoted field is returned exactly.
    let finish = |f: &str, q: bool| -> String {
        if q {
            f.to_string()
        } else {
            f.trim().to_string()
        }
    };

    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next(); // "" -> one literal quote
                    field.push('"');
                } else {
                    in_quotes = false; // closing quote
                }
            } else {
                field.push(c);
            }
        } else if c == '"' && field.trim().is_empty() {
            in_quotes = true; // opening quote at field start
            quoted = true;
            field.clear(); // drop any leading whitespace
        } else if c == delim {
            fields.push(finish(&field, quoted));
            field.clear();
            quoted = false;
        } else {
            field.push(c);
        }
    }
    fields.push(finish(&field, quoted));
    fields
}

/// Extract a `key=value` (logfmt) field. A value may be double-quoted and then
/// contain spaces (`msg="hello world"`), so this scans key=value pairs honoring
/// quotes rather than splitting on whitespace first (which would truncate a
/// quoted value at its first space).
fn logfmt_field(line: &str, key: &str) -> Option<String> {
    let cs: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        while i < cs.len() && cs[i].is_whitespace() {
            i += 1;
        }
        let kstart = i;
        while i < cs.len() && cs[i] != '=' && !cs[i].is_whitespace() {
            i += 1;
        }
        let k: String = cs[kstart..i].iter().collect();
        if i < cs.len() && cs[i] == '=' {
            i += 1; // consume '='
            let val = if cs.get(i) == Some(&'"') {
                i += 1;
                let mut v = String::new();
                while i < cs.len() && cs[i] != '"' {
                    if cs[i] == '\\' && i + 1 < cs.len() {
                        i += 1; // keep the escaped char verbatim
                    }
                    v.push(cs[i]);
                    i += 1;
                }
                i += 1; // closing quote (or past end)
                v
            } else {
                let vstart = i;
                while i < cs.len() && !cs[i].is_whitespace() {
                    i += 1;
                }
                cs[vstart..i].iter().collect()
            };
            if k == key {
                return Some(val);
            }
        }
    }
    None
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
                // jq array index: a negative index counts from the end (`.[-1]`).
                let idx = key.parse::<i64>().ok()?;
                let i = if idx < 0 { a.len() as i64 + idx } else { idx };
                if i >= 0 && (i as usize) < a.len() {
                    a.swap_remove(i as usize)
                } else {
                    return None;
                }
            }
            _ => return None,
        };
    }
    Some(cur)
}

/// Resolve jq slice bounds `[a:b]` to a clamped `lo..hi` over `len`. A negative
/// bound counts from the end; `None` is the start/end. Clamped into `0..=len`,
/// and `hi < lo` collapses to an empty slice.
fn slice_bounds(a: Option<i64>, b: Option<i64>, len: usize) -> (usize, usize) {
    let n = len as i64;
    let norm = |x: i64| (if x < 0 { x + n } else { x }).clamp(0, n);
    let lo = norm(a.unwrap_or(0));
    let hi = norm(b.unwrap_or(n));
    (lo as usize, hi.max(lo) as usize)
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
    // The fast `as i64` path is exact only inside i64's range; outside it an
    // `as` cast SATURATES to i64::MAX/MIN and silently corrupts the value
    // (1e19 -> 9.22e18). Beyond ~9.2e18 fall back to the float formatter, which
    // prints the full integer without scientific notation and without a `.0`.
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.2e18 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Group the integer part of a numeric line with thousands separators; leaves a
/// non-numeric line (and any fractional/sign parts) intact.
fn commafy(line: &str) -> String {
    let s = line.trim();
    if s.parse::<f64>().is_err() {
        return line.to_string();
    }
    let (sign, rest) = match s.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", s),
    };
    let (int, frac) = match rest.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (rest, None),
    };
    let mut grouped = String::new();
    for (idx, ch) in int.chars().enumerate() {
        if idx > 0 && (int.len() - idx) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    match frac {
        Some(f) => format!("{sign}{grouped}.{f}"),
        None => format!("{sign}{grouped}"),
    }
}

/// Humanize a byte count (1024-based): `1536` -> `1.5 KB`, `1024` -> `1 KB`,
/// `500` -> `500 B`. One decimal, trailing `.0` trimmed. Negatives keep the sign.
fn humanize_bytes(v: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let sign = if v < 0.0 { "-" } else { "" };
    let mut n = v.abs();
    let mut u = 0;
    while n >= 1024.0 && u < UNITS.len() - 1 {
        n /= 1024.0;
        u += 1;
    }
    // Bytes are whole; scaled values show one decimal (unless it rounds to .0).
    if u == 0 {
        format!("{sign}{} {}", n.round() as i64, UNITS[u])
    } else {
        let r = (n * 10.0).round() / 10.0;
        if (r.fract()).abs() < f64::EPSILON {
            format!("{sign}{} {}", r as i64, UNITS[u])
        } else {
            format!("{sign}{r:.1} {}", UNITS[u])
        }
    }
}

/// Humanize a duration in seconds as the two largest non-zero units: `3661` ->
/// `1h 1m`, `45` -> `45s`, `90061` -> `1d 1h`, `0` -> `0s`. Negatives keep the sign.
fn humanize_duration(v: f64) -> String {
    let sign = if v < 0.0 { "-" } else { "" };
    let total = v.abs().round() as i64;
    if total == 0 {
        return "0s".to_string();
    }
    let units = [("d", 86400), ("h", 3600), ("m", 60), ("s", 1)];
    let mut rem = total;
    let mut parts = Vec::new();
    for (label, secs) in units {
        let q = rem / secs;
        if q > 0 {
            parts.push(format!("{q}{label}"));
            rem %= secs;
        }
    }
    parts.truncate(2); // two largest non-zero units
    format!("{sign}{}", parts.join(" "))
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

/// Public wrapper for [`field_str`] — used by the TUI facet control to derive
/// candidate values from a stream field.
pub fn field_str_pub(line: &str, name: &str) -> String {
    field_str(line, name)
}

/// Public wrapper for [`field_num`] — used by the DAP `evaluate` request to
/// resolve `.field` references against the paused stream line (same resolver the
/// per-line `where`/`map` evaluation uses).
pub fn field_num_pub(line: &str, name: &str) -> f64 {
    field_num(line, name)
}

/// Format a control scalar: integers without a decimal, else the shortest repr.
pub fn fmt_scalar(v: f64) -> String {
    fmt_num(v)
}

/// A field's value as a string: a JSON object key, else a logfmt `key=value`,
/// else "". `x` (or `.` the whole line via Var) resolves to the whole line.
fn field_str(line: &str, name: &str) -> String {
    if name == "x" {
        return line.to_string();
    }
    if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(line) {
        if let Some(v) = m.get(name) {
            return json_to_string(v);
        }
    }
    logfmt_field(line, name).unwrap_or_default()
}

/// The string value of a substituted string node (`Str`), else "".
fn str_of(e: &crate::expr::Expr) -> String {
    match e {
        crate::expr::Expr::Str(s) => s.clone(),
        _ => String::new(),
    }
}

/// Evaluate a string-bearing `where` predicate against one line (Rust, not the
/// numeric VM). `match`/`in .set` test strings; and/or/not compose; any purely
/// numeric subtree falls back to the fusevm predicate path.
fn eval_where(e: &crate::expr::Expr, line: &str) -> bool {
    use crate::expr::{BinOp, Expr};
    match e {
        Expr::Match(inner) => {
            let q = str_of(inner);
            q.is_empty() || line.to_lowercase().contains(&q.to_lowercase())
        }
        Expr::InSet(field, inner) => {
            let set = str_of(inner);
            let items: Vec<&str> = set
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if items.is_empty() {
                return true; // empty selection -> no filter
            }
            let val = field_str(line, field);
            items.iter().any(|it| *it == val)
        }
        Expr::Not(a) => !eval_where(a, line),
        Expr::Bin(BinOp::And, a, b) => eval_where(a, line) && eval_where(b, line),
        Expr::Bin(BinOp::Or, a, b) => eval_where(a, line) || eval_where(b, line),
        // A numeric subtree: evaluate it on fusevm as usual.
        _ => {
            let x = line.trim().parse::<f64>().unwrap_or(f64::NAN);
            let resolve = |n: &str| field_num(line, n);
            crate::expr::eval_pred_ctx(e, x, &resolve).unwrap_or(false)
        }
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
/// The first real element of a parsed fragment, skipping the synthetic
/// `html`/`head`/`body` wrappers that `Html::parse_fragment` inserts.
fn first_element(frag: &Html) -> Option<ElementRef<'_>> {
    let star = Selector::parse("*").ok()?;
    frag.select(&star)
        .find(|e| !matches!(e.value().name(), "html" | "head" | "body"))
}

fn json_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_plain() {
        assert_eq!(split_delim("a,b,c", ','), vec!["a", "b", "c"]);
    }

    #[test]
    fn split_quoted_comma() {
        // A delimiter inside quotes is not a separator.
        assert_eq!(split_delim("\"a,b\",c", ','), vec!["a,b", "c"]);
    }

    #[test]
    fn split_doubled_quote() {
        // "" inside a quoted field is one literal quote.
        assert_eq!(
            split_delim("\"she \"\"said\"\"\",x", ','),
            vec!["she \"said\"", "x"]
        );
    }

    #[test]
    fn split_trailing_empty() {
        assert_eq!(split_delim("a,", ','), vec!["a", ""]);
    }

    #[test]
    fn split_tsv() {
        assert_eq!(split_delim("a\tb\tc", '\t'), vec!["a", "b", "c"]);
    }

    #[test]
    fn split_unquoted_trims_quoted_keeps_spaces() {
        // Unquoted fields are trimmed; a quoted field keeps its inner spaces.
        assert_eq!(split_delim(" a , b ", ','), vec!["a", "b"]);
        assert_eq!(split_delim("\" a \",b", ','), vec![" a ", "b"]);
    }
}
