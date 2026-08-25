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
    /// A JSON key path reached through the jq front-end (`.a.b`, `.foo[0]`).
    /// Identical to `Key` except for the miss: jq renders an explicit null, an
    /// absent key and an out-of-range index all as the literal `null`, where
    /// arb's native `field` yields "" and falls back to logfmt. Keeping the two
    /// apart lets a jq path answer like jq without changing what `field NAME`
    /// does to a plain-text or logfmt stream.
    /// Segments are TYPED, unlike `Key`'s flat strings: `.["0"]` is an object
    /// key and `.[0]` is an array index, and jq keeps them apart (`[1,2] |
    /// .["0"]` is an error, not the first element).
    JqKey(Vec<crate::jqval::Seg>),
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
    /// `via NAME [* N]` — fan the stream across a supervised pool of `N` copies
    /// of actor `NAME` (default: one per hardware thread). Each line's scalar is
    /// asked as the actor's first-handler message; the reply is the output line,
    /// order preserved. A parallel actor map (see [`crate::actor::run_via`]).
    Via(std::sync::Arc<crate::actor::ActorDef>, usize),
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
    /// The jq front-end's `keys`. Same key set as `Keys`, but emitted as ONE
    /// compact array line, which is what jq returns. The native `keys` verb keeps
    /// its documented line-per-key shape (SPEC §8) — `stdlib/json.arb` runs
    /// `keys; tally` over it — so the two are separate ops and only the jq
    /// literal front-end (`. | keys`, `map(…)`) reaches this one.
    JqKeys,
    /// Native `vals` verb: expand a JSON object's values, one per line.
    Vals,
    /// jq `values` == `select(. != null)`: drop JSON-null lines, pass every other
    /// line through unchanged. (NOT object-value iteration — that is `vals`/`.[]`.)
    NonNull,
    /// Render a PASSTHROUGH line the way `jq -r` prints it, for the one type where
    /// the raw line and jq's rendering disagree: a top-level JSON STRING.
    ///
    /// Identity `.`, `select(…)` and `values` all emit the INPUT LINE verbatim.
    /// For an object, array, number or boolean that is what jq does too — jq keeps
    /// the source literal of any number it never computed (`1.50` stays `1.50`,
    /// `[1,2]` stays `[1,2]`), so passing the line through matches it. A string is
    /// the exception: `jq -r` strips the quotes and unescapes, so a line reading
    /// `"hello"` must print `hello`. arb printed `"hello"` — and disagreed with
    /// ITSELF, since `.[1:3]` on the same input already rendered `el` raw.
    ///
    /// Deliberately string-ONLY. Re-rendering the other types would lose more than
    /// it gained: `serde_json::Map` is a `BTreeMap`, so a rebuilt object comes back
    /// key-SORTED (`{"b":1,"a":2}` -> `{"a":2,"b":1}`) where jq preserves input
    /// order, and every number would be reprinted through `fmt_num`, turning jq's
    /// preserved `1.50` into `1.5`. Both would be new divergences in place of one.
    JqRawString,
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
    /// jq `has(KEY)`: emit `true`/`false` per input line. The argument is a jq
    /// VALUE, not a name — jq accepts a string key on an object and a numeric
    /// index on an array, and refuses every other pairing by name ("Cannot check
    /// whether array has a string key") rather than answering `false`.
    HasKey(Value),
    /// jq to_entries: expand each JSON object line into one `{"key":<k>,"value":<v>}` line per key (BTreeMap key order); non-object lines pass through.
    Entries,
    /// The jq front-end's `to_entries`. Same mapping as `Entries`, but emits the
    /// whole result as ONE compact array line, which is what jq returns. Native
    /// `entries` keeps its documented line-per-key shape (SPEC §8) so specs built
    /// on it are unaffected; the two spellings are distinct, so they can differ.
    JqEntries,
    /// The jq front-end's `flatten`. Same full-depth leaf walk as `Flatten`, but
    /// emits them as ONE compact array line, which is what jq returns. Native
    /// `flatten` keeps its line-per-leaf shape — same context gating as
    /// `JqKeys`/`JqEntries`.
    JqFlatten,
    /// jq `map(f)` == `[.[] | f]`: run the inner ops over the ELEMENTS of each
    /// input line and re-wrap the results as one compact array line — jq's shape.
    /// Scoped per input line, so two input lines stay two output arrays. (Iterate
    /// WITHOUT the re-wrap is jq's own `.[] | f`, which arb also accepts.)
    JqMap(Vec<QueryOp>),
    /// The jq front-end's `.[]`. Same expansion as `Each`, but a line that parses
    /// to a NON-iterable JSON value (a scalar or `null`) is an error rather than a
    /// pass-through — jq raises "Cannot iterate over null (null)" and SPEC §8
    /// forbids answering where jq refuses. Native `each` keeps the pass-through,
    /// because it runs over plain-text streams too.
    JqEach,
    /// The jq front-end's `add` — `reduce .[] as $x (null; . + $x)`, so an EMPTY
    /// array is `null` (native `add` documents `[] -> ""`) and a mixed-type array
    /// raises the same error `+` would. Iterates an object's values.
    JqAdd,
    /// The jq front-end's `length`. Same JSON-aware measure as `JsonLen`, except a
    /// boolean is an error ("boolean (true) has no length") instead of falling back
    /// to the raw line's character count.
    JqLen,
    /// A COMPLETE jq program, run per line by [`crate::jqlang`].
    ///
    /// The ops above are arb's line-stream translations of the jq subset they
    /// cover, and they keep arb's own promises (identity passes the SOURCE line
    /// through, so its spacing and number literals survive). They cannot express
    /// jq's generator semantics — `.a, .b`, `reduce`, `..`, `try`, assignment —
    /// because a `Vec<QueryOp>` is a stage list, not a language. Anything
    /// outside that subset compiles to a real jq program instead and runs on
    /// arb's own jq engine, which is why the constructs SPEC §8 used to list as
    /// refusals now answer exactly as `jq` does.
    ///
    /// The payload is the SOURCE, not a compiled program: `QueryOp` is cloned
    /// into specs, actors and the web target, so it must stay `Send`-compatible
    /// plain data. Compilation is memoized per thread by [`jq_program`], so a
    /// per-line pipeline still parses the program exactly once.
    JqProgram(String),
    /// jq `select(f)` over jq VALUE semantics: keep the line when `f` is truthy,
    /// where only `false` and `null` are falsy. Distinct from native `Where`,
    /// which evaluates an f64 predicate over a numeric line stream.
    JqSelect(crate::jqval::Expr),
    /// A jq value expression stage (`.a + .b`, `. * 2`, `. > 1`). Distinct from
    /// native `Map`, which computes an f64: this yields a jq VALUE, so a compare
    /// renders `true`/`false` and a string `+` concatenates.
    JqCalc(crate::jqval::Expr),
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
    /// Prefix each line with its 1-based index and a tab: `"1\t<line>"`.
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

/// The output of evaluating a pipeline: lines, a scalar, grouped counts — or a
/// runtime REFUSAL.
///
/// `Error` is the per-line error channel the jq front-end needs. SPEC §8 promises
/// that a construct outside the documented subset "is a hard error … never
/// silently reinterpreted", and a type mismatch (`{"a":1} | . + 3`, `null | .[]`,
/// `true | length`) is only discoverable once the DATA arrives. Without this
/// variant every such case had to answer something — `null`, the raw line, the
/// line's character count — which is the exact failure the SPEC rules out, and
/// which several comments in this file used to record as "arb has no per-line
/// error channel".
///
/// Only the jq front-end raises it. Native verbs run over plain-text streams and
/// keep their documented pass-through behaviour.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    Lines(Vec<String>),
    Scalar(f64),
    Pairs(Vec<(String, u64)>),
    /// A refused pipeline. The message is already `jq: …`-anchored.
    Error(String),
}

/// Evaluate `ops` against `lines`. `elapsed_secs` feeds `rate`.
pub fn eval(ops: &[QueryOp], lines: &[String], elapsed_secs: f64) -> QueryResult {
    let mut cur: Vec<String> = lines.to_vec();
    for op in ops {
        match op {
            QueryOp::Match(re) => cur.retain(|l| re.is_match(l)),
            QueryOp::Reject(re) => cur.retain(|l| !re.is_match(l)),
            QueryOp::Field(sel) => {
                if let FieldSel::JqKey(segs) = sel {
                    for l in cur.iter_mut() {
                        // Through the jq value model, so an object this returns
                        // keeps the document's KEY ORDER — `serde_json::Map` is a
                        // `BTreeMap` and gave `.nested` back alphabetised, which
                        // `yq -o=json` on the same YAML does not.
                        match crate::jqlang::parse_json(l).ok() {
                            // A line that is not JSON at all has no jq TYPE to
                            // check, so it keeps the documented miss rendering.
                            None => *l = "null".to_string(),
                            Some(v) => match crate::jqlang::get_seg_path(&v, segs) {
                                Ok(r) => *l = crate::jqlang::render_raw(&r),
                                Err(e) => return QueryResult::Error(format!("jq: {e}")),
                            },
                        }
                    }
                    continue;
                }
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
            QueryOp::JqEach => {
                let mut out = Vec::with_capacity(cur.len());
                for l in &cur {
                    match jq_value(l) {
                        Some(Value::Array(arr)) => out.extend(arr.iter().map(jq_to_string)),
                        // jq `.[]` over an object iterates its VALUES, in the
                        // object's own key order.
                        Some(Value::Object(m)) => {
                            out.extend(jq_obj_entries(l, &m).iter().map(|(_, v)| jq_to_string(v)))
                        }
                        Some(v) => return QueryResult::Error(cannot_iterate(&v)),
                        None => out.push(l.clone()),
                    }
                }
                cur = out;
            }
            QueryOp::JqProgram(src) => {
                let prog = match jq_program(src) {
                    Ok(p) => p,
                    Err(msg) => return QueryResult::Error(format!("jq: {msg}")),
                };
                let interp = crate::jqlang::Interp::default();
                let mut out = Vec::with_capacity(cur.len());
                // ONE cursor over the stream, which is jq's model: the next
                // document is `.`, and `input` takes the one after it — so a
                // document `input` consumed is not replayed by this loop.
                //
                // The first shape of this was per-line: it handed every
                // iteration a freshly parsed copy of the whole REMAINING stream
                // so `inputs` would see it. That is O(n^2) parses, and it showed
                // — `{n: .name, v: .v}` over 2,000 records took 9.7s against
                // jq's 0.01s. Lines are moved in raw and parsed once, on the way
                // out.
                interp.set_input_lines(std::mem::take(&mut cur));
                let mut lineno = 0usize;
                while let Some(input) = interp.next_input() {
                    lineno += 1;
                    interp.set_line(lineno);
                    let r = prog.run_with(&interp, &input, &mut |v| {
                        out.push(crate::jqlang::render_raw(&v));
                        Ok(())
                    });
                    if let Err(e) = r {
                        match e {
                            crate::jqlang::JqErr::Halt(_, msg) => {
                                if let Some(m) = msg {
                                    return QueryResult::Error(format!(
                                        "jq: {}",
                                        crate::jqlang::render_raw(&m)
                                    ));
                                }
                                break;
                            }
                            other => {
                                return QueryResult::Error(format!("jq: {}", other.to_message()))
                            }
                        }
                    }
                }
                cur = out;
            }
            QueryOp::JqSelect(e) => {
                let mut out = Vec::with_capacity(cur.len());
                for l in &cur {
                    match crate::jqval::eval(e, &jq_scalar(l)) {
                        Ok(v) => {
                            if crate::jqval::truthy(&v) {
                                out.push(l.clone());
                            }
                        }
                        Err(msg) => return QueryResult::Error(format!("jq: {msg}")),
                    }
                }
                cur = out;
            }
            QueryOp::JqCalc(e) => {
                for l in cur.iter_mut() {
                    match crate::jqval::eval(e, &jq_scalar(l)) {
                        Ok(v) => *l = crate::jqval::render(&v),
                        Err(msg) => return QueryResult::Error(format!("jq: {msg}")),
                    }
                }
            }
            QueryOp::JqAdd => {
                for l in cur.iter_mut() {
                    // jq `add` is `reduce .[] as $x (null; . + $x)`: it starts from
                    // null (so `[]` is `null`, not ""), folds with the SAME `+` a
                    // jq expression uses (so `[1,"a"]` raises rather than
                    // stringifying), and iterates an object's values.
                    let items: Vec<Value> = match jq_value(l) {
                        Some(Value::Array(a)) => a,
                        Some(Value::Object(m)) => {
                            jq_obj_entries(l, &m).into_iter().map(|(_, v)| v).collect()
                        }
                        Some(v) => return QueryResult::Error(cannot_iterate(&v)),
                        None => continue,
                    };
                    let mut acc = Value::Null;
                    for it in &items {
                        match crate::jqval::binop(crate::jqval::Op::Add, &acc, it) {
                            Ok(v) => acc = v,
                            Err(msg) => return QueryResult::Error(format!("jq: {msg}")),
                        }
                    }
                    *l = crate::jqval::render(&acc);
                }
            }
            QueryOp::JqLen => {
                for l in cur.iter_mut() {
                    *l = match jq_value(l) {
                        Some(Value::Array(a)) => a.len().to_string(),
                        Some(Value::Object(m)) => m.len().to_string(),
                        Some(Value::String(s)) => s.chars().count().to_string(),
                        // jq's `length` on a number is its ABSOLUTE value.
                        Some(Value::Number(n)) => fmt_num(n.as_f64().unwrap_or(0.0).abs()),
                        Some(Value::Null) => "0".to_string(),
                        Some(v @ Value::Bool(_)) => {
                            return QueryResult::Error(format!(
                                "jq: {} ({v}) has no length",
                                crate::jqval::tname(&v)
                            ));
                        }
                        None => l.chars().count().to_string(),
                    };
                }
            }
            QueryOp::Each => {
                let mut out = Vec::with_capacity(cur.len());
                for l in &cur {
                    match serde_json::from_str::<Value>(l) {
                        // `each` IS jq's `.[]` (SPEC §8), so a null ELEMENT keeps
                        // jq's rendering — dropping it to "" loses the element's
                        // existence, which is what `[1,null,2]` must not do.
                        Ok(Value::Array(arr)) => {
                            out.extend(arr.iter().map(jq_to_string));
                        }
                        // jq `.[]` over an object iterates its VALUES.
                        Ok(Value::Object(m)) => {
                            out.extend(m.values().map(jq_to_string));
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
            QueryOp::Via(def, workers) => {
                cur = crate::actor::run_via(def, *workers, &cur);
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
            QueryOp::JqKeys => {
                for l in cur.iter_mut() {
                    // jq `keys` sorts; serde_json's Map is a BTreeMap here, so
                    // iteration is already sorted. An ARRAY's keys are its indices.
                    match serde_json::from_str::<Value>(l) {
                        Ok(Value::Object(m)) => {
                            let arr: Vec<Value> =
                                m.keys().map(|k| Value::String(k.clone())).collect();
                            *l = Value::Array(arr).to_string();
                        }
                        Ok(Value::Array(a)) => {
                            let arr: Vec<Value> = (0..a.len()).map(Value::from).collect();
                            *l = Value::Array(arr).to_string();
                        }
                        // A scalar (or null) has no keys and jq says so by name.
                        Ok(v) => return QueryResult::Error(has_no_keys(&v)),
                        Err(_) => {}
                    }
                }
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
            QueryOp::JqRawString => {
                for l in cur.iter_mut() {
                    // Re-render through the jq value model, which is exactly what
                    // `jq -rc` prints: a top-level string goes out RAW, and every
                    // other value comes back COMPACT. A line that is not JSON at
                    // all has no jq rendering and passes through as written.
                    if let Ok(v) = crate::jqlang::parse_json(l) {
                        *l = crate::jqlang::render_raw(&v);
                    }
                }
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
                // yields exactly one `true`/`false` line. The key must MATCH the
                // container: a string key on an object, a numeric index on an
                // array. Every other pairing is an error in jq, where arb used to
                // answer `false` — including `[1,2] | has(0)`, which is `true`.
                for l in cur.iter_mut() {
                    let present = match (jq_value(l), key) {
                        (Some(Value::Object(m)), Value::String(k)) => m.contains_key(k),
                        (Some(Value::Array(a)), Value::Number(n)) => {
                            let i = n.as_f64().unwrap_or(-1.0);
                            i >= 0.0 && (i as usize) < a.len()
                        }
                        // jq answers `false` for null rather than raising.
                        (Some(Value::Null), _) | (None, _) => false,
                        (Some(v), k) => {
                            return QueryResult::Error(format!(
                                "jq: Cannot check whether {} has a {} key",
                                crate::jqval::tname(&v),
                                crate::jqval::tname(k)
                            ));
                        }
                    };
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
            QueryOp::JqEntries => {
                fn entry(k: Value, v: Value) -> Value {
                    let mut e = serde_json::Map::new();
                    e.insert("key".into(), k);
                    e.insert("value".into(), v);
                    Value::Object(e)
                }
                for l in cur.iter_mut() {
                    let arr: Vec<Value> = match jq_value(l) {
                        Some(Value::Object(m)) => jq_obj_entries(l, &m)
                            .into_iter()
                            .map(|(k, v)| entry(Value::String(k), v))
                            .collect(),
                        // jq's `to_entries` also walks an ARRAY, keying by index.
                        Some(Value::Array(a)) => a
                            .into_iter()
                            .enumerate()
                            .map(|(i, v)| entry(Value::from(i), v))
                            .collect(),
                        Some(v) => return QueryResult::Error(has_no_keys(&v)),
                        None => continue,
                    };
                    *l = Value::Array(arr).to_string();
                }
            }
            QueryOp::JqMap(inner) => {
                // jq `map(f)` is `[.[] | f]` — a per-input SCOPED iterate, so two
                // input lines stay two output arrays. Running the inner ops over
                // the whole `cur` instead (which dropping the re-wrap amounts to)
                // both loses the array and merges the inputs together.
                let mut out = Vec::with_capacity(cur.len());
                for l in &cur {
                    let elems: Vec<String> = match jq_value(l) {
                        Some(Value::Array(a)) => a.iter().map(jq_to_string).collect(),
                        // jq `map` over an object maps its VALUES (and still
                        // returns an array).
                        Some(Value::Object(m)) => m.values().map(jq_to_string).collect(),
                        // Not iterable: jq raises, and so does arb now.
                        Some(v) => return QueryResult::Error(cannot_iterate(&v)),
                        // Not JSON at all: no jq type to check, so it passes.
                        None => {
                            out.push(l.clone());
                            continue;
                        }
                    };
                    let mapped = match eval(inner, &elems, elapsed_secs) {
                        QueryResult::Lines(v) => v,
                        QueryResult::Scalar(s) => vec![fmt_num(s)],
                        QueryResult::Pairs(ps) => {
                            ps.iter().map(|(k, c)| format!("{k}\t{c}")).collect()
                        }
                        // An inner refusal is the whole `map`'s refusal.
                        e @ QueryResult::Error(_) => return e,
                    };
                    out.push(jq_array_json(&mapped));
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
            QueryOp::JqFlatten => {
                // Same full-depth walk as `Flatten`, re-wrapped as jq's one array.
                fn leaves(v: &Value, out: &mut Vec<Value>) {
                    match v {
                        Value::Array(a) => a.iter().for_each(|e| leaves(e, out)),
                        other => out.push(other.clone()),
                    }
                }
                for l in cur.iter_mut() {
                    // jq's `flatten` is defined over `reduce .[] as $x`, so it
                    // iterates an OBJECT's values too and refuses a scalar.
                    let items: Vec<Value> = match jq_value(l) {
                        Some(Value::Array(a)) => a,
                        Some(Value::Object(m)) => {
                            jq_obj_entries(l, &m).into_iter().map(|(_, v)| v).collect()
                        }
                        Some(v) => return QueryResult::Error(cannot_iterate(&v)),
                        None => continue,
                    };
                    let mut out = Vec::new();
                    items.iter().for_each(|e| leaves(e, &mut out));
                    *l = Value::Array(out).to_string();
                }
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
                    match jq_value(l) {
                        Some(Value::Array(arr)) => {
                            let (lo, hi) = slice_bounds(*a, *b, arr.len());
                            *l = Value::Array(arr[lo..hi].to_vec()).to_string();
                        }
                        Some(Value::String(s)) => {
                            let chars: Vec<char> = s.chars().collect();
                            let (lo, hi) = slice_bounds(*a, *b, chars.len());
                            *l = chars[lo..hi].iter().collect();
                        }
                        // jq slices null to null, and refuses every other type —
                        // it reports the slice as the OBJECT it is internally.
                        Some(Value::Null) => *l = "null".to_string(),
                        Some(v) => {
                            let bound =
                                |x: &Option<i64>| x.map_or("null".to_string(), |n| n.to_string());
                            return QueryResult::Error(format!(
                                "jq: Cannot index {} with object ({{\"start\":{},\"end\":{}}})",
                                crate::jqval::tname(&v),
                                bound(a),
                                bound(b)
                            ));
                        }
                        None => {} // not JSON at all: passes through
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
        // `map(f)` is scoped to one input line, so it streams exactly when its
        // inner pipeline does — a reducer inside it (`map(add)`) does not.
        if let QueryOp::JqMap(inner) = op {
            return is_line_streamable(inner);
        }
        // A jq PROGRAM is per-line unless it reads the stream itself: `input`
        // and `inputs` pull the following documents, so they need the whole
        // buffer in hand. A program that does not touch them streams, which is
        // what keeps `arb -e '… | .name'` emitting as lines arrive instead of
        // buffering to EOF.
        if let QueryOp::JqProgram(src) = op {
            return jq_program(src).is_ok_and(|p| !p.reads_input_stream());
        }
        matches!(
            op,
            QueryOp::Match(_)
                | QueryOp::Reject(_)
                | QueryOp::Field(_)
                | QueryOp::Fields(_)
                | QueryOp::Each
                | QueryOp::JqEach
                | QueryOp::JqSelect(_)
                | QueryOp::JqCalc(_)
                | QueryOp::JqAdd
                | QueryOp::JqLen
                | QueryOp::Keys
                | QueryOp::JqKeys
                | QueryOp::Vals
                | QueryOp::NonNull
                | QueryOp::JqRawString
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
                | QueryOp::JqEntries
                | QueryOp::JqFlatten
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
///
/// `JqKey` is NOT handled here: a jq path can refuse (`3 | .a`), and a `String`
/// return has nowhere to put the refusal, so `eval`'s `Field` arm resolves it
/// through `jqval::get_path` and raises `QueryResult::Error` instead.
fn extract_field(line: &str, sel: &FieldSel) -> String {
    match sel {
        FieldSel::Col(n) => nth_col(line, *n).to_string(),
        FieldSel::JqKey(segs) => jq_value(line)
            .and_then(|v| crate::jqval::get_path(&v, segs).ok())
            .map_or_else(|| "null".to_string(), |v| jq_to_string(&v)),
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

/// Parse a line as JSON for a jq-context op. `None` means the line is not JSON
/// at all — arb's stream is TEXT, so such a line has no jq type to check and
/// every caller keeps its documented pass-through instead of raising.
fn jq_value(line: &str) -> Option<Value> {
    serde_json::from_str::<Value>(line).ok()
}

/// The current value for a jq EXPRESSION stage. Unlike [`jq_value`] there is no
/// "not JSON" case: a bare text line is jq's string, so `"ab" | . * 2` gives
/// `abab` the way jq does.
fn jq_scalar(line: &str) -> Value {
    jq_value(line).unwrap_or_else(|| Value::String(line.to_string()))
}

/// jq's message for iterating something that is not an array or object.
fn cannot_iterate(v: &Value) -> String {
    format!("jq: Cannot iterate over {} ({v})", crate::jqval::tname(v))
}

/// jq's message for asking a non-container for its keys.
fn has_no_keys(v: &Value) -> String {
    format!("jq: {} ({v}) has no keys", crate::jqval::tname(v))
}

/// Render a JSON value the way jq's `-r`/`-c` pair does: a string raw, a null as
/// the literal `null`, everything else compact. `json_to_string` renders a null
/// as "" instead, which is right for arb's native text verbs and wrong for jq.
/// An object line's entries in jq's ITERATION ORDER.
///
/// jq iterates an object in INSERTION order and that order is observable:
/// `{"b":1,"a":2} | to_entries` is `[{"key":"b",…},{"key":"a",…}]`, and `.[]`,
/// `add` and `flatten` all walk the same sequence. `serde_json::Map` is a
/// `BTreeMap`, so every one of those came back key-SORTED.
///
/// The order comes from [`crate::jqlang`], whose value model preserves it, while
/// the VALUES stay `serde_json`'s — so the ops that already fold with
/// `jqval::binop` or render through `jq_to_string` are unchanged apart from the
/// sequence they see. A line that will not re-parse falls back to the map's own
/// order rather than dropping entries.
fn jq_obj_entries(line: &str, m: &serde_json::Map<String, Value>) -> Vec<(String, Value)> {
    match crate::jqlang::parse_json(line) {
        Ok(crate::jqlang::JqVal::Obj(ordered)) => ordered
            .iter()
            .filter_map(|(k, _)| m.get(&**k).map(|v| (k.to_string(), v.clone())))
            .collect(),
        _ => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    }
}

fn jq_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Rebuild a jq array from element strings that have ALREADY been rendered.
///
/// `fmt_num` is the one place an arb number becomes text, but parsing its
/// output back into a `serde_json::Value` and letting serde_json print it again
/// runs the value through a SECOND, different float formatter (ryu) and undoes
/// it: `1e-06` came back out as `1e-6`, `1e-05` as `0.00001`, and
/// `123456789012345000000` as `1.23456789012345e+20`. Every one of those
/// disagreed with `jq -rc` on the same input while the scalar path next to it
/// agreed — one invariant, applied at one site and reverted at the other.
///
/// So an element that is already a JSON NUMBER is emitted verbatim. Everything
/// else still goes through `serde_json`, which is what gives an object, a nested
/// array and a string their escaping, and a line that is not JSON at all becomes
/// a JSON string. That last rule is lossy in exactly one place — a string whose
/// text is itself valid JSON (`"123"`) comes back as the number — which is
/// inherent to a raw line stream and is probed, not hidden, by
/// `scripts/jq_parity.sh`.
fn jq_array_json(elems: &[String]) -> String {
    let mut out = String::from("[");
    for (i, e) in elems.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match serde_json::from_str::<Value>(e) {
            Ok(Value::Number(_)) => out.push_str(e),
            Ok(v) => out.push_str(&v.to_string()),
            Err(_) => out.push_str(&Value::String(e.clone()).to_string()),
        }
    }
    out.push(']');
    out
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
        .filter_map(|de| serde_yaml::Value::deserialize(de).ok())
        .filter(|v| !v.is_null())
        .map(|mut v| {
            apply_yaml_merge(&mut v);
            crate::jqlang::render(&yaml_to_jq(&v))
        })
        .collect()
}

/// `serde_yaml::Value` -> the jq value model.
///
/// Deserializing straight into `serde_json::Value` was one line shorter and
/// dropped the document's KEY ORDER on the way — `serde_json::Map` is a
/// `BTreeMap`. That order is the thing `yq` preserves, so a `.` over
/// `name: …\ncount: …` came back alphabetised while `yq -o=json` kept the file's
/// order. `serde_yaml::Mapping` is insertion-ordered, and so is `JqVal::Obj`.
///
/// A non-string mapping key (YAML allows `1: x`) is rendered as its scalar text,
/// which is what a JSON object requires and what `yq -o=json` emits.
fn yaml_to_jq(v: &serde_yaml::Value) -> crate::jqlang::JqVal {
    use crate::jqlang::JqVal;
    match v {
        serde_yaml::Value::Null => JqVal::Null,
        serde_yaml::Value::Bool(b) => JqVal::Bool(*b),
        // Through the JSON reader so a literal that a double would not print back
        // the same way (a large integer, a trailing zero) keeps its source text.
        serde_yaml::Value::Number(n) => {
            crate::jqlang::parse_json(&n.to_string()).unwrap_or(JqVal::Null)
        }
        serde_yaml::Value::String(s) => JqVal::str(s.as_str()),
        serde_yaml::Value::Sequence(a) => JqVal::arr(a.iter().map(yaml_to_jq).collect()),
        serde_yaml::Value::Mapping(m) => JqVal::obj(
            m.iter()
                .map(|(k, val)| {
                    let key = match k {
                        serde_yaml::Value::String(s) => s.clone(),
                        other => crate::jqlang::render_raw(&yaml_to_jq(other)),
                    };
                    (std::rc::Rc::from(key.as_str()), yaml_to_jq(val))
                })
                .collect(),
        ),
        // A `!Tag value` carries its payload; the tag itself has no JSON form.
        serde_yaml::Value::Tagged(t) => yaml_to_jq(&t.value),
    }
}

/// Apply YAML merge keys (`<<`). serde_yaml expands anchors/aliases but leaves the
/// `<<` merge key literal, so `<<: *base` would otherwise surface a stray `<<`
/// key and omit the merged fields. Merge each `<<` source into its parent object
/// with correct precedence: an explicit key wins over any merged key, and among
/// several merged sources (`<<: [*a, *b]`) an earlier source wins.
fn apply_yaml_merge(v: &mut serde_yaml::Value) {
    if let serde_yaml::Value::Mapping(map) = v {
        let key = serde_yaml::Value::String("<<".to_string());
        if let Some(merge) = map.remove(&key) {
            let sources = match merge {
                serde_yaml::Value::Sequence(a) => a,
                other => vec![other],
            };
            for src in sources {
                if let serde_yaml::Value::Mapping(m) = src {
                    for (k, val) in m {
                        // A key already present wins, so explicit keys and
                        // earlier merge sources both take precedence.
                        if !map.contains_key(&k) {
                            map.insert(k, val);
                        }
                    }
                }
            }
        }
    }
    match v {
        serde_yaml::Value::Mapping(map) => {
            map.iter_mut().for_each(|(_, val)| apply_yaml_merge(val));
        }
        serde_yaml::Value::Sequence(a) => a.iter_mut().for_each(apply_yaml_merge),
        _ => {}
    }
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

/// Format a computed number the way `jq` prints a computed double.
///
/// Every arb value is an f64 (SPEC §6), so this is the one place a number
/// becomes text — `map`, `sum`, `avg`, `floor`, `diff`, the scalar result and
/// every control readout all arrive here. The reference is `jq -r`: arb claims
/// jq parity for its numeric core, and `scripts/expr_paths.sh` checks it.
///
/// jq renders a computed double with netlib `g_fmt`: take the SHORTEST
/// round-trip decimal digits `d1…dn` and the decimal-point position `decpt`
/// (the value is `0.d1…dn × 10^decpt`), then pick exponential notation when
/// `decpt <= -4 || decpt > n + 15` and plain positional notation otherwise.
/// The exponent carries a sign and at least two digits (`1e+17`, `1e-05`).
///
/// Both cutoffs are measured, not guessed: rendering 33,618 doubles this way —
/// every decade from 1e-330 to 1e308 at seven mantissas, 20,000 random bit
/// patterns and 5,000 random decimals — and diffing against `jq 1.8.2 -rc '.*1'`
/// gives zero mismatches. `.*1` forces jq to COMPUTE, so the comparison is
/// against its double formatter and not against its decNumber literal
/// preservation, which arb has no equivalent for (see the module docs).
///
/// The two behaviours this replaces were both wrong against that reference:
///   * Rust's `{}` never uses exponential notation at all, so `1e308` printed
///     as 309 digits and `5e-324` as 324, instead of `1e+308` / `5e-324`.
///   * The `v as i64` fast path printed the EXACT binary value of a whole
///     number, inventing precision the double does not carry: `1e18 / 3`
///     printed `333333333333333312` where jq prints `333333333333333300`, the
///     16 round-trip digits zero-padded. (That cast also saturated outside
///     i64's range — `tests/query.rs` still pins that it never reappears.)
pub(crate) fn fmt_num(v: f64) -> String {
    // A non-finite double has no decimal expansion, and neither JSON nor jq has
    // a spelling for one, so jq maps both onto values that do: an infinity
    // CLAMPS to ±DBL_MAX and a NaN renders as `null`. This function's contract is
    // jq's rendering, so it follows jq here too. Measured against jq 1.8.2:
    // `1e308 * 2` -> `1.7976931348623157e+308`, `-1e308 * 2` -> the negation of
    // that, and `(1e308 * 2) * 0` -> `null`.
    //
    // Rust's own spelling (`inf` / `NaN`) was returned here before. That was a
    // deviation from the contract, justified in this comment by the claim that
    // clamping "would hide the divergence the harness reports for a zero
    // divisor". It does not, and that is checked rather than argued: the split
    // `scripts/expr_paths.sh` reports for `x / 0` is `Value::Undef` on the
    // interpreter against an IEEE infinity in compiled code, and `Value::Undef`
    // renders `0` — not through this branch at all. Clamping moves only the
    // native side, from `inf` to `1.7976931348623157e+308`, so the two tiers
    // still disagree and the probe stays red. Both `x / 0` probes are still
    // listed as divergences by that harness after this change.
    if v.is_nan() {
        return "null".to_string();
    }
    // Clamp the VALUE and fall through to the normal path rather than returning a
    // literal string, so the digits an infinity prints as are by construction the
    // ones this formatter gives DBL_MAX — they can never drift apart.
    let v = v.clamp(-f64::MAX, f64::MAX);
    // `-0.0 == 0.0`, so this is also what keeps a negated zero printing as `0`.
    if v == 0.0 {
        return "0".to_string();
    }
    // `{:e}` is Rust's shortest round-trip form: `d[.ddd]e<exp>`, value =
    // d.ddd × 10^exp. It gives the DIGIT COUNT to work from, but not always the
    // right digits — shortest and shortest-AND-CLOSEST are different rules.
    //
    // Where a double's neighbours are further apart than one unit in the last
    // decimal place, SEVERAL decimals of that shortest length parse back to the
    // same double, and the two rules may pick different ones. jq's dtoa (David
    // Gay's) emits the closest; Rust emits one that round-trips, which need not
    // be. `191510495617760.12` came out of Rust as `…13` and `611630169981189.25`
    // as `…89.3` against jq's `…89.2`. Both of Rust's answers round-trip; neither
    // is the nearest decimal of that length.
    //
    // So: take the LENGTH from Rust's shortest form, then re-render at that many
    // significant digits with `{:.*e}`, which rounds correctly. That is Gay's
    // rule, and it cannot cost round-tripping — if some n-digit decimal parses
    // back to `v` then the CLOSEST n-digit decimal does too, being at least as
    // near. Measured over 200,000 doubles (150,000 random bit patterns plus
    // 50,000 in the 1e14..1e17 band where the ties concentrate) against
    // `jq 1.8.2 -rc '.*1'`: the shortest-only form missed 1,286 of them, this one
    // misses none. The band matters — a pool spread evenly over the exponent
    // range hits it so rarely that 33,618 doubles scored clean while 1 in 160 of
    // the numbers a person actually writes at that magnitude was wrong.
    let shortest = format!("{v:e}");
    let ndigits = shortest
        .split_once('e')
        .map_or(1, |(m, _)| m.chars().filter(char::is_ascii_digit).count());
    let sci = format!("{:.*e}", ndigits.saturating_sub(1), v);
    let (mant, exp) = match sci.split_once('e') {
        Some(p) => p,
        None => return sci,
    };
    let exp: i32 = match exp.parse() {
        Ok(e) => e,
        Err(_) => return sci,
    };
    let sign = if mant.starts_with('-') { "-" } else { "" };
    let mut digits: String = mant.chars().filter(char::is_ascii_digit).collect();
    // Correct rounding can carry into a trailing zero (`…95` at 4 digits becomes
    // `…0`), and a trailing zero would both lengthen the output and move the
    // fixed-vs-exponential cutoff below, which counts SIGNIFICANT digits.
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    let n = digits.len() as i32;
    let decpt = exp + 1;

    if decpt <= -4 || decpt > n + 15 {
        let m = if n == 1 {
            digits
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        // `{:+03}` is sign + at least two digits: `+17`, `-05`, `-300`.
        format!("{sign}{m}e{:+03}", decpt - 1)
    } else if decpt <= 0 {
        format!("{sign}0.{}{digits}", "0".repeat(-decpt as usize))
    } else if decpt >= n {
        format!("{sign}{digits}{}", "0".repeat((decpt - n) as usize))
    } else {
        let (int, frac) = digits.split_at(decpt as usize);
        format!("{sign}{int}.{frac}")
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

/// Public wrapper for `field_str` — used by the TUI facet control to derive
/// candidate values from a stream field.
pub fn field_str_pub(line: &str, name: &str) -> String {
    field_str(line, name)
}

/// Public wrapper for `field_num` — used by the DAP `evaluate` request to
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

/// One side of a string comparison, resolved against the current line: a literal
/// as itself, a field as its value, and `x`/`.` as the whole line.
fn cmp_str(e: &crate::expr::Expr, line: &str) -> String {
    use crate::expr::Expr;
    match e {
        Expr::Str(s) => s.clone(),
        Expr::Field(f) => field_str(line, f),
        Expr::Var => line.trim().to_string(),
        Expr::Num(n) => fmt_num(*n),
        _ => String::new(),
    }
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
        // A compare where one side is a string literal — jq's `select(.status ==
        // "ok")`. Compared as TEXT: the numeric fallback below reads a non-numeric
        // field as NaN, and every NaN comparison is false, so without this arm such
        // a predicate silently matched NOTHING (a filter that drops every row
        // rather than erroring). The ORDERED forms belong here for the same reason:
        // SPEC §8 says "a compare may test strings as well as numbers", and jq
        // orders strings by codepoint — which is Rust's `str` ordering, since UTF-8
        // byte order and codepoint order agree.
        Expr::Bin(
            op @ (BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge),
            a,
            b,
        ) if matches!(**a, Expr::Str(_)) || matches!(**b, Expr::Str(_)) => {
            let sa = cmp_str(a, line);
            let sb = cmp_str(b, line);
            match op {
                BinOp::Eq => sa == sb,
                BinOp::Ne => sa != sb,
                BinOp::Lt => sa < sb,
                BinOp::Le => sa <= sb,
                BinOp::Gt => sa > sb,
                _ => sa >= sb,
            }
        }
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

thread_local! {
    /// Compiled jq programs, keyed by source. A `QueryOp::JqProgram` is
    /// re-evaluated on every stream tick (the TUI re-runs the pipeline as lines
    /// arrive), so without this the same program would be lexed and parsed once
    /// per frame. Thread-local because a compiled program holds `Rc`s.
    static JQ_PROGRAMS: std::cell::RefCell<
        std::collections::HashMap<String, std::rc::Rc<crate::jqlang::Program>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Compile (or fetch from this thread's cache) the jq program `src`.
pub fn jq_program(src: &str) -> Result<std::rc::Rc<crate::jqlang::Program>, String> {
    JQ_PROGRAMS.with(|c| {
        if let Some(p) = c.borrow().get(src) {
            return Ok(p.clone());
        }
        let p = std::rc::Rc::new(crate::jqlang::Program::compile(src)?);
        c.borrow_mut().insert(src.to_string(), p.clone());
        Ok(p)
    })
}
