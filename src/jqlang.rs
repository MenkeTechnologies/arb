//! `jqlang` — a complete jq language engine.
//!
//! # Why this exists next to `crate::jq`
//!
//! [`crate::jq`] is a TRANSLATOR: it rewrites a jq literal into arb's linear
//! `Vec<QueryOp>` line-stream pipeline. That shape is exactly right for the
//! constructs arb's own verbs already cover (a path, an iterate, a `select`, a
//! `map`) and it keeps arb's line-stream promises — notably that identity and
//! `select` emit the SOURCE line verbatim, so `{ "a" : 1 }` keeps its spacing
//! and `1.50` keeps its literal.
//!
//! It is structurally incapable of the REST of jq. A `Vec<QueryOp>` is a
//! sequence of one-line-in/one-line-out (or reducing) stages; jq is a language
//! of GENERATORS, where every filter maps one input to a STREAM of outputs, and
//! where `reduce`/`foreach`/`label`/`try` are control flow over that stream.
//! `.a, .b` alone cannot be expressed as a stage list. So `crate::jq` refused
//! everything it could not translate, and SPEC §8 listed those refusals.
//!
//! This module is the other half: a real jq lexer, parser and evaluator with
//! jq's own value model, so the constructs that used to be refused are now
//! ANSWERED, byte-for-byte as `jq` answers them. `crate::jq` still handles what
//! it handled (unchanged, so the line-stream passthrough guarantees are intact)
//! and hands anything else here instead of erroring.
//!
//! # The value model
//!
//! Not `serde_json::Value`, for two measured reasons:
//!
//! * **Key order is observable in jq.** `{"b":1,"a":2} | to_entries` is
//!   `[{"key":"b",…},{"key":"a",…}]` and `keys_unsorted` is `["b","a"]`.
//!   `serde_json::Map` is a `BTreeMap`, which re-sorts both.
//! * **Number literals survive unmodified values.** `jq -c .` on
//!   `{"a":1.50,"b":1E+2}` prints `{"a":1.50,"b":1E+2}`, and `12345678901234567890`
//!   round-trips exactly. An `f64` loses all three. jq only reformats a number it
//!   COMPUTED (`.a+0` is `1.5`), so the literal is carried alongside the double
//!   and dropped the moment arithmetic touches it.
//!
//! Containers are `Rc`-shared so a clone is a refcount bump, which is what makes
//! `reduce`/`foreach`/path updates affordable — the same choice jq's own
//! refcounted `jv` makes.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::fmt::Write as _;
use std::rc::Rc;

// ─────────────────────────────────────────────────────────────────────────────
// Value model
// ─────────────────────────────────────────────────────────────────────────────

/// A jq value. `Num` carries the source literal when the number came from input
/// text and has not been computed on, so `1.50` prints back as `1.50`.
#[derive(Debug, Clone)]
pub enum JqVal {
    Null,
    Bool(bool),
    Num(f64, Option<Rc<str>>),
    Str(Rc<str>),
    Arr(Rc<Vec<JqVal>>),
    /// Insertion-ordered key/value pairs. jq objects are small in practice, so a
    /// vector with a linear scan beats a hash map on both lookup and clone while
    /// preserving the order jq exposes through `keys_unsorted`/`to_entries`.
    Obj(Rc<Vec<(Rc<str>, JqVal)>>),
    /// A YAML NODE: one of the six values above plus the metadata YAML records
    /// about it — comments, anchor, alias, tag, style, position. See
    /// [`crate::ynode`] for why the metadata rides alongside the value instead
    /// of replacing it.
    ///
    /// **Only [`crate::yaml`] constructs this.** `parse_json` cannot, so a JSON
    /// program never sees the variant and every jq answer is reached through
    /// exactly the arms it was reached through before. Every operation that
    /// cares about the VALUE calls [`JqVal::bare`] first; the yq metadata
    /// builtins are the only ones that look at the box.
    Node(Rc<crate::ynode::YNode>),
}

impl JqVal {
    pub fn num(v: f64) -> Self {
        JqVal::Num(v, None)
    }
    pub fn str(s: impl Into<Rc<str>>) -> Self {
        JqVal::Str(s.into())
    }
    pub fn arr(v: Vec<JqVal>) -> Self {
        JqVal::Arr(Rc::new(v))
    }
    pub fn obj(v: Vec<(Rc<str>, JqVal)>) -> Self {
        JqVal::Obj(Rc::new(v))
    }

    /// The value with any YAML node box removed.
    ///
    /// Every operation whose answer is about the VALUE goes through this, so a
    /// commented YAML scalar compares, sorts, renders and arithmetics exactly as
    /// the same scalar read from JSON would. The box is one level deep by
    /// construction ([`JqVal::wrap`] collapses a re-wrap), so this never
    /// recurses more than once.
    pub fn bare(&self) -> &JqVal {
        match self {
            JqVal::Node(n) => &n.val,
            other => other,
        }
    }

    /// The YAML metadata on this node, or `None` for a plain value.
    pub fn meta(&self) -> Option<&crate::ynode::NodeMeta> {
        match self {
            JqVal::Node(n) => Some(&n.meta),
            _ => None,
        }
    }

    /// Box `v` with `meta`, or hand `v` back untouched when the metadata is
    /// [`crate::ynode::NodeMeta::is_bare`] — a document with no comments,
    /// anchors, tags or quoting pays no allocation and produces values that are
    /// bit-identical to the JSON reader's.
    pub fn wrap(v: JqVal, meta: crate::ynode::NodeMeta) -> JqVal {
        if meta.is_bare() {
            return v;
        }
        // Never nest: re-wrapping replaces the metadata rather than layering it,
        // which is what `.x | (. tag = "!!str") | anchor = "a"` needs.
        let val = match v {
            JqVal::Node(n) => n.val.clone(),
            other => other,
        };
        JqVal::Node(Rc::new(crate::ynode::YNode { meta, val }))
    }

    /// Replace this node's metadata, keeping the value. Used by every `… = …`
    /// metadata assignment (`anchor`, `tag`, `style`, the three comments).
    pub fn with_meta(&self, f: impl FnOnce(&mut crate::ynode::NodeMeta)) -> JqVal {
        let mut meta = self.meta().cloned().unwrap_or_default();
        f(&mut meta);
        JqVal::wrap(self.bare().clone(), meta)
    }

    /// jq's `type`.
    pub fn type_name(&self) -> &'static str {
        match self.bare() {
            JqVal::Null => "null",
            JqVal::Bool(_) => "boolean",
            JqVal::Num(..) => "number",
            JqVal::Str(_) => "string",
            JqVal::Arr(_) => "array",
            JqVal::Obj(_) => "object",
            JqVal::Node(_) => unreachable!("bare() never returns a Node"),
        }
    }

    /// jq truthiness: only `false` and `null` are falsy. `0`, `""`, `[]` and
    /// `{}` are all TRUE, which is the rule `select` rides on.
    pub fn truthy(&self) -> bool {
        !matches!(self.bare(), JqVal::Null | JqVal::Bool(false))
    }

    fn as_f64(&self) -> Option<f64> {
        match self.bare() {
            JqVal::Num(n, _) => Some(*n),
            _ => None,
        }
    }

    /// The rank of this value's type in jq's total order:
    /// `null < false < true < numbers < strings < arrays < objects`.
    fn order_rank(&self) -> u8 {
        match self.bare() {
            JqVal::Null => 0,
            JqVal::Bool(false) => 1,
            JqVal::Bool(true) => 2,
            JqVal::Num(..) => 3,
            JqVal::Str(_) => 4,
            JqVal::Arr(_) => 5,
            JqVal::Obj(_) => 6,
            JqVal::Node(_) => unreachable!("bare() never returns a Node"),
        }
    }

    /// Look a key up in an object, unboxing the container first. The public
    /// twin of `obj_get`, for the yq encoders that walk a value they did not
    /// build.
    pub fn obj_lookup(&self, k: &str) -> Option<&JqVal> {
        self.obj_get(k)
    }

    fn obj_get(&self, k: &str) -> Option<&JqVal> {
        match self.bare() {
            JqVal::Obj(m) => m.iter().find(|(key, _)| &**key == k).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// jq's order over values (`<`, `==`, `min`, `max`, `unique`, `group_by`).
///
/// Ported from jq 1.8.2 `src/jv.c:jv_cmp`. Objects compare by their SORTED key
/// list first and only then by the values at those keys, which is why
/// `{"a":1} < {"b":0}` even though `1 > 0`. A NaN compares as `null` against a
/// number -- below every number, itself included -- so `nan < 1`, `nan < nan`
/// and `nan != nan` are all true, which makes this NOT a total order; sorting
/// uses [`cmp_sort`].
pub fn cmp_vals(a: &JqVal, b: &JqVal) -> Ordering {
    cmp_with(a, b, false)
}

/// The order `sort` and `sort_by` use: [`cmp_vals`] with two NaNs equal, so a
/// sort is handed a total order. Where jq's `nan < nan` would have the sort
/// place one NaN before another, the two are indistinguishable anyway.
pub fn cmp_sort(a: &JqVal, b: &JqVal) -> Ordering {
    cmp_with(a, b, true)
}

/// Compare two finite decimal literals exactly (`None` for anything else, such
/// as a non-finite one). The value is `±0.DIGITS × 10^pos`: the sign decides
/// first (every zero is equal), then the position of the leading digit, then
/// the digits themselves with trailing zeros removed.
fn cmp_decimal(a: &str, b: &str) -> Option<Ordering> {
    fn parts(s: &str) -> Option<(bool, i64, String)> {
        let (neg, s) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        let (mant, exp) = match s.find(['e', 'E']) {
            Some(i) => (&s[..i], s[i + 1..].parse::<i64>().ok()?),
            None => (s, 0),
        };
        let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
        if !(int.bytes().chain(frac.bytes())).all(|c| c.is_ascii_digit()) {
            return None;
        }
        let all = format!("{int}{frac}");
        let lead = all.len() - all.trim_start_matches('0').len();
        let digits = all[lead..].trim_end_matches('0').to_string();
        // Position of the leading digit relative to the decimal point.
        let pos = exp + int.len() as i64 - lead as i64;
        Some((neg && !digits.is_empty(), pos, digits))
    }
    let (na, pa, da) = parts(a)?;
    let (nb, pb, db) = parts(b)?;
    let mag = |p: i64, d: &String, q: i64, e: &String| match (d.is_empty(), e.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => p.cmp(&q).then_with(|| d.cmp(e)),
    };
    Some(match (na, nb) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => mag(pa, &da, pb, &db),
        (true, true) => mag(pb, &db, pa, &da),
    })
}

fn cmp_with(a: &JqVal, b: &JqVal, total: bool) -> Ordering {
    let (ra, rb) = (a.order_rank(), b.order_rank());
    if ra != rb {
        return ra.cmp(&rb);
    }
    // Order is a property of the VALUE. A YAML node's comment or anchor must not
    // reorder a sort, so both sides are unboxed before the comparison.
    let (a, b) = (a.bare(), b.bare());
    match (a, b) {
        (JqVal::Num(x, _), JqVal::Num(y, _)) => match (x.is_nan(), y.is_nan()) {
            (true, true) if total => Ordering::Equal,
            (true, _) => Ordering::Less,
            (false, true) => Ordering::Greater,
            // Two number LITERALS compare as decimals, as jq 1.8's
            // `jvp_number_cmp` does with decNumber: `100000000000000000001` is
            // greater than `100000000000000000000` though both are one double.
            _ => match (a, b) {
                (JqVal::Num(_, Some(la)), JqVal::Num(_, Some(lb))) => cmp_decimal(la, lb),
                _ => None,
            }
            .unwrap_or_else(|| x.partial_cmp(y).unwrap_or(Ordering::Equal)),
        },
        (JqVal::Str(x), JqVal::Str(y)) => x.cmp(y),
        (JqVal::Arr(x), JqVal::Arr(y)) => {
            for (ea, eb) in x.iter().zip(y.iter()) {
                let c = cmp_with(ea, eb, total);
                if c != Ordering::Equal {
                    return c;
                }
            }
            x.len().cmp(&y.len())
        }
        (JqVal::Obj(x), JqVal::Obj(y)) => {
            let mut ka: Vec<&Rc<str>> = x.iter().map(|(k, _)| k).collect();
            let mut kb: Vec<&Rc<str>> = y.iter().map(|(k, _)| k).collect();
            ka.sort();
            kb.sort();
            let c = ka.cmp(&kb);
            if c != Ordering::Equal {
                return c;
            }
            for k in ka {
                let c = cmp_with(
                    a.obj_get(k).unwrap_or(&JqVal::Null),
                    b.obj_get(k).unwrap_or(&JqVal::Null),
                    total,
                );
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        }
        _ => Ordering::Equal,
    }
}

/// jq's `==`: the total order's equality, so type counts (`1` is not `"1"`).
pub fn eq_vals(a: &JqVal, b: &JqVal) -> bool {
    cmp_vals(a, b) == Ordering::Equal
}

// ─────────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────────

/// A jq runtime signal. `Break` is not a failure — it is how `label $l | … |
/// break $l` unwinds, and how `first`/`limit`/`any`/`all` stop early.
#[derive(Debug)]
pub enum JqErr {
    /// `error(v)`. The payload is a jq VALUE, because `try f catch .` receives it.
    Err(JqVal),
    /// Unwind to the matching `label`.
    Break(u64),
    /// `halt` / `halt_error`: stop the whole program with this exit status.
    Halt(i32, Option<JqVal>),
}

impl JqErr {
    fn msg(s: impl Into<String>) -> Self {
        JqErr::Err(JqVal::str(s.into()))
    }
    /// The one-line text jq prints for this error on stderr.
    pub fn to_message(&self) -> String {
        match self {
            JqErr::Err(JqVal::Str(s)) => s.to_string(),
            JqErr::Err(v) => format!("{} ({}) not a string", v.type_name(), render(v)),
            JqErr::Break(_) => "break".to_string(),
            JqErr::Halt(..) => "halt".to_string(),
        }
    }
}

type R<T> = Result<T, JqErr>;
/// Where a filter's output stream goes. Returning `Err` aborts the stream, which
/// is what makes `limit`/`first`/`any` stop pulling instead of materializing.
type Sink<'a> = &'a mut dyn FnMut(JqVal) -> R<()>;

// ─────────────────────────────────────────────────────────────────────────────
// JSON: parse (literal-preserving) and render (jq-compatible)
// ─────────────────────────────────────────────────────────────────────────────

/// Parse one JSON document, preserving each number's source literal.
///
/// Hand-written rather than delegated to `serde_json` for the reason the module
/// header gives: the two things this keeps — key ORDER and the number LITERAL —
/// are precisely the two `serde_json::Value` discards.
pub fn parse_json(src: &str) -> Result<JqVal, String> {
    parse_json_with(src)
}

fn parse_json_with(src: &str) -> Result<JqVal, String> {
    let b = src.as_bytes();
    let mut p = JsonParser { b, i: 0 };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i != b.len() {
        return Err(format!("trailing garbage at byte {}", p.i));
    }
    Ok(v)
}

/// Parse a stream of whitespace-separated JSON documents (jq's own input model).
pub fn parse_json_stream(src: &str) -> Result<Vec<JqVal>, String> {
    let b = src.as_bytes();
    let mut p = JsonParser { b, i: 0 };
    let mut out = Vec::new();
    loop {
        p.ws();
        if p.i >= b.len() {
            return Ok(out);
        }
        out.push(p.value()?);
    }
}

struct JsonParser<'a> {
    b: &'a [u8],
    i: usize,
}

impl JsonParser<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    fn value(&mut self) -> Result<JqVal, String> {
        match self.b.get(self.i) {
            None => Err("unexpected end of input".into()),
            Some(b'n') => self.lit("null", JqVal::Null),
            Some(b't') => self.lit("true", JqVal::Bool(true)),
            Some(b'f') => self.lit("false", JqVal::Bool(false)),
            Some(b'"') => Ok(JqVal::Str(self.string_rc()?)),
            Some(b'[') => self.array(),
            Some(b'{') => self.object(),
            Some(_) => self.number(),
        }
    }
    fn lit(&mut self, w: &str, v: JqVal) -> Result<JqVal, String> {
        if self.b[self.i..].starts_with(w.as_bytes()) {
            self.i += w.len();
            Ok(v)
        } else {
            Err(format!("expected `{w}` at byte {}", self.i))
        }
    }
    fn number(&mut self) -> Result<JqVal, String> {
        let start = self.i;
        if matches!(self.b.get(self.i), Some(b'-') | Some(b'+')) {
            self.i += 1;
        }
        while matches!(self.b.get(self.i), Some(c) if c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            while matches!(self.b.get(self.i), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        if matches!(self.b.get(self.i), Some(b'e') | Some(b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'-') | Some(b'+')) {
                self.i += 1;
            }
            while matches!(self.b.get(self.i), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        if self.i == start {
            return Err(format!("unexpected byte at {start}"));
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).map_err(|e| e.to_string())?;
        let n: f64 = text
            .parse()
            .map_err(|_| format!("bad number `{text}` at byte {start}"))?;
        // Only carry the literal when re-rendering the double would CHANGE it.
        // Storing it unconditionally would make every `1` an allocation for no
        // observable gain, and the equality check is the exact condition under
        // which the literal is load-bearing.
        Ok(num_from_literal(n, text))
    }
    /// A string, built straight from the source slice when it holds no escape —
    /// which is the overwhelmingly common case, and saves a `String` build plus a
    /// copy into the `Rc` for every key and every string value in the stream.
    fn string_rc(&mut self) -> Result<Rc<str>, String> {
        debug_assert_eq!(self.b[self.i], b'"');
        let start = self.i + 1;
        let mut j = start;
        while let Some(c) = self.b.get(j) {
            match c {
                b'"' => {
                    let raw = std::str::from_utf8(&self.b[start..j]).map_err(|e| e.to_string())?;
                    self.i = j + 1;
                    return Ok(Rc::from(raw));
                }
                b'\\' => break,
                _ => j += 1,
            }
        }
        if self.b.get(j).is_none() {
            return Err("unterminated string".into());
        }
        Ok(Rc::from(self.string()?.as_str()))
    }

    fn string(&mut self) -> Result<String, String> {
        debug_assert_eq!(self.b[self.i], b'"');
        self.i += 1;
        let mut s = String::new();
        loop {
            match self.b.get(self.i) {
                None => return Err("unterminated string".into()),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(s);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = *self.b.get(self.i).ok_or("unterminated escape")?;
                    self.i += 1;
                    match c {
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'b' => s.push('\u{8}'),
                        b'f' => s.push('\u{c}'),
                        b'n' => s.push('\n'),
                        b'r' => s.push('\r'),
                        b't' => s.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            // A high surrogate must pair with the `\uDC00`-range
                            // low one that follows, or the code point is lost.
                            let ch = if (0xD800..0xDC00).contains(&hi)
                                && self.b.get(self.i) == Some(&b'\\')
                                && self.b.get(self.i + 1) == Some(&b'u')
                            {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    char::from_u32(0x1_0000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
                                } else {
                                    self.i = save;
                                    None
                                }
                            } else {
                                char::from_u32(hi)
                            };
                            s.push(ch.unwrap_or('\u{fffd}'));
                        }
                        other => return Err(format!("bad escape `\\{}`", other as char)),
                    }
                }
                Some(_) => {
                    let start = self.i;
                    while !matches!(self.b.get(self.i), None | Some(b'"') | Some(b'\\')) {
                        self.i += 1;
                    }
                    s.push_str(
                        std::str::from_utf8(&self.b[start..self.i]).map_err(|e| e.to_string())?,
                    );
                }
            }
        }
    }
    fn hex4(&mut self) -> Result<u32, String> {
        let s = self
            .b
            .get(self.i..self.i + 4)
            .ok_or("truncated \\u escape")?;
        self.i += 4;
        u32::from_str_radix(std::str::from_utf8(s).map_err(|e| e.to_string())?, 16)
            .map_err(|e| e.to_string())
    }
    fn array(&mut self) -> Result<JqVal, String> {
        self.i += 1;
        let mut out = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(JqVal::arr(out));
        }
        loop {
            self.ws();
            out.push(self.value()?);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(JqVal::arr(out));
                }
                _ => return Err(format!("expected `,` or `]` at byte {}", self.i)),
            }
        }
    }
    fn object(&mut self) -> Result<JqVal, String> {
        self.i += 1;
        let mut out: Vec<(Rc<str>, JqVal)> = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(JqVal::obj(out));
        }
        loop {
            self.ws();
            if self.b.get(self.i) != Some(&b'"') {
                return Err(format!("expected a key string at byte {}", self.i));
            }
            let k: Rc<str> = self.string_rc()?;
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return Err(format!("expected `:` at byte {}", self.i));
            }
            self.i += 1;
            self.ws();
            let v = self.value()?;
            // A duplicate key keeps its FIRST position and takes the LAST value,
            // which is what jq's object builder does.
            match out.iter_mut().find(|(ek, _)| *ek == k) {
                Some(slot) => slot.1 = v,
                None => out.push((k, v)),
            }
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(JqVal::obj(out));
                }
                _ => return Err(format!("expected `,` or `}}` at byte {}", self.i)),
            }
        }
    }
}

/// jq's rendering of a number LITERAL it has not computed on.
///
/// jq 1.7+ keeps the source literal, but not verbatim: it stores it as a
/// decNumber and re-emits it through decNumber's "to-scientific-string", so
/// `1e2` comes back as `1E+2` and `12e3` as `1.2E+4` while `1.50` and
/// `100000000000000000000000` come back unchanged. Measured against jq 1.8.2
/// across the plain/exponential boundary (`0.000001` stays plain, `0.0000001`
/// becomes `1E-7`).
///
/// The rule, from decNumber's `decToString`: with a coefficient of `n` digits
/// and an exponent `exp`, the ADJUSTED exponent is `exp + n - 1`; plain notation
/// is used when `exp <= 0 && adjusted >= -6`, and exponential otherwise.
///
/// Returns `None` for text this cannot canonicalize, in which case the caller
/// keeps the double's own formatting.
fn canonical_num_literal(text: &str) -> Option<String> {
    let b = text.as_bytes();
    let mut i = 0;
    let neg = match b.first() {
        Some(b'-') => {
            i = 1;
            true
        }
        Some(b'+') => {
            i = 1;
            false
        }
        _ => false,
    };
    let int_start = i;
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let int_part = &text[int_start..i];
    let mut frac = "";
    if b.get(i) == Some(&b'.') {
        i += 1;
        let fs = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        frac = &text[fs..i];
    }
    let mut exp: i64 = 0;
    if matches!(b.get(i), Some(b'e') | Some(b'E')) {
        i += 1;
        let es = i;
        if matches!(b.get(i), Some(b'+') | Some(b'-')) {
            i += 1;
        }
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        exp = text[es..i].parse().ok()?;
    }
    if i != text.len() || (int_part.is_empty() && frac.is_empty()) {
        return None;
    }
    exp -= frac.len() as i64;
    let joined = format!("{int_part}{frac}");
    // decNumber holds a coefficient with no leading zeros (but never empty).
    let digits = joined.trim_start_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let n = digits.len() as i64;
    let sign = if neg { "-" } else { "" };
    // decNumber renders a zero coefficient with a zero exponent as plain `0`,
    // which is what makes `0e0` print as `0`.
    if digits == "0" && exp >= 0 {
        return Some(format!("{sign}0"));
    }
    let adjusted = exp + n - 1;
    if exp <= 0 && adjusted >= -6 {
        return Some(if exp == 0 {
            format!("{sign}{digits}")
        } else if n > -exp {
            let split = (n + exp) as usize;
            format!("{sign}{}.{}", &digits[..split], &digits[split..])
        } else {
            format!("{sign}0.{}{digits}", "0".repeat((-exp - n) as usize))
        });
    }
    let (head, rest) = digits.split_at(1);
    let mantissa = if rest.is_empty() {
        head.to_string()
    } else {
        format!("{head}.{rest}")
    };
    Some(format!(
        "{sign}{mantissa}E{}{}",
        if adjusted < 0 { "-" } else { "+" },
        adjusted.abs()
    ))
}

/// jq's unary minus. jq 1.8 negates a number that still carries its literal
/// as a decNumber, so the literal survives: `-100000000000000000001` and
/// `1.50 | -.` print exactly, and `-1E1000` is `-1E+1000` rather than the
/// clamped double. A zero coefficient negates to itself (`-0.0` is `0.0`),
/// which is decNumber's `0 - x`. Measured against jq 1.8.2.
fn negate_num(n: f64, lit: Option<&str>) -> JqVal {
    let Some(lit) = lit else {
        return JqVal::num(-n);
    };
    let neg = if let Some(rest) = lit.strip_prefix('-') {
        rest.to_string()
    } else if lit
        .split(['E', 'e'])
        .next()
        .unwrap_or("")
        .bytes()
        .all(|b| b == b'0' || b == b'.')
    {
        lit.to_string()
    } else {
        format!("-{lit}")
    };
    let v = if n == 0.0 { 0.0 } else { -n };
    if neg == fmt_num(v) {
        JqVal::Num(v, None)
    } else {
        JqVal::Num(v, Some(Rc::from(neg.as_str())))
    }
}

/// Build a number value from its source text, keeping the literal only when jq
/// would print something other than the double's own shortest form.
pub(crate) fn num_from_literal(n: f64, text: &str) -> JqVal {
    // A ZERO literal is kept even when it prints like the double: a literal
    // negates as a decNumber (`0 | -.` is `0`) while a computed zero negates as
    // a double (`(1-1) | -.` is `-0`), so the two must stay distinguishable.
    // So is one past 2^53, where distinct integer literals share a double and
    // only the literal still tells them apart in a comparison.
    if n == 0.0 || n.abs() >= 9_007_199_254_740_992.0 {
        if let Some(c) = canonical_num_literal(text) {
            return JqVal::Num(n, Some(Rc::from(c.as_str())));
        }
    }
    if is_plain_shortest(text) {
        return JqVal::Num(n, None);
    }
    match canonical_num_literal(text) {
        Some(c) if c != fmt_num(n) => JqVal::Num(n, Some(Rc::from(c.as_str()))),
        _ => JqVal::Num(n, None),
    }
}

/// Is `text` already both decNumber's canonical form AND the shortest decimal
/// that round-trips to its double? Then no literal need be kept and neither
/// formatter need run — which matters because this is on the JSON reader's
/// innermost path, once per number in the stream.
///
/// The test is deliberately conservative: no exponent, no leading zero (except a
/// lone `0` before the point), no leading zero INSIDE the fraction, no trailing
/// zero in the fraction, at most 15 significant digits, and at most 15 digits
/// before the point. Under 15 digits two distinct decimals cannot share a double
/// (an IEEE double carries ~15.95 decimal digits), so no SHORTER decimal can
/// round-trip to the same value and `text` is itself the shortest form; the two
/// magnitude bounds keep it inside the band where both formatters render plainly
/// rather than in exponent form. Without the fraction rule `0.000001` slipped
/// through and printed as `1e-06`.
///
/// Anything this rejects falls through to the exact path, so a false negative
/// costs time and never correctness.
fn is_plain_shortest(text: &str) -> bool {
    let b = text.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let int_start = i;
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let int_len = i - int_start;
    if int_len == 0 || int_len > 15 || (int_len > 1 && b[int_start] == b'0') {
        return false;
    }
    let mut sig = if int_len == 1 && b[int_start] == b'0' {
        0
    } else {
        int_len
    };
    if b.get(i) == Some(&b'.') {
        i += 1;
        let frac_start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        let frac_len = i - frac_start;
        if frac_len == 0 || b[i - 1] == b'0' {
            return false;
        }
        // A `0.0…` value is below the plain/exponent boundary the two formatters
        // draw differently, so only `0.<nonzero>` takes the fast path.
        if sig == 0 {
            if b[frac_start] == b'0' {
                return false;
            }
            sig += frac_len;
        } else {
            sig += frac_len;
        }
    }
    i == b.len() && sig > 0 && sig <= 15
}

/// jq's number rendering. Delegates to the formatter `crate::query` already
/// validated against `jq 1.8.2` over 200,000 doubles, so there is exactly one
/// number formatter in the tree and the two can never drift.
pub fn fmt_num(v: f64) -> String {
    // jq prints a computed negative zero with its sign (`0 * -1` is `-0`).
    if v == 0.0 && v.is_sign_negative() {
        return "-0".to_string();
    }
    crate::query::fmt_num(v)
}

/// Render a value as jq's compact JSON (`jq -c`).
pub fn render(v: &JqVal) -> String {
    let mut s = String::new();
    write_val(&mut s, v);
    s
}

/// Render a value the way `jq -r` prints it: a top-level STRING goes out raw,
/// everything else is compact JSON.
pub fn render_raw(v: &JqVal) -> String {
    match v.bare() {
        JqVal::Str(s) => s.to_string(),
        other => render(other),
    }
}

/// JSON has no place for YAML node metadata, so the box is dropped here: a
/// commented, anchored, single-quoted YAML scalar renders as exactly the JSON
/// its value alone would. `crate::ynode::emit` is the writer that keeps it.
fn write_val(out: &mut String, v: &JqVal) {
    match v.bare() {
        JqVal::Null => out.push_str("null"),
        JqVal::Bool(true) => out.push_str("true"),
        JqVal::Bool(false) => out.push_str("false"),
        JqVal::Num(n, lit) => match lit {
            Some(t) => out.push_str(t),
            None => out.push_str(&fmt_num(*n)),
        },
        JqVal::Str(s) => write_json_str(out, s),
        JqVal::Arr(a) => {
            out.push('[');
            for (i, e) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_val(out, e);
            }
            out.push(']');
        }
        JqVal::Obj(m) => {
            out.push('{');
            for (i, (k, val)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_str(out, k);
                out.push(':');
                write_val(out, val);
            }
            out.push('}');
        }
        JqVal::Node(_) => unreachable!("bare() never returns a Node"),
    }
}

/// JSON string escaping, matching jq's string writer: the seven short escapes,
/// and `\u00XX` for every remaining C0 control plus DEL (0x7f), which jq escapes
/// even though JSON does not require it.
fn write_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ─────────────────────────────────────────────────────────────────────────────
// Lexer
// ─────────────────────────────────────────────────────────────────────────────

/// One piece of a jq string literal. `"a\(.b)c"` is `Lit("a")`, `Interp(".b")`,
/// `Lit("c")`; the interpolation carries its RAW source and is parsed on demand,
/// which keeps the lexer non-recursive.
#[derive(Debug, Clone)]
pub(crate) enum StrPiece {
    Lit(String),
    Interp(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    /// `$name`.
    Var(String),
    /// `@name`.
    Format(String),
    Num(f64, String),
    Str(Vec<StrPieceTok>),
    /// `.name` lexed as ONE token. Splitting it into `.` + an identifier would
    /// make `if . then 1 else 2 end` read `else` as a field name, which is the
    /// same reason jq's own lexer emits a single `FIELD` token here.
    Field(String),
    Op(&'static str),
}

/// `StrPiece` inside a token needs `PartialEq` for the token stream's own
/// comparisons; the payload is plain text so deriving it is exact.
#[derive(Debug, Clone, PartialEq)]
enum StrPieceTok {
    Lit(String),
    Interp(String),
}

impl From<StrPieceTok> for StrPiece {
    fn from(t: StrPieceTok) -> Self {
        match t {
            StrPieceTok::Lit(s) => StrPiece::Lit(s),
            StrPieceTok::Interp(s) => StrPiece::Interp(s),
        }
    }
}

/// The multi-character operators, longest first so `//=` never lexes as `//`
/// then `=`, and `?//` never as `?` then `//`.
const OPS: &[&str] = &[
    "?//", "//=", "|=", "+=", "-=", "*=", "/=", "%=", "==", "!=", "<=", ">=", "//", "..", "|", ",",
    "=", "<", ">", "+", "-", "*", "/", "%", "(", ")", "[", "]", "{", "}", ":", ";", "?", ".",
];

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let cs: Vec<char> = src.chars().collect();
    let mut i = 0usize;
    let mut out = Vec::new();
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '#' {
            while i < cs.len() && cs[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '"' {
            let (pieces, next) = lex_string(&cs, i)?;
            out.push(Tok::Str(pieces));
            i = next;
            continue;
        }
        if c == '$' {
            let start = i + 1;
            let mut j = start;
            while j < cs.len() && (cs[j].is_alphanumeric() || cs[j] == '_') {
                j += 1;
            }
            if j == start {
                return Err("jq: `$` must be followed by a variable name".into());
            }
            out.push(Tok::Var(cs[start..j].iter().collect()));
            i = j;
            continue;
        }
        if c == '@' {
            let start = i + 1;
            let mut j = start;
            while j < cs.len() && (cs[j].is_alphanumeric() || cs[j] == '_') {
                j += 1;
            }
            if j == start {
                return Err("jq: `@` must be followed by a format name".into());
            }
            out.push(Tok::Format(cs[start..j].iter().collect()));
            i = j;
            continue;
        }
        // `.name` -- but not `..`, and not a `.` that begins a number (`.5`).
        if c == '.'
            && cs
                .get(i + 1)
                .is_some_and(|n| n.is_alphabetic() || *n == '_')
        {
            let start = i + 1;
            let mut j = start;
            while j < cs.len() && (cs[j].is_alphanumeric() || cs[j] == '_') {
                j += 1;
            }
            out.push(Tok::Field(cs[start..j].iter().collect()));
            i = j;
            continue;
        }
        // A number, lexed as jq 1.8's scanner does: `([0-9]+(\.[0-9]*)?|\.[0-9]+)`
        // then an optional exponent, maximal munch. So `.5` is 0.5 and `1.` is
        // 1, and `1.foo` is the literal `1.` followed by `foo` -- the syntax
        // error jq reports -- rather than a field access on 1.
        let fraction_first = c == '.' && cs.get(i + 1).is_some_and(|d| d.is_ascii_digit());
        if c.is_ascii_digit() || fraction_first {
            let start = i;
            while i < cs.len() && cs[i].is_ascii_digit() {
                i += 1;
            }
            if cs.get(i) == Some(&'.') {
                i += 1;
                while i < cs.len() && cs[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if matches!(cs.get(i), Some('e') | Some('E')) {
                let save = i;
                let mut j = i + 1;
                if matches!(cs.get(j), Some('+') | Some('-')) {
                    j += 1;
                }
                if matches!(cs.get(j), Some(d) if d.is_ascii_digit()) {
                    while matches!(cs.get(j), Some(d) if d.is_ascii_digit()) {
                        j += 1;
                    }
                    i = j;
                } else {
                    i = save;
                }
            }
            let text: String = cs[start..i].iter().collect();
            let n: f64 = text
                .parse()
                .map_err(|_| format!("jq: bad number literal `{text}`"))?;
            out.push(Tok::Num(n, text));
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '_') {
                i += 1;
            }
            // `a::b` is jq's module-qualified name; keep it as one identifier so
            // the parser sees the whole name rather than a stray `:`.
            while cs.get(i) == Some(&':') && cs.get(i + 1) == Some(&':') {
                i += 2;
                while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '_') {
                    i += 1;
                }
            }
            out.push(Tok::Ident(cs[start..i].iter().collect()));
            continue;
        }
        let rest: String = cs[i..].iter().collect();
        match OPS.iter().find(|op| rest.starts_with(**op)) {
            Some(op) => {
                out.push(Tok::Op(op));
                i += op.chars().count();
            }
            None => return Err(format!("jq: unexpected character `{c}`")),
        }
    }
    Ok(out)
}

/// Lex a `"…"` literal starting at `cs[i]`, splitting out `\(…)` interpolations.
fn lex_string(cs: &[char], i: usize) -> Result<(Vec<StrPieceTok>, usize), String> {
    let mut i = i + 1;
    let mut pieces = Vec::new();
    let mut cur = String::new();
    while i < cs.len() {
        match cs[i] {
            '"' => {
                if !cur.is_empty() || pieces.is_empty() {
                    pieces.push(StrPieceTok::Lit(cur));
                }
                return Ok((pieces, i + 1));
            }
            '\\' => {
                let e = *cs.get(i + 1).ok_or("jq: unterminated string escape")?;
                if e == '(' {
                    // Interpolation: copy the balanced parenthesised source out
                    // verbatim, tracking nested strings so a `)` inside one does
                    // not close it early.
                    if !cur.is_empty() {
                        pieces.push(StrPieceTok::Lit(std::mem::take(&mut cur)));
                    }
                    let start = i + 2;
                    let mut j = start;
                    let mut depth = 1i32;
                    let mut in_str = false;
                    while j < cs.len() {
                        match cs[j] {
                            '\\' if in_str => j += 1,
                            '"' => in_str = !in_str,
                            '(' if !in_str => depth += 1,
                            ')' if !in_str => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        j += 1;
                    }
                    if j >= cs.len() {
                        return Err("jq: unterminated string interpolation".into());
                    }
                    pieces.push(StrPieceTok::Interp(cs[start..j].iter().collect()));
                    i = j + 1;
                    continue;
                }
                cur.push(match e {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    '/' => '/',
                    '\\' => '\\',
                    '"' => '"',
                    'u' => {
                        let hex: String = cs
                            .get(i + 2..i + 6)
                            .ok_or("jq: truncated \\u")?
                            .iter()
                            .collect();
                        let cp = u32::from_str_radix(&hex, 16).map_err(|_| "jq: bad \\u escape")?;
                        i += 4;
                        // Surrogate pair, same rule as the JSON reader.
                        if (0xD800..0xDC00).contains(&cp)
                            && cs.get(i + 2) == Some(&'\\')
                            && cs.get(i + 3) == Some(&'u')
                        {
                            let hex2: String = cs
                                .get(i + 4..i + 8)
                                .ok_or("jq: truncated \\u")?
                                .iter()
                                .collect();
                            if let Ok(lo) = u32::from_str_radix(&hex2, 16) {
                                if (0xDC00..0xE000).contains(&lo) {
                                    i += 6;
                                    cur.push(
                                        char::from_u32(
                                            0x1_0000 + ((cp - 0xD800) << 10) + (lo - 0xDC00),
                                        )
                                        .unwrap_or('\u{fffd}'),
                                    );
                                    i += 2;
                                    continue;
                                }
                            }
                        }
                        char::from_u32(cp).unwrap_or('\u{fffd}')
                    }
                    other => return Err(format!("jq: bad escape `\\{other}`")),
                });
                i += 2;
            }
            c => {
                cur.push(c);
                i += 1;
            }
        }
    }
    Err("jq: unterminated string".into())
}

// ─────────────────────────────────────────────────────────────────────────────
// AST
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// The update-assignment family. `Set` is `=`, `Update` is `|=`, and the rest
/// are jq's arithmetic update forms, which are defined as `a op= b` ==
/// `a |= . op b` with `b` evaluated against the ORIGINAL input (`$__x`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssignOp {
    Set,
    Update,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Alt,
}

/// A destructuring pattern for `as`, `reduce`, `foreach`.
#[derive(Debug, Clone)]
pub(crate) enum Pattern {
    Var(Rc<str>),
    Arr(Vec<Pattern>),
    /// Each entry is (key filter, optional sub-pattern). `{$a}` is sugar for
    /// `{a: $a}` and produces `(Lit("a"), None)` with the variable named `a`.
    Obj(Vec<ObjPatEntry>),
}

#[derive(Debug, Clone)]
pub(crate) struct FuncDef {
    name: Rc<str>,
    /// Filter parameters. A `$x` parameter is desugared at parse time into a
    /// filter parameter plus a `. as $x` binding around the body, which is what
    /// jq's own `parser.y` does.
    params: Vec<Rc<str>>,
    body: Rc<Filter>,
}

#[derive(Debug, Clone)]
pub(crate) enum ObjEntry {
    /// `key: value`, where the key is any filter producing a string.
    KeyVal(Filter, Filter),
}

#[derive(Debug, Clone)]
pub(crate) enum Filter {
    Identity,
    /// `..` — jq's `recurse`.
    RecurseDefault,
    Lit(JqVal),
    /// An interpolated string, optionally under a `@fmt "…"` prefix which
    /// applies that format to every INTERPOLATED piece (never to the literals).
    Str(Vec<StrPiece>, Option<Rc<str>>),
    /// `@base64` used as a filter in its own right.
    Format(Rc<str>),
    Field(Box<Filter>, Rc<str>),
    Index(Box<Filter>, Box<Filter>),
    Slice(Box<Filter>, Option<Box<Filter>>, Option<Box<Filter>>),
    Iterate(Box<Filter>),
    /// `f?` — swallow errors raised by `f`, emitting nothing instead.
    Optional(Box<Filter>),
    /// `.a?`, `.[e]?`, `.[a:b]?`, `.[]?` — parser.y's INDEX_OPT/EACH_OPT. The
    /// inner filter is a `Field`/`Index`/`Slice`/`Iterate`, and only that
    /// step's own indexing error is suppressed: an error from its base or its
    /// key still raises.
    IndexOpt(Box<Filter>),
    Pipe(Box<Filter>, Box<Filter>),
    Comma(Box<Filter>, Box<Filter>),
    Neg(Box<Filter>),
    Bin(BinOp, Box<Filter>, Box<Filter>),
    And(Box<Filter>, Box<Filter>),
    Or(Box<Filter>, Box<Filter>),
    Alt(Box<Filter>, Box<Filter>),
    Assign(AssignOp, Box<Filter>, Box<Filter>),
    /// `if a then b elif c then d else e end`; the `else` may be absent, in
    /// which case a false condition yields the INPUT unchanged (jq 1.7+).
    If(Vec<(Filter, Filter)>, Option<Box<Filter>>),
    Try(Box<Filter>, Option<Box<Filter>>),
    Reduce(Box<Filter>, Pattern, Box<Filter>, Box<Filter>),
    Foreach(
        Box<Filter>,
        Pattern,
        Box<Filter>,
        Box<Filter>,
        Option<Box<Filter>>,
    ),
    /// `SOURCE as PAT ?// PAT | BODY`.
    Bind(Box<Filter>, Vec<Pattern>, Box<Filter>),
    Label(Rc<str>, Box<Filter>),
    Break(Rc<str>),
    Var(Rc<str>),
    Call(Rc<str>, Vec<Rc<Filter>>),
    Def(Rc<FuncDef>, Box<Filter>),
    Object(Vec<ObjEntry>),
    /// `[f]`, or `[]` when the inner filter is absent.
    Array(Option<Box<Filter>>),
}

// ─────────────────────────────────────────────────────────────────────────────
// Parser
// ─────────────────────────────────────────────────────────────────────────────

struct Parser {
    t: Vec<Tok>,
    i: usize,
}

/// Parse a complete jq program.
pub(crate) fn parse(src: &str) -> Result<Filter, String> {
    let toks = lex(src)?;
    let mut p = Parser { t: toks, i: 0 };
    let f = p.pipe()?;
    if p.i != p.t.len() {
        return Err(format!("jq: unexpected `{}` in `{src}`", p.describe(p.i)));
    }
    Ok(f)
}

impl Parser {
    fn describe(&self, i: usize) -> String {
        match self.t.get(i) {
            None => "end of program".into(),
            Some(Tok::Ident(s)) => s.clone(),
            Some(Tok::Var(s)) => format!("${s}"),
            Some(Tok::Format(s)) => format!("@{s}"),
            Some(Tok::Num(_, s)) => s.clone(),
            Some(Tok::Field(f)) => format!(".{f}"),
            Some(Tok::Str(_)) => "a string".into(),
            Some(Tok::Op(o)) => (*o).to_string(),
        }
    }
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn is_op(&self, op: &str) -> bool {
        matches!(self.peek(), Some(Tok::Op(o)) if *o == op)
    }
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s == kw)
    }
    fn eat_op(&mut self, op: &str) -> bool {
        if self.is_op(op) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn want_op(&mut self, op: &str) -> Result<(), String> {
        if self.eat_op(op) {
            Ok(())
        } else {
            Err(format!(
                "jq: expected `{op}`, found `{}`",
                self.describe(self.i)
            ))
        }
    }
    fn want_kw(&mut self, kw: &str) -> Result<(), String> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(format!(
                "jq: expected `{kw}`, found `{}`",
                self.describe(self.i)
            ))
        }
    }
    fn ident(&mut self) -> Result<Rc<str>, String> {
        match self.peek() {
            Some(Tok::Ident(s)) => {
                let s: Rc<str> = Rc::from(s.as_str());
                self.i += 1;
                Ok(s)
            }
            _ => Err(format!(
                "jq: expected a name, found `{}`",
                self.describe(self.i)
            )),
        }
    }
    fn var(&mut self) -> Result<Rc<str>, String> {
        match self.peek() {
            Some(Tok::Var(s)) => {
                let s: Rc<str> = Rc::from(s.as_str());
                self.i += 1;
                Ok(s)
            }
            _ => Err(format!(
                "jq: expected `$name`, found `{}`",
                self.describe(self.i)
            )),
        }
    }

    /// `|` — the loosest binder, right-associative, and the level `def` and
    /// `as`-bindings extend over.
    fn pipe(&mut self) -> Result<Filter, String> {
        if self.is_kw("def") {
            let def = self.funcdef()?;
            let rest = self.pipe()?;
            return Ok(Filter::Def(Rc::new(def), Box::new(rest)));
        }
        if self.is_kw("label") {
            self.i += 1;
            let name = self.var()?;
            self.want_op("|")?;
            let body = self.pipe()?;
            return Ok(Filter::Label(name, Box::new(body)));
        }
        let lhs = self.comma()?;
        if self.eat_op("|") {
            let rhs = self.pipe()?;
            return Ok(Filter::Pipe(Box::new(lhs), Box::new(rhs)));
        }
        Ok(lhs)
    }

    fn funcdef(&mut self) -> Result<FuncDef, String> {
        self.want_kw("def")?;
        let name = self.ident()?;
        let mut params = Vec::new();
        let mut value_params = Vec::new();
        if self.eat_op("(") {
            loop {
                match self.peek() {
                    Some(Tok::Var(v)) => {
                        // `def f($a)` is jq's sugar for a filter parameter plus
                        // `. as $a` at the top of the body, evaluated once.
                        let v: Rc<str> = Rc::from(v.as_str());
                        self.i += 1;
                        params.push(v.clone());
                        value_params.push(v);
                    }
                    _ => params.push(self.ident()?),
                }
                if self.eat_op(";") {
                    continue;
                }
                self.want_op(")")?;
                break;
            }
        }
        self.want_op(":")?;
        let mut body = self.pipe()?;
        self.want_op(";")?;
        for v in value_params.into_iter().rev() {
            body = Filter::Bind(
                Box::new(Filter::Call(v.clone(), vec![])),
                vec![Pattern::Var(v)],
                Box::new(body),
            );
        }
        Ok(FuncDef {
            name,
            params,
            body: Rc::new(body),
        })
    }

    fn pattern(&mut self) -> Result<Pattern, String> {
        match self.peek() {
            Some(Tok::Var(_)) => Ok(Pattern::Var(self.var()?)),
            Some(Tok::Op("[")) => {
                self.i += 1;
                let mut out = Vec::new();
                if !self.eat_op("]") {
                    loop {
                        out.push(self.pattern()?);
                        if self.eat_op(",") {
                            continue;
                        }
                        self.want_op("]")?;
                        break;
                    }
                }
                Ok(Pattern::Arr(out))
            }
            Some(Tok::Op("{")) => {
                self.i += 1;
                let mut out = Vec::new();
                loop {
                    match self.peek().cloned() {
                        // `{$a}` — bind `.a` to `$a`. `{$a: P}` binds `$a` to `.a` as
                        // well and destructures the same value with `P`: the
                        // key is the variable's NAME, never its value.
                        Some(Tok::Var(v)) => {
                            self.i += 1;
                            let name: Rc<str> = Rc::from(v.as_str());
                            let sub = if self.eat_op(":") {
                                Some(self.pattern()?)
                            } else {
                                None
                            };
                            out.push((Filter::Lit(JqVal::str(v.clone())), sub, Some(name)));
                        }
                        Some(Tok::Ident(k)) => {
                            self.i += 1;
                            self.want_op(":")?;
                            let sub = self.pattern()?;
                            out.push((Filter::Lit(JqVal::str(k)), Some(sub), None));
                        }
                        Some(Tok::Str(pieces)) => {
                            self.i += 1;
                            self.want_op(":")?;
                            let sub = self.pattern()?;
                            out.push((
                                Filter::Str(pieces.into_iter().map(Into::into).collect(), None),
                                Some(sub),
                                None,
                            ));
                        }
                        Some(Tok::Op("(")) => {
                            self.i += 1;
                            let k = self.pipe()?;
                            self.want_op(")")?;
                            self.want_op(":")?;
                            let sub = self.pattern()?;
                            out.push((k, Some(sub), None));
                        }
                        _ => {
                            return Err(format!(
                                "jq: bad object pattern near `{}`",
                                self.describe(self.i)
                            ))
                        }
                    }
                    if self.eat_op(",") {
                        continue;
                    }
                    self.want_op("}")?;
                    break;
                }
                Ok(Pattern::Obj(out))
            }
            _ => Err(format!(
                "jq: expected a destructuring pattern, found `{}`",
                self.describe(self.i)
            )),
        }
    }

    /// `,` — left-associative, binds tighter than `|`.
    fn comma(&mut self) -> Result<Filter, String> {
        let mut lhs = self.bind()?;
        while self.eat_op(",") {
            let rhs = self.bind()?;
            lhs = Filter::Comma(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// `Expr "as" Patterns '|' Query` — jq 1.8's production. The SOURCE is a
    /// whole operator expression (everything that binds tighter than `,`), and
    /// the BODY is a full query running to the end of the pipeline. Measured
    /// against jq 1.8.2: `1 + 2 as $x | $x * 10` is `30`, `-1 as $x | 5` is
    /// `5`, and `[1, 2 as $x | $x]` is `[1,2]` — the `,` stays outside.
    fn bind(&mut self) -> Result<Filter, String> {
        let src = self.alt()?;
        if !self.is_kw("as") {
            return Ok(src);
        }
        self.i += 1;
        let mut pats = vec![self.pattern()?];
        while self.eat_op("?//") {
            pats.push(self.pattern()?);
        }
        // yq's POSTFIX REDUCE: `.[] as $item ireduce (0; . + $item)`, where
        // jq writes `reduce .[] as $item (0; . + $item)`. Same three parts in
        // a different order, and `as … ireduce` is not valid jq either — jq
        // requires a `|` here — so this is another shape with no owner.
        if matches!(self.peek(), Some(Tok::Ident(n)) if n == "ireduce") {
            self.i += 1;
            self.want_op("(")?;
            let init = self.pipe()?;
            self.want_op(";")?;
            let update = self.pipe()?;
            self.want_op(")")?;
            return Ok(Filter::Reduce(
                Box::new(src),
                pats.remove(0),
                Box::new(init),
                Box::new(update),
            ));
        }
        self.want_op("|")?;
        let body = self.pipe()?;
        Ok(Filter::Bind(Box::new(src), pats, Box::new(body)))
    }

    /// `//` — right-associative (jq's `%right "//"`).
    fn alt(&mut self) -> Result<Filter, String> {
        let lhs = self.assign()?;
        if self.eat_op("//") {
            let rhs = self.alt()?;
            return Ok(Filter::Alt(Box::new(lhs), Box::new(rhs)));
        }
        Ok(lhs)
    }

    /// The assignment family — non-associative in jq's grammar, so exactly one
    /// operator is accepted at this level.
    fn assign(&mut self) -> Result<Filter, String> {
        let lhs = self.or()?;
        let op = match self.peek() {
            Some(Tok::Op("=")) => AssignOp::Set,
            Some(Tok::Op("|=")) => AssignOp::Update,
            Some(Tok::Op("+=")) => AssignOp::Add,
            Some(Tok::Op("-=")) => AssignOp::Sub,
            Some(Tok::Op("*=")) => AssignOp::Mul,
            Some(Tok::Op("/=")) => AssignOp::Div,
            Some(Tok::Op("%=")) => AssignOp::Mod,
            Some(Tok::Op("//=")) => AssignOp::Alt,
            _ => return Ok(lhs),
        };
        self.i += 1;
        let rhs = self.or()?;
        Ok(Filter::Assign(op, Box::new(lhs), Box::new(rhs)))
    }

    fn or(&mut self) -> Result<Filter, String> {
        let mut lhs = self.and()?;
        while self.is_kw("or") {
            self.i += 1;
            let rhs = self.and()?;
            lhs = Filter::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn and(&mut self) -> Result<Filter, String> {
        let mut lhs = self.compare()?;
        while self.is_kw("and") {
            self.i += 1;
            let rhs = self.compare()?;
            lhs = Filter::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// The comparisons are `%nonassoc` in jq, so at most one appears here.
    fn compare(&mut self) -> Result<Filter, String> {
        let lhs = self.additive()?;
        let op = match self.peek() {
            Some(Tok::Op("==")) => BinOp::Eq,
            Some(Tok::Op("!=")) => BinOp::Ne,
            Some(Tok::Op("<")) => BinOp::Lt,
            Some(Tok::Op("<=")) => BinOp::Le,
            Some(Tok::Op(">")) => BinOp::Gt,
            Some(Tok::Op(">=")) => BinOp::Ge,
            _ => return Ok(lhs),
        };
        self.i += 1;
        let rhs = self.additive()?;
        Ok(Filter::Bin(op, Box::new(lhs), Box::new(rhs)))
    }

    fn additive(&mut self) -> Result<Filter, String> {
        let mut lhs = self.multiplicative()?;
        loop {
            let op = if self.is_op("+") {
                BinOp::Add
            } else if self.is_op("-") {
                BinOp::Sub
            } else {
                return Ok(lhs);
            };
            self.i += 1;
            let rhs = self.multiplicative()?;
            lhs = Filter::Bin(op, Box::new(lhs), Box::new(rhs));
        }
    }

    fn multiplicative(&mut self) -> Result<Filter, String> {
        let mut lhs = self.unary()?;
        loop {
            let op = if self.is_op("*") {
                BinOp::Mul
            } else if self.is_op("/") {
                BinOp::Div
            } else if self.is_op("%") {
                BinOp::Mod
            } else {
                return Ok(lhs);
            };
            self.i += 1;
            let rhs = self.unary()?;
            lhs = Filter::Bin(op, Box::new(lhs), Box::new(rhs));
        }
    }

    fn unary(&mut self) -> Result<Filter, String> {
        if self.eat_op("-") {
            let inner = self.unary()?;
            return Ok(Filter::Neg(Box::new(inner)));
        }
        self.postfix()
    }
}

impl Parser {
    /// A term plus its postfix chain (`.k`, `[e]`, `[]`, `[a:b]`, `?`). An
    /// `as`-binding is not part of it: its source is a whole expression (see
    /// `bind`).
    fn postfix(&mut self) -> Result<Filter, String> {
        // parser.y gives `?` two meanings. Directly after an index step
        // (`Term FIELD '?'`, `Term '[' Query ']' '?'`, `Term '[' ']' '?'`, the
        // slices) it is INDEX_OPT/EACH_OPT, which suppresses only that step's
        // own error; anywhere else (`Term '?'`) it is `try`. `index_step` says
        // whether the term so far ends in such a step.
        let mut index_step = matches!(
            (self.peek(), self.t.get(self.i + 1)),
            (Some(Tok::Field(_)), _) | (Some(Tok::Op(".")), Some(Tok::Str(_) | Tok::Op("[")))
        );
        let mut f = self.term()?;
        loop {
            if self.eat_op("?") {
                f = if index_step {
                    Filter::IndexOpt(Box::new(f))
                } else {
                    Filter::Optional(Box::new(f))
                };
                index_step = false;
                continue;
            }
            // Every continuation below but the metadata postfix is an index step.
            index_step = true;
            if let Some(Tok::Field(name)) = self.peek() {
                let name: Rc<str> = Rc::from(name.as_str());
                self.i += 1;
                f = Filter::Field(Box::new(f), name);
                continue;
            }
            if self.is_op(".") {
                // `."foo"` continuing a term. A bare `.` followed by `[` is the
                // `.[…]` form, also a continuation.
                match self.t.get(self.i + 1) {
                    Some(Tok::Str(pieces)) => {
                        let pieces: Vec<StrPiece> =
                            pieces.clone().into_iter().map(Into::into).collect();
                        self.i += 2;
                        f = Filter::Index(Box::new(f), Box::new(Filter::Str(pieces, None)));
                        continue;
                    }
                    Some(Tok::Op("[")) => {
                        self.i += 1;
                        continue;
                    }
                    _ => {}
                }
            }
            if self.is_op("[") {
                self.i += 1;
                f = self.bracket_suffix(f)?;
                continue;
            }
            // yq's METADATA POSTFIX: `.a anchor`, and with it `.a anchor = "x"`.
            //
            // jq never allows a bare identifier to follow a complete expression —
            // juxtaposition is a syntax error in every jq grammar position — so
            // claiming this shape takes nothing away from the jq leg. It desugars
            // to the pipe arb already spelled it with, which is what makes the
            // ASSIGNMENT form work for free: `.a anchor = "x"` is
            // `Assign(Set, Pipe(.a, anchor), "x")`, and `eval_assign` already
            // recognises that left-hand side and edits the whole document.
            if let Some(Tok::Ident(name)) = self.peek() {
                if is_meta_setter(name) || matches!(&**name, "alias" | "kind") {
                    let name: Rc<str> = Rc::from(name.as_str());
                    self.i += 1;
                    f = Filter::Pipe(Box::new(f), Box::new(Filter::Call(name, Vec::new())));
                    index_step = false;
                    continue;
                }
            }
            break;
        }
        Ok(f)
    }

    /// The `[…]` suffix, already past the `[`: `[]` iterate, `[e]` index,
    /// `[a:b]` / `[a:]` / `[:b]` slice.
    fn bracket_suffix(&mut self, base: Filter) -> Result<Filter, String> {
        if self.eat_op("]") {
            return Ok(Filter::Iterate(Box::new(base)));
        }
        if self.eat_op(":") {
            let hi = self.pipe()?;
            self.want_op("]")?;
            return Ok(Filter::Slice(Box::new(base), None, Some(Box::new(hi))));
        }
        let first = self.pipe()?;
        if self.eat_op(":") {
            if self.eat_op("]") {
                return Ok(Filter::Slice(Box::new(base), Some(Box::new(first)), None));
            }
            let hi = self.pipe()?;
            self.want_op("]")?;
            return Ok(Filter::Slice(
                Box::new(base),
                Some(Box::new(first)),
                Some(Box::new(hi)),
            ));
        }
        self.want_op("]")?;
        Ok(Filter::Index(Box::new(base), Box::new(first)))
    }

    fn term(&mut self) -> Result<Filter, String> {
        match self.peek().cloned() {
            None => Err("jq: unexpected end of program".into()),
            Some(Tok::Op("..")) => {
                self.i += 1;
                Ok(Filter::RecurseDefault)
            }
            Some(Tok::Field(name)) => {
                self.i += 1;
                Ok(Filter::Field(
                    Box::new(Filter::Identity),
                    Rc::from(name.as_str()),
                ))
            }
            Some(Tok::Op(".")) => {
                self.i += 1;
                match self.peek().cloned() {
                    Some(Tok::Str(pieces)) => {
                        self.i += 1;
                        Ok(Filter::Index(
                            Box::new(Filter::Identity),
                            Box::new(Filter::Str(
                                pieces.into_iter().map(Into::into).collect(),
                                None,
                            )),
                        ))
                    }
                    Some(Tok::Op("[")) => {
                        self.i += 1;
                        self.bracket_suffix(Filter::Identity)
                    }
                    _ => Ok(Filter::Identity),
                }
            }
            Some(Tok::Num(n, text)) => {
                self.i += 1;
                Ok(Filter::Lit(num_from_literal(n, &text)))
            }
            Some(Tok::Str(pieces)) => {
                self.i += 1;
                Ok(Filter::Str(
                    pieces.into_iter().map(Into::into).collect(),
                    None,
                ))
            }
            Some(Tok::Format(name)) => {
                self.i += 1;
                // `@fmt "…"` applies the format to the string's interpolations;
                // `@fmt` alone is the format applied to `.`.
                if let Some(Tok::Str(pieces)) = self.peek().cloned() {
                    self.i += 1;
                    return Ok(Filter::Str(
                        pieces.into_iter().map(Into::into).collect(),
                        Some(Rc::from(name.as_str())),
                    ));
                }
                Ok(Filter::Format(Rc::from(name.as_str())))
            }
            Some(Tok::Var(name)) => {
                self.i += 1;
                Ok(Filter::Var(Rc::from(name.as_str())))
            }
            Some(Tok::Op("(")) => {
                self.i += 1;
                let inner = self.pipe()?;
                self.want_op(")")?;
                Ok(inner)
            }
            Some(Tok::Op("[")) => {
                self.i += 1;
                if self.eat_op("]") {
                    return Ok(Filter::Array(None));
                }
                let inner = self.pipe()?;
                self.want_op("]")?;
                Ok(Filter::Array(Some(Box::new(inner))))
            }
            Some(Tok::Op("{")) => {
                self.i += 1;
                self.object_cons()
            }
            Some(Tok::Ident(name)) => self.ident_term(&name),
            Some(t) => Err(format!(
                "jq: unexpected `{}`",
                match t {
                    Tok::Op(o) => o.to_string(),
                    other => format!("{other:?}"),
                }
            )),
        }
    }

    fn ident_term(&mut self, name: &str) -> Result<Filter, String> {
        match name {
            "if" => {
                self.i += 1;
                let mut arms = Vec::new();
                loop {
                    let cond = self.pipe()?;
                    self.want_kw("then")?;
                    let then = self.pipe()?;
                    arms.push((cond, then));
                    if self.eat_kw("elif") {
                        continue;
                    }
                    break;
                }
                let els = if self.eat_kw("else") {
                    let e = self.pipe()?;
                    Some(Box::new(e))
                } else {
                    None
                };
                self.want_kw("end")?;
                Ok(Filter::If(arms, els))
            }
            "try" => {
                self.i += 1;
                // parser.y: `"try" Expr "catch" Expr`, with `"try"`/`"catch"`
                // binding tighter than every binary operator, so each side
                // reduces to one Term — and `'-' Term` is a Term, so
                // `try -. catch .` negates inside the try.
                let body = self.unary()?;
                let handler = if self.eat_kw("catch") {
                    Some(Box::new(self.unary()?))
                } else {
                    None
                };
                Ok(Filter::Try(Box::new(body), handler))
            }
            "reduce" | "foreach" => {
                let is_reduce = name == "reduce";
                self.i += 1;
                // `"reduce" Expr "as" Patterns …`: the source runs up to `as`.
                let src = self.alt()?;
                self.want_kw("as")?;
                let pat = self.pattern()?;
                self.want_op("(")?;
                let init = self.pipe()?;
                self.want_op(";")?;
                let update = self.pipe()?;
                let extract = if self.eat_op(";") {
                    Some(Box::new(self.pipe()?))
                } else {
                    None
                };
                self.want_op(")")?;
                if is_reduce {
                    Ok(Filter::Reduce(
                        Box::new(src),
                        pat,
                        Box::new(init),
                        Box::new(update),
                    ))
                } else {
                    Ok(Filter::Foreach(
                        Box::new(src),
                        pat,
                        Box::new(init),
                        Box::new(update),
                        extract,
                    ))
                }
            }
            "label" => {
                self.i += 1;
                let lbl = self.var()?;
                self.want_op("|")?;
                let body = self.pipe()?;
                Ok(Filter::Label(lbl, Box::new(body)))
            }
            "break" => {
                self.i += 1;
                let lbl = self.var()?;
                Ok(Filter::Break(lbl))
            }
            "def" => {
                let def = self.funcdef()?;
                let rest = self.pipe()?;
                Ok(Filter::Def(Rc::new(def), Box::new(rest)))
            }
            "true" => {
                self.i += 1;
                Ok(Filter::Lit(JqVal::Bool(true)))
            }
            "false" => {
                self.i += 1;
                Ok(Filter::Lit(JqVal::Bool(false)))
            }
            "null" => {
                self.i += 1;
                Ok(Filter::Lit(JqVal::Null))
            }
            _ => {
                self.i += 1;
                let mut args = Vec::new();
                if self.eat_op("(") {
                    loop {
                        args.push(Rc::new(self.pipe()?));
                        if self.eat_op(";") {
                            continue;
                        }
                        self.want_op(")")?;
                        break;
                    }
                }
                Ok(Filter::Call(Rc::from(name), args))
            }
        }
    }

    /// `{ … }` construction. Entries are `k: v`, `"k": v`, `(e): v`, `$v`,
    /// `k` (shorthand for `k: .k`), `@fmt "…"`, and `$__loc__`.
    fn object_cons(&mut self) -> Result<Filter, String> {
        let mut entries = Vec::new();
        if self.eat_op("}") {
            return Ok(Filter::Object(entries));
        }
        loop {
            let (key, default): (Filter, Option<Filter>) = match self.peek().cloned() {
                Some(Tok::Ident(k)) => {
                    self.i += 1;
                    (
                        Filter::Lit(JqVal::str(k.clone())),
                        Some(Filter::Field(
                            Box::new(Filter::Identity),
                            Rc::from(k.as_str()),
                        )),
                    )
                }
                // parser.y: `BINDING ':' DictExpr` keys by the variable's
                // VALUE (LOADV); a bare `BINDING` keys by its name.
                Some(Tok::Var(v)) => {
                    self.i += 1;
                    let var = Filter::Var(Rc::from(v.as_str()));
                    if self.is_op(":") {
                        (var, None)
                    } else {
                        (Filter::Lit(JqVal::str(v.clone())), Some(var))
                    }
                }
                Some(Tok::Str(pieces)) => {
                    self.i += 1;
                    let key = Filter::Str(pieces.into_iter().map(Into::into).collect(), None);
                    (
                        key.clone(),
                        Some(Filter::Index(Box::new(Filter::Identity), Box::new(key))),
                    )
                }
                Some(Tok::Format(name)) => {
                    self.i += 1;
                    match self.peek().cloned() {
                        Some(Tok::Str(pieces)) => {
                            self.i += 1;
                            let key = Filter::Str(
                                pieces.into_iter().map(Into::into).collect(),
                                Some(Rc::from(name.as_str())),
                            );
                            (key.clone(), None)
                        }
                        _ => return Err("jq: `@fmt` in an object key needs a string".into()),
                    }
                }
                Some(Tok::Op("(")) => {
                    self.i += 1;
                    let k = self.pipe()?;
                    self.want_op(")")?;
                    (k, None)
                }
                _ => return Err(format!("jq: bad object key `{}`", self.describe(self.i))),
            };
            let val = if self.eat_op(":") {
                // An object VALUE binds tighter than `,` (which separates
                // entries) but may still be a `|` pipeline via parens. jq uses
                // `ExpD`, an alternation of `|`-joined non-comma terms.
                self.obj_val()?
            } else {
                default.ok_or_else(|| "jq: object entry needs a `: value`".to_string())?
            };
            entries.push(ObjEntry::KeyVal(key, val));
            // `DictPair ',' DictPairs`, where DictPairs may be empty: a trailing
            // comma is legal (`{a: 1,}`), a leading or doubled one is not.
            if self.eat_op(",") {
                if self.eat_op("}") {
                    return Ok(Filter::Object(entries));
                }
                continue;
            }
            self.want_op("}")?;
            return Ok(Filter::Object(entries));
        }
    }

    /// An object entry's value: `|`-joined, but never `,`-joined — the comma at
    /// this level separates entries.
    fn obj_val(&mut self) -> Result<Filter, String> {
        let mut lhs = self.alt()?;
        while self.eat_op("|") {
            let rhs = self.alt()?;
            lhs = Filter::Pipe(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Environment
// ─────────────────────────────────────────────────────────────────────────────

struct VarNode {
    name: Rc<str>,
    val: JqVal,
    next: Option<Rc<VarNode>>,
}

/// A function binding. The node also carries the environment it was DEFINED in,
/// which is what makes closures work; a user function's body additionally sees
/// the node itself, so recursion needs no fixed point.
struct FuncNode {
    name: Rc<str>,
    arity: usize,
    kind: FnKind,
    next: Option<Rc<FuncNode>>,
    vars: Option<Rc<VarNode>>,
}

enum FnKind {
    User(Rc<FuncDef>),
    /// A closure passed as a function argument: the caller's filter, evaluated
    /// in the caller's environment.
    Arg(Rc<Filter>, Env),
}

#[derive(Clone, Default)]
struct Env {
    vars: Option<Rc<VarNode>>,
    funcs: Option<Rc<FuncNode>>,
}

impl Env {
    fn bind(&self, name: Rc<str>, val: JqVal) -> Env {
        Env {
            vars: Some(Rc::new(VarNode {
                name,
                val,
                next: self.vars.clone(),
            })),
            funcs: self.funcs.clone(),
        }
    }
    fn lookup(&self, name: &str) -> Option<&JqVal> {
        let mut cur = self.vars.as_ref();
        while let Some(n) = cur {
            if &*n.name == name {
                return Some(&n.val);
            }
            cur = n.next.as_ref();
        }
        None
    }
    fn define(&self, def: Rc<FuncDef>) -> Env {
        Env {
            vars: self.vars.clone(),
            funcs: Some(Rc::new(FuncNode {
                name: def.name.clone(),
                arity: def.params.len(),
                kind: FnKind::User(def),
                next: self.funcs.clone(),
                vars: self.vars.clone(),
            })),
        }
    }
    fn find_fn(&self, name: &str, arity: usize) -> Option<Rc<FuncNode>> {
        let mut cur = self.funcs.clone();
        while let Some(n) = cur {
            if &*n.name == name && n.arity == arity {
                return Some(n);
            }
            cur = n.next.clone();
        }
        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Interpreter
// ─────────────────────────────────────────────────────────────────────────────

/// A jq program compiled once and runnable over many inputs.
pub struct Program {
    filter: Filter,
    base: Env,
}

/// Per-run state: the `input`/`inputs` queue and the label counter.
pub struct Interp {
    labels: std::cell::Cell<u64>,
    /// The documents `.`, `input` and `inputs` all draw from, held as RAW TEXT
    /// and parsed on the way out. jq's model is one shared cursor over the input
    /// stream: the next document is `.`, and `input` takes the one after it, so
    /// a document consumed by `input` is not seen again by the outer loop.
    ///
    /// Raw rather than pre-parsed because most programs never call `input`, and
    /// parsing a whole stream up front to serve a builtin nobody used is the same
    /// cost as running the query twice.
    inputs: RefCell<std::collections::VecDeque<String>>,
    env_obj: RefCell<Option<JqVal>>,
    /// The document currently being evaluated, which is what `parent` walks. Set
    /// per input by the pipeline; `None` when nothing set it, and `parent` then
    /// answers `null` rather than guessing.
    doc: RefCell<Option<JqVal>>,
    /// What `input_line_number` reports: how many input lines have been taken,
    /// which is what jq reports while reading a line-per-document stream.
    line: std::cell::Cell<f64>,
}

impl Default for Interp {
    fn default() -> Self {
        Interp {
            labels: std::cell::Cell::new(0),
            inputs: RefCell::new(std::collections::VecDeque::new()),
            env_obj: RefCell::new(None),
            doc: RefCell::new(None),
            line: std::cell::Cell::new(0.0),
        }
    }
}

impl Interp {
    /// Seed the document queue from the raw input lines.
    pub fn set_input_lines(&self, lines: Vec<String>) {
        *self.inputs.borrow_mut() = lines.into();
    }

    /// Take the next document, or `None` at end of stream. A line that is not
    /// JSON is jq's STRING — the reading SPEC §8 gives a text line.
    ///
    /// Taking a line advances `input_line_number`, whoever takes it: jq's count
    /// is the parser's position, so `input` moves it too (`[., input,
    /// input_line_number]` over `1`, `2` is `[1,2,2]`).
    pub fn next_input(&self) -> Option<JqVal> {
        let line = self.inputs.borrow_mut().pop_front()?;
        self.line.set(self.line.get() + 1.0);
        Some(parse_json(&line).unwrap_or_else(|_| JqVal::str(line.as_str())))
    }
    /// Set what `input_line_number` reports for the value about to be run, or
    /// (before the first `next_input`) how many lines precede this batch.
    pub fn set_line(&self, n: usize) {
        self.line.set(n as f64);
    }
    /// Record the document about to be evaluated, so `parent` can walk it.
    pub fn set_doc(&self, v: &JqVal) {
        *self.doc.borrow_mut() = Some(v.clone());
    }
    fn current_doc(&self) -> Option<JqVal> {
        self.doc.borrow().clone()
    }
    fn env_object(&self) -> JqVal {
        if let Some(v) = self.env_obj.borrow().as_ref() {
            return v.clone();
        }
        let v = JqVal::obj(
            std::env::vars()
                .map(|(k, val)| (Rc::from(k.as_str()), JqVal::str(val)))
                .collect(),
        );
        *self.env_obj.borrow_mut() = Some(v.clone());
        v
    }
}

/// An error raised by the DOWNSTREAM sink rather than by the filter being
/// evaluated. `try`/`?`/`//` must not swallow it — `[.[] | try error] ` catches
/// its own error, but a failure while writing the result is not the filter's.
/// Wrapping happens exactly at the boundaries that catch, so it never escapes.
fn wrap_downstream(e: JqErr) -> JqErr {
    match e {
        JqErr::Err(v) => JqErr::Err(JqVal::arr(vec![JqVal::str(DOWNSTREAM_TAG), v])),
        other => other,
    }
}

const DOWNSTREAM_TAG: &str = "\u{1}jqlang-downstream";

/// Undo [`wrap_downstream`], or `None` when the error was raised by the filter.
fn unwrap_downstream(e: JqErr) -> Result<JqErr, JqErr> {
    if let JqErr::Err(JqVal::Arr(a)) = &e {
        if a.len() == 2 {
            if let JqVal::Str(tag) = &a[0] {
                if &**tag == DOWNSTREAM_TAG {
                    return Ok(JqErr::Err(a[1].clone()));
                }
            }
        }
    }
    Err(e)
}

impl Program {
    /// Compile a jq program. The prelude (jq's own `builtin.jq` definitions) is
    /// parsed once per process and shared.
    pub fn compile(src: &str) -> Result<Program, String> {
        let filter = parse(src)?;
        let base = prelude_env();
        // jq resolves names at COMPILE time — `jq 'bogus'` is a compile error
        // (exit 3), not a runtime one. Checking here keeps that, and it is what
        // lets arb still report `unknown verb` for a typo'd arb verb instead of
        // silently accepting it as a jq program that fails later.
        let mut funcs: std::collections::HashSet<String> = builtin_names().into_iter().collect();
        funcs.extend(NATIVE_ONLY.iter().map(|s| (*s).to_string()));
        let vars: std::collections::HashSet<String> = ["ENV", "__loc__"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        check_names(&filter, &funcs, &vars)?;
        Ok(Program { filter, base })
    }

    /// Does this program read from the input STREAM (`input` / `inputs`)?
    ///
    /// A program that does not is per-line, and arb's pipeline can stream it —
    /// emitting as lines arrive instead of buffering to EOF. One that does needs
    /// the whole stream in hand by construction. Conservative in the safe
    /// direction: a user `def input:` that shadows the builtin still reports
    /// `true`, which costs streaming and never correctness.
    pub fn reads_input_stream(&self) -> bool {
        fn walk(f: &Filter) -> bool {
            let mut hit = false;
            for_each_child(f, &mut |c| hit |= walk(c));
            if let Filter::Call(name, args) = f {
                if args.is_empty() && matches!(&**name, "input" | "inputs") {
                    return true;
                }
            }
            hit
        }
        walk(&self.filter)
    }

    /// Run the program over one input, collecting every output value.
    pub fn run(&self, interp: &Interp, input: &JqVal) -> R<Vec<JqVal>> {
        let mut out = Vec::new();
        self.run_with(interp, input, &mut |v| {
            out.push(v);
            Ok(())
        })?;
        Ok(out)
    }

    /// Run the program over one input, streaming each output to `sink`.
    pub fn run_with(&self, interp: &Interp, input: &JqVal, sink: Sink) -> R<()> {
        eval(interp, &self.filter, input, &self.base, sink)
    }
}

/// Evaluate `f` against `input`, sending every output value to `out`.
fn eval(it: &Interp, f: &Filter, input: &JqVal, env: &Env, out: Sink) -> R<()> {
    match f {
        Filter::Identity => out(input.clone()),
        Filter::RecurseDefault => recurse_all(input, out),
        Filter::Lit(v) => out(v.clone()),
        Filter::Str(pieces, fmt) => eval_string(it, pieces, fmt.as_deref(), input, env, out),
        Filter::Format(name) => out(JqVal::str(apply_format(name, input)?)),
        Filter::Field(..) | Filter::Index(..) | Filter::Slice(..) | Filter::Iterate(..) => {
            eval_index(it, f, false, input, env, out)
        }
        Filter::IndexOpt(inner) => eval_index(it, inner, true, input, env, out),
        Filter::Optional(inner) => {
            match eval(it, inner, input, env, &mut |v| {
                out(v).map_err(wrap_downstream)
            }) {
                Ok(()) => Ok(()),
                Err(e) => match unwrap_downstream(e) {
                    Ok(real) => Err(real),
                    Err(JqErr::Err(_)) => Ok(()),
                    Err(other) => Err(other),
                },
            }
        }
        Filter::Pipe(a, b) => eval(it, a, input, env, &mut |v| eval(it, b, &v, env, out)),
        Filter::Comma(a, b) => {
            eval(it, a, input, env, out)?;
            eval(it, b, input, env, out)
        }
        Filter::Neg(inner) => eval(it, inner, input, env, &mut |v| match v.bare() {
            JqVal::Num(n, lit) => out(negate_num(*n, lit.as_deref())),
            other => Err(JqErr::msg(format!(
                "{}{} cannot be negated",
                other.type_name(),
                paren_of(other)
            ))),
        }),
        // jq evaluates a binary operator's RIGHT side in the outer loop: for
        // `(1,2) as the left and (10,20) as the right`, the emitted order is
        // 11,12,21,22 — the right value varies slowest.
        Filter::Bin(op, a, b) => eval(it, b, input, env, &mut |rv| {
            eval(it, a, input, env, &mut |lv| out(binop(*op, &lv, &rv)?))
        }),
        Filter::And(a, b) => eval(it, a, input, env, &mut |lv| {
            if !lv.truthy() {
                return out(JqVal::Bool(false));
            }
            eval(it, b, input, env, &mut |rv| out(JqVal::Bool(rv.truthy())))
        }),
        Filter::Or(a, b) => eval(it, a, input, env, &mut |lv| {
            if lv.truthy() {
                return out(JqVal::Bool(true));
            }
            eval(it, b, input, env, &mut |rv| out(JqVal::Bool(rv.truthy())))
        }),
        // `a // b`: every TRUTHY output of `a`; only if there were none does
        // `b` run. An error raised by `a` PROPAGATES — jq 1.8 does not suppress
        // it (`1 | .a // 3` is "Cannot index number", not `3`); `(a)? // b` is
        // how a program asks for suppression.
        Filter::Alt(a, b) => {
            let mut any = false;
            eval(it, a, input, env, &mut |v| {
                if v.truthy() {
                    any = true;
                    out(v)
                } else {
                    Ok(())
                }
            })?;
            if any {
                Ok(())
            } else {
                eval(it, b, input, env, out)
            }
        }
        Filter::If(arms, els) => eval_if(it, arms, els.as_deref(), 0, input, env, out),
        Filter::Try(body, handler) => {
            match eval(it, body, input, env, &mut |v| {
                out(v).map_err(wrap_downstream)
            }) {
                Ok(()) => Ok(()),
                Err(e) => match unwrap_downstream(e) {
                    Ok(real) => Err(real),
                    Err(JqErr::Err(payload)) => match handler {
                        Some(h) => eval(it, h, &payload, env, out),
                        None => Ok(()),
                    },
                    Err(other) => Err(other),
                },
            }
        }
        Filter::Reduce(src, pat, init, update) => eval(it, init, input, env, &mut |init_v| {
            let mut acc = init_v;
            eval(it, src, input, env, &mut |item| {
                bind_pattern(it, pat, &item, env, &mut |benv| {
                    let mut last = None;
                    eval(it, update, &acc, &benv, &mut |v| {
                        last = Some(v);
                        Ok(())
                    })?;
                    // An update that yields nothing collapses the accumulator to
                    // null, which is what jq 1.7+ does (`reduce (1) as $x (0;
                    // empty)` is `null`).
                    acc = last.unwrap_or(JqVal::Null);
                    Ok(())
                })
            })?;
            out(acc.clone())
        }),
        Filter::Foreach(src, pat, init, update, extract) => {
            eval(it, init, input, env, &mut |init_v| {
                let mut acc = init_v;
                eval(it, src, input, env, &mut |item| {
                    bind_pattern(it, pat, &item, env, &mut |benv| {
                        let mut states = Vec::new();
                        eval(it, update, &acc, &benv, &mut |v| {
                            states.push(v);
                            Ok(())
                        })?;
                        for st in states {
                            acc = st.clone();
                            match extract {
                                Some(e) => eval(it, e, &st, &benv, out)?,
                                None => out(st)?,
                            }
                        }
                        Ok(())
                    })
                })
            })
        }
        Filter::Bind(src, pats, body) => eval(it, src, input, env, &mut |v| {
            bind_alternatives(it, pats, &v, env, input, body, out)
        }),
        Filter::Label(name, body) => {
            let id = it.labels.get() + 1;
            it.labels.set(id);
            let benv = env.bind(label_key(name), JqVal::num(id as f64));
            match eval(it, body, input, &benv, &mut |v| {
                out(v).map_err(wrap_downstream)
            }) {
                Err(JqErr::Break(b)) if b == id => Ok(()),
                Err(e) => match unwrap_downstream(e) {
                    Ok(real) => Err(real),
                    Err(other) => Err(other),
                },
                Ok(()) => Ok(()),
            }
        }
        Filter::Break(name) => match env.lookup(&label_key(name)) {
            Some(JqVal::Num(id, _)) => Err(JqErr::Break(*id as u64)),
            _ => Err(JqErr::msg(format!("$*label-{name} is not defined"))),
        },
        Filter::Var(name) => match &**name {
            "ENV" => out(it.env_object()),
            "__loc__" => out(JqVal::obj(vec![
                (Rc::from("file"), JqVal::str("<top-level>")),
                (Rc::from("line"), JqVal::num(1.0)),
            ])),
            _ => match env.lookup(name) {
                Some(v) => out(v.clone()),
                None => Err(JqErr::msg(format!("${name} is not defined"))),
            },
        },
        Filter::Def(def, rest) => {
            let inner = env.define(def.clone());
            eval(it, rest, input, &inner, out)
        }
        Filter::Call(name, args) => eval_call(it, name, args, input, env, out),
        Filter::Object(entries) => build_object(it, entries, 0, Vec::new(), input, env, out),
        Filter::Array(inner) => {
            let mut items = Vec::new();
            if let Some(f) = inner {
                eval(it, f, input, env, &mut |v| {
                    items.push(v);
                    Ok(())
                })?;
            }
            out(JqVal::arr(items))
        }
        Filter::Assign(op, lhs, rhs) => eval_assign(it, *op, lhs, rhs, input, env, out),
    }
}

/// One index step — `.k`, `.[e]`, `.[a:b]` or `.[]` — over every output of its
/// base. Under `opt` (parser.y's INDEX_OPT/EACH_OPT, the `?` written right
/// after the step) the step's OWN error yields nothing; an error from the
/// base, from the key, or from downstream still propagates.
fn eval_index(it: &Interp, f: &Filter, opt: bool, input: &JqVal, env: &Env, out: Sink) -> R<()> {
    let step = |r: R<JqVal>, out: Sink| match r {
        Ok(v) => out(v),
        Err(_) if opt => Ok(()),
        Err(e) => Err(e),
    };
    match f {
        Filter::Field(base, name) => eval(it, base, input, env, &mut |v| {
            step(index_value(&v, &JqVal::Str(name.clone())), out)
        }),
        Filter::Index(base, idx) => eval(it, base, input, env, &mut |v| {
            eval(it, idx, input, env, &mut |i| step(index_value(&v, &i), out))
        }),
        Filter::Slice(base, lo, hi) => eval(it, base, input, env, &mut |v| {
            eval_opt(it, lo.as_deref(), input, env, &mut |lo_v| {
                eval_opt(it, hi.as_deref(), input, env, &mut |hi_v| {
                    step(slice_value(&v, &lo_v, &hi_v), out)
                })
            })
        }),
        Filter::Iterate(base) => eval(it, base, input, env, &mut |v| match v.bare() {
            JqVal::Arr(a) => {
                for e in a.iter() {
                    out(e.clone())?;
                }
                Ok(())
            }
            JqVal::Obj(m) => {
                for (_, val) in m.iter() {
                    out(val.clone())?;
                }
                Ok(())
            }
            _ if opt => Ok(()),
            other => Err(JqErr::msg(format!(
                "Cannot iterate over {}{}",
                other.type_name(),
                paren_of(other)
            ))),
        }),
        other => unreachable!("not an index step: {other:?}"),
    }
}

/// The `(value)` suffix jq appends to a type in an error message.
pub(crate) fn paren_of(v: &JqVal) -> String {
    format!(" ({})", dump_trunc(v))
}

/// A port of `jv_dump_string_trunc` (src/jv_print.c) with the 30-byte buffer
/// every error message uses. A dump of 30 BYTES or more keeps its first 25
/// (26 when it does not open with `"`, `[` or `{`), backed up to the start of
/// the UTF-8 character that byte falls in, then `...` and the closing delimiter.
pub(crate) fn dump_trunc(v: &JqVal) -> String {
    const BUFSIZE: usize = 30;
    let s = render(v.bare());
    if s.len() <= BUFSIZE - 1 {
        return s;
    }
    let delim = match s.as_bytes()[0] {
        b'"' => Some('"'),
        b'[' => Some(']'),
        b'{' => Some('}'),
        _ => None,
    };
    let mut l = BUFSIZE - if delim.is_some() { 5 } else { 4 };
    while !s.is_char_boundary(l) {
        l -= 1;
    }
    let mut out = format!("{}...", &s[..l]);
    out.extend(delim);
    out
}

fn label_key(name: &str) -> Rc<str> {
    Rc::from(format!("*label*{name}").as_str())
}

fn eval_opt(it: &Interp, f: Option<&Filter>, input: &JqVal, env: &Env, out: Sink) -> R<()> {
    match f {
        Some(f) => eval(it, f, input, env, out),
        None => out(JqVal::Null),
    }
}

fn eval_if(
    it: &Interp,
    arms: &[(Filter, Filter)],
    els: Option<&Filter>,
    i: usize,
    input: &JqVal,
    env: &Env,
    out: Sink,
) -> R<()> {
    let Some((cond, then)) = arms.get(i) else {
        // No arm matched: an absent `else` is the identity in jq 1.7+.
        return match els {
            Some(e) => eval(it, e, input, env, out),
            None => out(input.clone()),
        };
    };
    eval(it, cond, input, env, &mut |c| {
        if c.truthy() {
            eval(it, then, input, env, out)
        } else {
            eval_if(it, arms, els, i + 1, input, env, out)
        }
    })
}

/// jq's `..`: the value itself, then every descendant, depth first.
fn recurse_all(v: &JqVal, out: Sink) -> R<()> {
    // The NODE is what comes out — `.. | anchor` has to see the box — while the
    // traversal walks the value inside it.
    out(v.clone())?;
    match v.bare() {
        JqVal::Arr(a) => {
            for e in a.iter() {
                recurse_all(e, out)?;
            }
            Ok(())
        }
        JqVal::Obj(m) => {
            for (_, val) in m.iter() {
                recurse_all(val, out)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Value operations
// ─────────────────────────────────────────────────────────────────────────────

/// `v[idx]`. jq's rules: `null` indexes to `null` for both a key and an index, a
/// missing key or an out-of-range index is `null`, a negative array index counts
/// from the end, and every other type pairing is an error.
fn index_value(v: &JqVal, idx: &JqVal) -> R<JqVal> {
    // The CHILD keeps its box (that is how `.a | line_comment` reaches the
    // comment on `a`'s value); the container and the index are read unboxed.
    let (v, idx) = (v.bare(), idx.bare());
    match (v, idx) {
        (JqVal::Null, JqVal::Str(_) | JqVal::Num(..) | JqVal::Null) => Ok(JqVal::Null),
        (JqVal::Obj(_), JqVal::Str(k)) => Ok(v.obj_get(k).cloned().unwrap_or(JqVal::Null)),
        (JqVal::Arr(a), JqVal::Num(n, _)) => {
            if !n.is_finite() {
                return Ok(JqVal::Null);
            }
            let i = n.trunc();
            let i = if i < 0.0 { i + a.len() as f64 } else { i };
            if i < 0.0 || i >= a.len() as f64 {
                Ok(JqVal::Null)
            } else {
                Ok(a[i as usize].clone())
            }
        }
        // `.[ {"start":s,"end":e} ]` is how jq spells a slice internally, and it
        // reaches `index` when the object form is written out. `jv_get` hands
        // ANY object key on an array or string to `parse_slice`, which reads only
        // `start` and `end` (extra keys are ignored, a missing one refuses), and
        // indexes `null` with an object to `null`.
        (JqVal::Null, JqVal::Obj(_)) => Ok(JqVal::Null),
        (JqVal::Arr(_) | JqVal::Str(_), JqVal::Obj(_)) => {
            let (s, e) = (idx.obj_get("start"), idx.obj_get("end"));
            match v {
                JqVal::Arr(a) => {
                    let (s, e) = slice_bounds(s, e, a.len())?;
                    Ok(JqVal::arr(a[s..e].to_vec()))
                }
                JqVal::Str(st) => {
                    let cs: Vec<char> = st.chars().collect();
                    let (s, e) = slice_bounds(s, e, cs.len())?;
                    Ok(JqVal::str(cs[s..e].iter().collect::<String>()))
                }
                _ => unreachable!("matched an array or a string above"),
            }
        }
        // `[a] | .[ [x] ]` is jq's "indices of the subarray" form.
        (JqVal::Arr(_), JqVal::Arr(sub)) => Ok(JqVal::arr(array_indices(v, sub))),
        _ => Err(index_err(v, idx)),
    }
}

fn index_err(v: &JqVal, idx: &JqVal) -> JqErr {
    JqErr::msg(format!(
        "Cannot index {} with {}",
        v.type_name(),
        match idx.bare() {
            JqVal::Str(s) => format!("string ({})", render(&JqVal::Str(s.clone()))),
            other => format!("{}{}", other.type_name(), paren_of(other)),
        }
    ))
}

/// Every start offset at which `sub` occurs inside array `hay`.
fn array_indices(hay: &JqVal, sub: &[JqVal]) -> Vec<JqVal> {
    let JqVal::Arr(a) = hay.bare() else {
        return Vec::new();
    };
    if sub.is_empty() || sub.len() > a.len() {
        return Vec::new();
    }
    (0..=a.len() - sub.len())
        .filter(|&i| {
            a[i..i + sub.len()]
                .iter()
                .zip(sub)
                .all(|(x, y)| eq_vals(x, y))
        })
        .map(|i| JqVal::num(i as f64))
        .collect()
}

/// The `[start, end)` a slice `.[lo:hi]` of `len` items covers, as jq 1.8's
/// `parse_slice` computes it: a negative bound counts from the end, both are
/// clamped to `0..=len`, a fractional start is truncated and a fractional end
/// rounded UP (`[1,2,3,4,5] | .[1.2:3.5]` is `[2,3,4]`), and an end before the
/// start is the start. A `null` bound is open;
/// any other non-number is jq's "slice indices must be integers" error. Reading, assigning
/// and deleting a slice all go through here.
fn slice_bounds(lo: Option<&JqVal>, hi: Option<&JqVal>, len: usize) -> R<(usize, usize)> {
    // A port of `parse_slice` (src/jv_aux.c). A null bound is open; a MISSING
    // one (jq's INVALID) and any non-number refuse.
    let bound = |b: Option<&JqVal>, open: f64| match b.map(JqVal::bare) {
        Some(JqVal::Num(n, _)) => Ok(*n),
        Some(JqVal::Null) => Ok(open),
        _ => Err(JqErr::msg("Array/string slice indices must be integers")),
    };
    let lenf = len as f64;
    let (mut ds, mut de) = (bound(lo, 0.0)?, bound(hi, lenf)?);
    // The start rounds DOWN and a NaN start is 0.
    if ds.is_nan() {
        ds = 0.0;
    }
    if ds < 0.0 {
        ds += lenf;
    }
    let start = ds.clamp(0.0, lenf) as usize;
    // The end rounds UP and a NaN end is the length.
    if de.is_nan() {
        de = lenf;
    }
    if de < 0.0 {
        de += lenf;
    }
    if de < 0.0 {
        de = start as f64;
    }
    let mut end = (de.min(lenf)) as usize;
    if end < len && (end as f64) < de {
        end += 1;
    }
    Ok((start, end.max(start)))
}

/// jq's slice: clamped, negative-from-the-end, over arrays and strings, with
/// `null` slicing to `null`.
fn slice_value(v: &JqVal, lo: &JqVal, hi: &JqVal) -> R<JqVal> {
    let bounds = |len: usize| slice_bounds(Some(lo), Some(hi), len);
    match v.bare() {
        JqVal::Null => Ok(JqVal::Null),
        JqVal::Arr(a) => {
            let (s, e) = bounds(a.len())?;
            Ok(JqVal::arr(a[s..e].to_vec()))
        }
        JqVal::Str(s) => {
            // jq slices a string by CODE POINT, not by byte.
            let cs: Vec<char> = s.chars().collect();
            let (a, b) = bounds(cs.len())?;
            Ok(JqVal::str(cs[a..b].iter().collect::<String>()))
        }
        other => Err(JqErr::msg(format!(
            "Cannot index {} with object ({{\"start\":{},\"end\":{}}})",
            other.type_name(),
            render(lo),
            render(hi)
        ))),
    }
}

fn binop(op: BinOp, a: &JqVal, b: &JqVal) -> R<JqVal> {
    // Arithmetic and comparison are about VALUES. `1 # a` + `2 # b` is 3, and
    // the result carries no comment because it is a new value, not either node —
    // which is also what yq answers.
    let (a, b) = (a.bare(), b.bare());
    match op {
        BinOp::Eq => return Ok(JqVal::Bool(eq_vals(a, b))),
        BinOp::Ne => return Ok(JqVal::Bool(!eq_vals(a, b))),
        BinOp::Lt => return Ok(JqVal::Bool(cmp_vals(a, b) == Ordering::Less)),
        BinOp::Le => return Ok(JqVal::Bool(cmp_vals(a, b) != Ordering::Greater)),
        BinOp::Gt => return Ok(JqVal::Bool(cmp_vals(a, b) == Ordering::Greater)),
        BinOp::Ge => return Ok(JqVal::Bool(cmp_vals(a, b) != Ordering::Less)),
        _ => {}
    }
    let bad = |verb: &str| {
        JqErr::msg(format!(
            "{}{} and {}{} cannot be {verb}",
            a.type_name(),
            paren_of(a),
            b.type_name(),
            paren_of(b)
        ))
    };
    match op {
        BinOp::Add => match (a, b) {
            // `null` is the identity of `+` on either side, which is what makes
            // `add` == `reduce .[] as $x (null; . + $x)` work on any element type.
            (JqVal::Null, x) | (x, JqVal::Null) => Ok(x.clone()),
            (JqVal::Num(x, _), JqVal::Num(y, _)) => Ok(JqVal::num(x + y)),
            (JqVal::Str(x), JqVal::Str(y)) => Ok(JqVal::str(format!("{x}{y}"))),
            (JqVal::Arr(x), JqVal::Arr(y)) => {
                let mut v = x.as_ref().clone();
                v.extend(y.iter().cloned());
                Ok(JqVal::arr(v))
            }
            (JqVal::Obj(x), JqVal::Obj(y)) => {
                let mut v = x.as_ref().clone();
                for (k, val) in y.iter() {
                    match v.iter_mut().find(|(ek, _)| ek == k) {
                        Some(slot) => slot.1 = val.clone(),
                        None => v.push((k.clone(), val.clone())),
                    }
                }
                Ok(JqVal::Obj(Rc::new(v)))
            }
            _ => Err(bad("added")),
        },
        BinOp::Sub => match (a, b) {
            (JqVal::Num(x, _), JqVal::Num(y, _)) => Ok(JqVal::num(x - y)),
            (JqVal::Arr(x), JqVal::Arr(y)) => Ok(JqVal::arr(
                x.iter()
                    .filter(|e| !y.iter().any(|d| eq_vals(e, d)))
                    .cloned()
                    .collect(),
            )),
            _ => Err(bad("subtracted")),
        },
        BinOp::Mul => match (a, b) {
            (JqVal::Num(x, _), JqVal::Num(y, _)) => Ok(JqVal::num(x * y)),
            (JqVal::Str(s), JqVal::Num(n, _)) | (JqVal::Num(n, _), JqVal::Str(s)) => {
                // jq 1.8.2: a negative or NaN count is `null`, otherwise the
                // count is truncated (`"ab" * 0.5` is `""`), and a result past
                // `INT_MAX` bytes is refused rather than allocated.
                if n.is_nan() || *n < 0.0 {
                    return Ok(JqVal::Null);
                }
                let times = *n as usize;
                if s.len().saturating_mul(times) > i32::MAX as usize {
                    return Err(JqErr::msg("Repeat string result too long"));
                }
                Ok(JqVal::str(s.repeat(times)))
            }
            (JqVal::Obj(_), JqVal::Obj(_)) => Ok(deep_merge(a, b)),
            _ => Err(bad("multiplied")),
        },
        BinOp::Div => match (a, b) {
            (JqVal::Num(_, _), JqVal::Num(y, _)) if *y == 0.0 => Err(JqErr::msg(format!(
                "{}{} and {}{} cannot be divided because the divisor is zero",
                a.type_name(),
                paren_of(a),
                b.type_name(),
                paren_of(b)
            ))),
            (JqVal::Num(x, _), JqVal::Num(y, _)) => Ok(JqVal::num(x / y)),
            (JqVal::Str(x), JqVal::Str(y)) => Ok(JqVal::arr(split_str(x, y))),
            _ => Err(bad("divided")),
        },
        BinOp::Mod => match (a, b) {
            // `binop_mod` (src/builtin.c) answers NaN when either side is NaN,
            // before the zero-divisor check that a NaN would otherwise trip.
            (JqVal::Num(x, _), JqVal::Num(y, _)) if x.is_nan() || y.is_nan() => {
                Ok(JqVal::num(f64::NAN))
            }
            (JqVal::Num(x, _), JqVal::Num(y, _)) => {
                // jq truncates BOTH operands to integers first, so `5.9 % 3` is
                // `2` and not the f64 remainder.
                let (xi, yi) = (trunc_i64(*x), trunc_i64(*y));
                if yi == 0 {
                    return Err(JqErr::msg(format!(
                        "{}{} and {}{} cannot be divided (remainder) because the divisor is zero",
                        a.type_name(),
                        paren_of(a),
                        b.type_name(),
                        paren_of(b)
                    )));
                }
                // jq special-cases a `-1` divisor to 0 so `INTMAX_MIN % -1` cannot
                // overflow; Rust's `%` panics there.
                Ok(JqVal::num(if yi == -1 { 0.0 } else { (xi % yi) as f64 }))
            }
            _ => Err(bad("divided (remainder)")),
        },
        _ => unreachable!("comparisons returned above"),
    }
}

/// jq casts through `intmax_t` for `%`; a non-finite double has no such cast, so
/// it saturates rather than being undefined.
fn trunc_i64(v: f64) -> i64 {
    if v.is_nan() {
        0
    } else {
        v.trunc().clamp(i64::MIN as f64, i64::MAX as f64) as i64
    }
}

/// `*` on two objects: recursive merge, with the RIGHT side winning at leaves.
fn deep_merge(a: &JqVal, b: &JqVal) -> JqVal {
    match (a.bare(), b.bare()) {
        (JqVal::Obj(x), JqVal::Obj(y)) => {
            let mut v = x.as_ref().clone();
            for (k, bv) in y.iter() {
                match v.iter_mut().find(|(ek, _)| ek == k) {
                    Some(slot) => slot.1 = deep_merge(&slot.1.clone(), bv),
                    None => v.push((k.clone(), bv.clone())),
                }
            }
            JqVal::Obj(Rc::new(v))
        }
        _ => b.clone(),
    }
}

/// `"a,b" / ","`. An EMPTY separator splits nothing (jq returns the whole
/// string as one element), matching `jv_string_split`.
fn split_str(s: &str, sep: &str) -> Vec<JqVal> {
    if s.is_empty() {
        return Vec::new();
    }
    // An empty separator splits between every character, as jq 1.8 does
    // (`"ab" | split("")` is `["a","b"]`, and so is `"ab" / ""`).
    if sep.is_empty() {
        return s.chars().map(|c| JqVal::str(c.to_string())).collect();
    }
    s.split(sep).map(JqVal::str).collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Strings, formats and object construction
// ─────────────────────────────────────────────────────────────────────────────

/// Build an interpolated string. Each `\(…)` is a GENERATOR, so `"\(1,2)"`
/// yields two strings. jq compiles the pieces as a chain of `+` whose RIGHT
/// operand is evaluated outermost, so the RIGHTMOST interpolation varies
/// slowest: `"\(1,2)-\(3,4)"` is `1-3`, `2-3`, `1-4`, `2-4`. The pieces are
/// therefore walked right to left, each prepended to the suffix built so far.
/// Everything a string build carries unchanged from piece to piece.
struct StrBuild<'a> {
    it: &'a Interp,
    pieces: &'a [StrPiece],
    fmt: Option<&'a str>,
    input: &'a JqVal,
    env: &'a Env,
}

fn eval_string(
    it: &Interp,
    pieces: &[StrPiece],
    fmt: Option<&str>,
    input: &JqVal,
    env: &Env,
    out: Sink,
) -> R<()> {
    fn go(b: &StrBuild, i: usize, acc: &str, out: Sink) -> R<()> {
        let Some(i) = i.checked_sub(1) else {
            return out(JqVal::str(acc));
        };
        match &b.pieces[i] {
            StrPiece::Lit(s) => {
                let next = format!("{s}{acc}");
                go(b, i, &next, out)
            }
            StrPiece::Interp(src) => {
                // The interpolation's source is parsed here rather than at lex
                // time so the lexer stays flat; the result is cached per program
                // run by the `Filter` the caller already holds.
                let f = parse(src).map_err(JqErr::msg)?;
                eval(b.it, &f, b.input, b.env, &mut |v| {
                    let piece = match b.fmt {
                        Some(name) => apply_format(name, &v)?,
                        None => render_raw(&v),
                    };
                    let next = format!("{piece}{acc}");
                    go(b, i, &next, out)
                })
            }
        }
    }
    go(
        &StrBuild {
            it,
            pieces,
            fmt,
            input,
            env,
        },
        pieces.len(),
        "",
        out,
    )
}

/// Every jq `@format` name. A leading `@` is otherwise an XPATH attribute
/// step, so only these names are claimed for jq — `@href` still selects an
/// attribute. The lexer, the body dispatcher and the xpath front-end all read
/// this one list, so a format added here is claimed everywhere at once.
pub const FORMAT_NAMES: &[&str] = &[
    "base64", "base64d", "csv", "tsv", "json", "text", "html", "uri", "urid", "sh",
];

/// jq's `@name` format strings.
fn apply_format(name: &str, v: &JqVal) -> R<String> {
    let v = v.bare();
    match name {
        "text" => Ok(render_raw(v)),
        "json" => Ok(render(v)),
        "base64" => Ok(b64_encode(render_raw(v).as_bytes())),
        "base64d" => {
            let text = render_raw(v);
            let raw = b64_decode(&text).map_err(|why| {
                let s = JqVal::str(text.as_str());
                JqErr::msg(format!("{}{} {why}", s.type_name(), paren_of(&s)))
            })?;
            Ok(jq_utf8_lossy(&raw))
        }
        "uri" => {
            let s = render_raw(v);
            let mut out = String::with_capacity(s.len());
            for b in s.bytes() {
                if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                    out.push(b as char);
                } else {
                    let _ = write!(out, "%{b:02X}");
                }
            }
            Ok(out)
        }
        // The inverse of `@uri`: `%XX` escapes decode to bytes, every other
        // byte passes through (`+` is NOT a space), and the decoded bytes must
        // be UTF-8. A stray `%`, a non-hex escape or a bad byte sequence is an
        // error, as jq 1.8 has it.
        "urid" => {
            let s = render_raw(v);
            let bad = || {
                JqErr::msg(format!(
                    "string ({}) is not a valid uri encoding",
                    render(&JqVal::str(s.as_str()))
                ))
            };
            let src = s.as_bytes();
            let mut raw = Vec::with_capacity(src.len());
            let mut i = 0;
            while i < src.len() {
                if src[i] == b'%' {
                    let hex = src.get(i + 1..i + 3).ok_or_else(bad)?;
                    // Exactly two hex DIGITS: `from_str_radix` alone also takes
                    // a sign, so `%+1` decoded to byte 1 where jq refuses.
                    if !hex.iter().all(u8::is_ascii_hexdigit) {
                        return Err(bad());
                    }
                    let hex = std::str::from_utf8(hex).map_err(|_| bad())?;
                    raw.push(u8::from_str_radix(hex, 16).map_err(|_| bad())?);
                    i += 3;
                } else {
                    raw.push(src[i]);
                    i += 1;
                }
            }
            String::from_utf8(raw).map_err(|_| bad())
        }
        "csv" | "tsv" => {
            let JqVal::Arr(a) = v else {
                return Err(JqErr::msg(format!(
                    "{}{} cannot be {}-formatted, only array",
                    v.type_name(),
                    paren_of(v),
                    name
                )));
            };
            let mut cells = Vec::with_capacity(a.len());
            for e in a.iter() {
                cells.push(match e {
                    JqVal::Null => String::new(),
                    JqVal::Bool(b) => b.to_string(),
                    // NaN renders as an empty cell, as null does.
                    JqVal::Num(n, _) if n.is_nan() => String::new(),
                    JqVal::Num(..) => render(e),
                    JqVal::Str(s) if name == "csv" => {
                        format!("\"{}\"", escape_string(s, &[('"', "\"\"")]))
                    }
                    JqVal::Str(s) => escape_string(
                        s,
                        &[('\t', "\\t"), ('\r', "\\r"), ('\n', "\\n"), ('\\', "\\\\")],
                    ),
                    other => {
                        return Err(JqErr::msg(format!(
                            // jq 1.8.2 words this "csv row" for `@tsv` too.
                            "{}{} is not valid in a csv row",
                            other.type_name(),
                            paren_of(other)
                        )));
                    }
                });
            }
            Ok(cells.join(if name == "csv" { "," } else { "\t" }))
        }
        "html" => {
            let table = [
                ('&', "&amp;"),
                ('<', "&lt;"),
                ('>', "&gt;"),
                ('\'', "&apos;"),
                ('"', "&quot;"),
            ];
            Ok(escape_string(&render_raw(v), &table))
        }
        "sh" => {
            let one = |x: &JqVal| -> R<String> {
                match x {
                    JqVal::Str(s) => Ok(format!("'{}'", escape_string(s, &[('\'', r"'\''")]))),
                    JqVal::Null | JqVal::Bool(_) | JqVal::Num(..) => Ok(render(x)),
                    other => Err(JqErr::msg(format!(
                        "{}{} can not be escaped for shell",
                        other.type_name(),
                        paren_of(other)
                    ))),
                }
            };
            match v.bare() {
                JqVal::Arr(a) => Ok(a.iter().map(one).collect::<R<Vec<_>>>()?.join(" ")),
                other => one(other),
            }
        }
        other => Err(JqErr::msg(format!("{other} is not a valid format"))),
    }
}

/// jq's `escape_string` (src/builtin.c), shared by `@csv`, `@tsv`, `@html` and
/// `@sh`: each listed char is replaced by its escape, and a NUL is ALWAYS
/// written as the two characters `\0`, whatever the table says.
fn escape_string(s: &str, table: &[(char, &str)]) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match table.iter().find(|(k, _)| *k == c) {
            _ if c == '\0' => out.push_str("\\0"),
            Some((_, esc)) => out.push_str(esc),
            None => out.push(c),
        }
    }
    out
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// `@base64d`'s decoder, ported from jq 1.8.2 `src/builtin.c:f_format`.
///
/// Decoding stops at the first `=`, whatever follows it (`"YW=Jj"` is `a`);
/// any byte outside the alphabet -- a newline included -- is `is not valid
/// base64 data`; and one character left over after the last whole group is
/// `trailing base64 byte found`, while two or three decode with their spare
/// bits dropped. The error is the reason, for the caller to name the input.
fn b64_decode(s: &str) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 2);
    let (mut code, mut held) = (0u32, 0u32);
    for c in s.bytes().take_while(|&c| c != b'=') {
        let v = B64
            .iter()
            .position(|&x| x == c)
            .ok_or("is not valid base64 data")? as u32;
        code = (code << 6) | v;
        held += 1;
        if held == 4 {
            out.extend_from_slice(&[(code >> 16) as u8, (code >> 8) as u8, code as u8]);
            (code, held) = (0, 0);
        }
    }
    match held {
        3 => out.extend_from_slice(&[(code >> 10) as u8, (code >> 2) as u8]),
        2 => out.push((code >> 4) as u8),
        1 => return Err("trailing base64 byte found"),
        _ => {}
    }
    Ok(out)
}

/// Bytes to a string the way jq 1.8's `jv_string_sized` repairs them, which is
/// not `String::from_utf8_lossy`: jq's `jvp_utf8_next` writes one U+FFFD per
/// bad sequence, and a lead byte whose sequence runs past the END takes every
/// remaining byte with it -- `9E E9 65` is two replacement characters in jq,
/// where the Rust repair keeps the `e`.
fn jq_utf8_lossy(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let first = b[i];
        let length = match first {
            0x00..=0x7F => {
                out.push(first as char);
                i += 1;
                continue;
            }
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            // A continuation byte, or a lead byte no sequence starts with.
            _ => {
                out.push('\u{FFFD}');
                i += 1;
                continue;
            }
        };
        if i + length > b.len() {
            out.push('\u{FFFD}');
            break;
        }
        match std::str::from_utf8(&b[i..i + length]) {
            Ok(s) => {
                out.push_str(s);
                i += length;
            }
            Err(_) => {
                out.push('\u{FFFD}');
                // Consume up to the first byte that is not a continuation; an
                // overlong or surrogate sequence of whole length goes entirely.
                let bad = (1..length)
                    .find(|&k| b[i + k] & 0xC0 != 0x80)
                    .unwrap_or(length);
                i += bad;
            }
        }
    }
    out
}

/// `{k: v, …}`. Both the key and the value are generators, and the FIRST entry
/// varies slowest — measured: `{a:(1,2), b:(3,4)}` emits `a=1,b=3`, `a=1,b=4`,
/// `a=2,b=3`, `a=2,b=4`.
fn build_object(
    it: &Interp,
    entries: &[ObjEntry],
    i: usize,
    acc: Vec<(Rc<str>, JqVal)>,
    input: &JqVal,
    env: &Env,
    out: Sink,
) -> R<()> {
    let Some(ObjEntry::KeyVal(kf, vf)) = entries.get(i) else {
        return out(JqVal::obj(acc));
    };
    eval(it, kf, input, env, &mut |k| {
        let JqVal::Str(key) = k.bare().clone() else {
            return Err(JqErr::msg(format!(
                "Cannot use {}{} as object key",
                k.type_name(),
                paren_of(&k)
            )));
        };
        eval(it, vf, input, env, &mut |v| {
            let mut next = acc.clone();
            match next.iter_mut().find(|(ek, _)| *ek == key) {
                Some(slot) => slot.1 = v,
                None => next.push((key.clone(), v)),
            }
            build_object(it, entries, i + 1, next, input, env, out)
        })
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Destructuring
// ─────────────────────────────────────────────────────────────────────────────

/// Bind `pat` against `v` and call `k` with the extended environment. A pattern
/// whose key is a generator produces several binding sets, so `k` may run more
/// than once.
fn bind_pattern(
    it: &Interp,
    pat: &Pattern,
    v: &JqVal,
    env: &Env,
    k: &mut dyn FnMut(Env) -> R<()>,
) -> R<()> {
    match pat {
        Pattern::Var(name) => k(env.bind(name.clone(), v.clone())),
        Pattern::Arr(subs) => bind_arr(it, subs, 0, v, env.clone(), k),
        Pattern::Obj(subs) => bind_obj(it, subs, 0, v, env.clone(), k),
    }
}

fn bind_arr(
    it: &Interp,
    subs: &[Pattern],
    i: usize,
    v: &JqVal,
    env: Env,
    k: &mut dyn FnMut(Env) -> R<()>,
) -> R<()> {
    let Some(p) = subs.get(i) else { return k(env) };
    let elem = index_value(v, &JqVal::num(i as f64))?;
    bind_pattern(it, p, &elem, &env, &mut |e| {
        bind_arr(it, subs, i + 1, v, e, k)
    })
}

/// One entry of an object destructuring pattern: the KEY filter, an optional
/// sub-pattern for the value, and the variable a `{$a}` shorthand binds.
type ObjPatEntry = (Filter, Option<Pattern>, Option<Rc<str>>);

fn bind_obj(
    it: &Interp,
    subs: &[ObjPatEntry],
    i: usize,
    v: &JqVal,
    env: Env,
    k: &mut dyn FnMut(Env) -> R<()>,
) -> R<()> {
    let Some((kf, sub, shorthand)) = subs.get(i) else {
        return k(env);
    };
    eval(it, kf, v, &env, &mut |key| {
        let field = index_value(v, &key)?;
        let base = match shorthand {
            // `{$a}` binds `$a` to `.a` AND may still carry a sub-pattern.
            Some(name) => env.bind(name.clone(), field.clone()),
            None => env.clone(),
        };
        match sub {
            Some(p) => bind_pattern(it, p, &field, &base, &mut |e| {
                bind_obj(it, subs, i + 1, v, e, k)
            }),
            None => bind_obj(it, subs, i + 1, v, base, k),
        }
    })
}

/// `SRC as P1 ?// P2 | BODY`. Each alternative is tried in turn; the first that
/// binds without error wins, and every variable named anywhere in the group is
/// bound (to `null` where the winning pattern does not mention it), which is
/// jq's rule for the destructuring-alternative operator.
fn bind_alternatives(
    it: &Interp,
    pats: &[Pattern],
    v: &JqVal,
    env: &Env,
    input: &JqVal,
    body: &Filter,
    out: Sink,
) -> R<()> {
    let mut all_names = Vec::new();
    for p in pats {
        collect_pattern_vars(p, &mut all_names);
    }
    for (i, p) in pats.iter().enumerate() {
        let last = i + 1 == pats.len();
        let base = all_names
            .iter()
            .fold(env.clone(), |e, n| e.bind(n.clone(), JqVal::Null));
        // The BODY sees the original `.`, not the bound value: `jq -n '1 as $x
        // | .'` is `null`. Only the pattern reads `v`.
        let r = bind_pattern(it, p, v, &base, &mut |benv| {
            eval(it, body, input, &benv, &mut |o| {
                out(o).map_err(wrap_downstream)
            })
        });
        match r {
            Ok(()) => return Ok(()),
            Err(e) => match unwrap_downstream(e) {
                Ok(real) => return Err(real),
                Err(err) if last => return Err(err),
                Err(JqErr::Err(_)) => continue,
                Err(other) => return Err(other),
            },
        }
    }
    Ok(())
}

fn collect_pattern_vars(p: &Pattern, out: &mut Vec<Rc<str>>) {
    match p {
        Pattern::Var(n) => out.push(n.clone()),
        Pattern::Arr(subs) => subs.iter().for_each(|s| collect_pattern_vars(s, out)),
        Pattern::Obj(subs) => {
            for (_, sub, shorthand) in subs {
                if let Some(n) = shorthand {
                    out.push(n.clone());
                }
                if let Some(s) = sub {
                    collect_pattern_vars(s, out);
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Path expressions
// ─────────────────────────────────────────────────────────────────────────────

/// The path state jq keeps while it evaluates a path expression
/// (src/execute.c): the keys followed so far, and `value_at_path`, the value
/// they lead to. The value TRAVELLING with the state may differ — a filter
/// that is not a path expression replaces it — and the path is INTACT only
/// while the two are [`identical`]. jq checks that where it next needs the
/// path (an index step, an iteration, the end of `path(…)`), so a value that
/// is never used as a path is no error, and one identical to the value at the
/// path (`path(.a | tostring)` on a string) keeps the path intact.
#[derive(Clone)]
struct Tracked {
    keys: Vec<JqVal>,
    at: JqVal,
}

impl Tracked {
    /// `PATH_BEGIN`: an empty path at `v`.
    fn root(v: &JqVal) -> Tracked {
        Tracked {
            keys: Vec::new(),
            at: v.clone(),
        }
    }

    /// `path_append`: one more key, reaching `next`.
    fn push(&self, key: JqVal, next: &JqVal) -> Tracked {
        let mut keys = self.keys.clone();
        keys.push(key);
        Tracked {
            keys,
            at: next.clone(),
        }
    }

    fn intact(&self, v: &JqVal) -> bool {
        identical(v, &self.at)
    }
}

/// Where a path expression's results go: the path state and the value
/// travelling with it.
type PathSink<'a> = &'a mut dyn FnMut(Tracked, JqVal) -> R<()>;

/// `PATH_END`: the keys of a path expression's result, or jq's refusal
/// naming the value when the path is no longer intact.
fn intact_path(p: Tracked, v: &JqVal) -> R<Vec<JqVal>> {
    if p.intact(v) {
        Ok(p.keys)
    } else {
        Err(JqErr::msg(format!(
            "Invalid path expression with result {}",
            dump_trunc(v)
        )))
    }
}

/// jq's `jv_identical`, as far as arb's values can tell: equal null/booleans,
/// a computed number with the same bits, or the same shared literal, string,
/// array or object allocation.
fn identical(a: &JqVal, b: &JqVal) -> bool {
    match (a, b) {
        (JqVal::Null, JqVal::Null) => true,
        (JqVal::Bool(x), JqVal::Bool(y)) => x == y,
        (JqVal::Num(x, None), JqVal::Num(y, None)) => x.to_bits() == y.to_bits(),
        (JqVal::Num(_, Some(x)), JqVal::Num(_, Some(y))) => Rc::ptr_eq(x, y),
        (JqVal::Str(x), JqVal::Str(y)) => Rc::ptr_eq(x, y),
        (JqVal::Arr(x), JqVal::Arr(y)) => Rc::ptr_eq(x, y),
        (JqVal::Obj(x), JqVal::Obj(y)) => Rc::ptr_eq(x, y),
        (JqVal::Node(x), JqVal::Node(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

/// Evaluate `f` as a PATH expression — jq with path tracking on. This is what
/// `path`, `del`, `paths`, `pick` and every assignment operator are built on.
///
/// `input` is the `.` that index/condition sub-filters see; `val` is the value
/// travelling with the path state `pre`.
fn eval_paths(
    it: &Interp,
    f: &Filter,
    input: &JqVal,
    pre: &Tracked,
    val: &JqVal,
    env: &Env,
    out: PathSink,
) -> R<()> {
    match f {
        Filter::Identity => out(pre.clone(), val.clone()),
        Filter::RecurseDefault => recurse_paths(pre, val, out),
        Filter::Field(..) | Filter::Index(..) | Filter::Slice(..) | Filter::Iterate(..) => {
            eval_index_paths(it, f, false, input, pre, val, env, out)
        }
        Filter::IndexOpt(inner) => eval_index_paths(it, inner, true, input, pre, val, env, out),
        Filter::Pipe(a, b) => eval_paths(it, a, input, pre, val, env, &mut |p, v| {
            eval_paths(it, b, &v, &p, &v, env, out)
        }),
        Filter::Comma(a, b) => {
            eval_paths(it, a, input, pre, val, env, out)?;
            eval_paths(it, b, input, pre, val, env, out)
        }
        Filter::Optional(inner) => eval_try_paths(it, inner, None, input, pre, val, env, out),
        // `gen_try`: the body is tracked; the handler runs on the error from
        // the state the `try` began in.
        Filter::Try(body, handler) => {
            eval_try_paths(it, body, handler.as_deref(), input, pre, val, env, out)
        }
        Filter::If(arms, els) => eval_if_paths(
            it,
            IfNode {
                arms,
                els: els.as_deref(),
            },
            input,
            pre,
            val,
            env,
            out,
        ),
        // The path form of `a // b`: errors in `a` propagate here too.
        Filter::Alt(a, b) => {
            let mut any = false;
            eval_paths(it, a, input, pre, val, env, &mut |p, v| {
                if v.truthy() {
                    any = true;
                    out(p, v)
                } else {
                    Ok(())
                }
            })?;
            if any {
                Ok(())
            } else {
                eval_paths(it, b, input, pre, val, env, out)
            }
        }
        Filter::Def(def, rest) => {
            let inner = env.define(def.clone());
            eval_paths(it, rest, input, pre, val, &inner, out)
        }
        Filter::Bind(src, pats, body) => eval(it, src, input, env, &mut |sv| {
            let mut names = Vec::new();
            for p in pats {
                collect_pattern_vars(p, &mut names);
            }
            let base = names
                .iter()
                .fold(env.clone(), |e, n| e.bind(n.clone(), JqVal::Null));
            bind_pattern(it, &pats[0], &sv, &base, &mut |benv| {
                eval_paths(it, body, input, pre, val, &benv, out)
            })
        }),
        Filter::Label(name, body) => {
            let id = it.labels.get() + 1;
            it.labels.set(id);
            let benv = env.bind(label_key(name), JqVal::num(id as f64));
            match eval_paths(it, body, input, pre, val, &benv, &mut |p, v| {
                out(p, v).map_err(wrap_downstream)
            }) {
                Err(JqErr::Break(b)) if b == id => Ok(()),
                Err(e) => match unwrap_downstream(e) {
                    Ok(real) => Err(real),
                    Err(other) => Err(other),
                },
                Ok(()) => Ok(()),
            }
        }
        // `gen_reduce`: the init is tracked, then each source item is tracked
        // from there and the update tracked from the item's state — but every
        // iteration BACKTRACKS, so the result travels with the state the init
        // left.
        Filter::Reduce(src, pat, init, update) => {
            eval_paths(it, init, input, pre, val, env, &mut |ti, init_v| {
                let mut acc = init_v;
                eval_paths(it, src, input, &ti, val, env, &mut |ts, item| {
                    bind_pattern(it, pat, &item, env, &mut |benv| {
                        let mut last = None;
                        eval_paths(it, update, &acc, &ts, &acc, &benv, &mut |_, v| {
                            last = Some(v);
                            Ok(())
                        })?;
                        acc = last.unwrap_or(JqVal::Null);
                        Ok(())
                    })
                })?;
                out(ti, acc.clone())
            })
        }
        // `gen_foreach`: as `reduce`, except each update output is extracted
        // from the state the source item and the update left.
        Filter::Foreach(src, pat, init, update, extract) => {
            eval_paths(it, init, input, pre, val, env, &mut |ti, init_v| {
                let mut acc = init_v;
                eval_paths(it, src, input, &ti, val, env, &mut |ts, item| {
                    bind_pattern(it, pat, &item, env, &mut |benv| {
                        let mut states = Vec::new();
                        eval_paths(it, update, &acc, &ts, &acc, &benv, &mut |tu, v| {
                            states.push((tu, v));
                            Ok(())
                        })?;
                        for (tu, st) in states {
                            acc = st.clone();
                            match extract {
                                Some(e) => eval_paths(it, e, &st, &tu, &st, &benv, out)?,
                                None => out(tu, st)?,
                            }
                        }
                        Ok(())
                    })
                })
            })
        }
        Filter::Call(name, args) => eval_call_paths(it, (name, args), input, pre, val, env, out),
        other => non_path(it, other, pre, val, env, out),
    }
}

/// `try`/`?` as a path expression. The body is tracked; downstream errors
/// pass through; a body error runs the handler (if any) from `pre`.
#[allow(clippy::too_many_arguments)]
fn eval_try_paths(
    it: &Interp,
    body: &Filter,
    handler: Option<&Filter>,
    input: &JqVal,
    pre: &Tracked,
    val: &JqVal,
    env: &Env,
    out: PathSink,
) -> R<()> {
    match eval_paths(it, body, input, pre, val, env, &mut |p, v| {
        out(p, v).map_err(wrap_downstream)
    }) {
        Ok(()) => Ok(()),
        Err(e) => match unwrap_downstream(e) {
            Ok(real) => Err(real),
            Err(JqErr::Err(payload)) => match handler {
                Some(h) => eval_paths(it, h, &payload, pre, &payload, env, out),
                None => Ok(()),
            },
            Err(other) => Err(other),
        },
    }
}

/// [`eval_index`] as a path expression: each step extends the path by its key
/// (a slice by its `{"start","end"}` object). Under `opt` the step's own error
/// yields no path — but a path that is no longer intact refuses either way,
/// as INDEX_OPT/EACH_OPT do.
#[allow(clippy::too_many_arguments)]
fn eval_index_paths(
    it: &Interp,
    f: &Filter,
    opt: bool,
    input: &JqVal,
    pre: &Tracked,
    val: &JqVal,
    env: &Env,
    out: PathSink,
) -> R<()> {
    let step = |p: &Tracked, v: &JqVal, key: JqVal, r: R<JqVal>, out: PathSink| {
        if !p.intact(v) {
            return Err(JqErr::msg(format!(
                "Invalid path expression near attempt to access element {} of {}",
                dump_trunc(&key),
                dump_trunc(v)
            )));
        }
        match r {
            Ok(next) => out(p.push(key, &next), next),
            Err(_) if opt => Ok(()),
            Err(e) => Err(e),
        }
    };
    match f {
        Filter::Field(base, name) => eval_paths(it, base, input, pre, val, env, &mut |p, v| {
            let key = JqVal::Str(name.clone());
            let r = index_value(&v, &key);
            step(&p, &v, key, r, out)
        }),
        Filter::Index(base, idx) => eval_paths(it, base, input, pre, val, env, &mut |p, v| {
            eval(it, idx, input, env, &mut |i| {
                let r = index_value(&v, &i);
                step(&p, &v, i, r, out)
            })
        }),
        Filter::Slice(base, lo, hi) => eval_paths(it, base, input, pre, val, env, &mut |p, v| {
            eval_opt(it, lo.as_deref(), input, env, &mut |l| {
                eval_opt(it, hi.as_deref(), input, env, &mut |h| {
                    let r = slice_value(&v, &l, &h);
                    let key =
                        JqVal::obj(vec![(Rc::from("start"), l.clone()), (Rc::from("end"), h)]);
                    step(&p, &v, key, r, out)
                })
            })
        }),
        Filter::Iterate(base) => eval_paths(it, base, input, pre, val, env, &mut |p, v| {
            if !p.intact(&v) {
                return Err(not_intact_iterate(&v));
            }
            match v.bare() {
                JqVal::Arr(a) => {
                    for (i, e) in a.iter().enumerate() {
                        out(p.push(JqVal::num(i as f64), e), e.clone())?;
                    }
                    Ok(())
                }
                JqVal::Obj(m) => {
                    for (k, e) in m.iter() {
                        out(p.push(JqVal::Str(k.clone()), e), e.clone())?;
                    }
                    Ok(())
                }
                _ if opt => Ok(()),
                other => Err(JqErr::msg(format!(
                    "Cannot iterate over {}{}",
                    other.type_name(),
                    paren_of(other)
                ))),
            }
        }),
        other => unreachable!("not an index step: {other:?}"),
    }
}

/// EACH/EACH_OPT over a path that is no longer intact.
fn not_intact_iterate(v: &JqVal) -> JqErr {
    JqErr::msg(format!(
        "Invalid path expression near attempt to iterate through {}",
        dump_trunc(v)
    ))
}

/// A filter that is not a path expression, met where a path is required. jq
/// RUNS it with tracking left as it was: each output travels on with the same
/// path state, to be judged where the path is next needed. A filter that
/// yields nothing is no error (`path(empty | tostring)` is empty), and one
/// that raises reports its own error (`null | path(abs)` is `null (null)
/// cannot be negated`).
fn non_path(
    it: &Interp,
    f: &Filter,
    pre: &Tracked,
    val: &JqVal,
    env: &Env,
    out: PathSink,
) -> R<()> {
    eval(it, f, val, env, &mut |v| out(pre.clone(), v))
}

/// The remaining arms of an `if` plus its `else`, walked one arm at a time.
#[derive(Clone, Copy)]
struct IfNode<'a> {
    arms: &'a [(Filter, Filter)],
    els: Option<&'a Filter>,
}

fn eval_if_paths(
    it: &Interp,
    node: IfNode,
    input: &JqVal,
    pre: &Tracked,
    val: &JqVal,
    env: &Env,
    out: PathSink,
) -> R<()> {
    let Some(((cond, then), rest)) = node.arms.split_first() else {
        return match node.els {
            Some(e) => eval_paths(it, e, input, pre, val, env, out),
            None => out(pre.clone(), val.clone()),
        };
    };
    eval(it, cond, val, env, &mut |c| {
        if c.truthy() {
            eval_paths(it, then, input, pre, val, env, out)
        } else {
            eval_if_paths(
                it,
                IfNode {
                    arms: rest,
                    els: node.els,
                },
                input,
                pre,
                val,
                env,
                out,
            )
        }
    })
}

/// The builtins that are legal inside a path expression.
fn eval_call_paths(
    it: &Interp,
    call: (&str, &[Rc<Filter>]),
    input: &JqVal,
    pre: &Tracked,
    val: &JqVal,
    env: &Env,
    out: PathSink,
) -> R<()> {
    let (name, args) = call;
    match (name, args.len()) {
        ("empty", 0) => Ok(()),
        ("error", 0) => Err(JqErr::Err(val.clone())),
        ("error", 1) => eval(it, &args[0], val, env, &mut |m| Err(JqErr::Err(m))),
        ("select", 1) => eval(it, &args[0], val, env, &mut |c| {
            if c.truthy() {
                out(pre.clone(), val.clone())
            } else {
                Ok(())
            }
        }),
        // `_jq_path_append`: an intact path grows by the whole array; one that
        // is not intact is left as it was, with the value found.
        ("getpath", 1) => eval(it, &args[0], val, env, &mut |p| {
            let JqVal::Arr(segs) = &p else {
                return Err(JqErr::msg("Path must be specified as an array"));
            };
            let found = get_path(val, segs)?;
            if !pre.intact(val) {
                return out(pre.clone(), found);
            }
            let mut keys = pre.keys.clone();
            keys.extend(segs.iter().cloned());
            out(
                Tracked {
                    keys,
                    at: found.clone(),
                },
                found,
            )
        }),
        ("recurse", 0) => recurse_paths(pre, val, out),
        ("recurse", 1) => recurse_paths_f(it, &args[0], input, pre, val, env, out),
        ("first", 1) => {
            let mut done = false;
            let r = eval_paths(it, &args[0], input, pre, val, env, &mut |p, v| {
                done = true;
                out(p, v).map_err(wrap_downstream)?;
                Err(JqErr::Break(u64::MAX))
            });
            match r {
                Err(JqErr::Break(b)) if b == u64::MAX && done => Ok(()),
                Err(e) => match unwrap_downstream(e) {
                    Ok(real) => Err(real),
                    Err(other) => Err(other),
                },
                Ok(()) => Ok(()),
            }
        }
        // `gen_last_1`: the argument is tracked, but every output BACKTRACKS,
        // so the last value travels with the state `last` began in.
        ("last", 1) => {
            let mut last = None;
            eval_paths(it, &args[0], input, pre, val, env, &mut |_, v| {
                last = Some(v);
                Ok(())
            })?;
            match last {
                Some(v) => out(pre.clone(), v),
                None => Ok(()),
            }
        }
        // A user-defined function may still be a path expression — inline its
        // body and keep walking.
        _ => match env.find_fn(name, args.len()) {
            Some(node) => {
                let (body, benv) = bind_call(it, &node, args, env)?;
                eval_paths(it, &body, input, pre, val, &benv, out)
            }
            None => non_path(
                it,
                &Filter::Call(Rc::from(name), args.to_vec()),
                pre,
                val,
                env,
                out,
            ),
        },
    }
}

/// `..` as a path expression: `recurse(.[]?)`. The node itself comes out
/// first; descending through a path that is no longer intact refuses, as
/// EACH_OPT does.
fn recurse_paths(pre: &Tracked, val: &JqVal, out: PathSink) -> R<()> {
    // The NODE is what comes out, so `.. | anchor` sees the box; the traversal
    // walks the value inside it, so `paths` does not stop at the first commented
    // container.
    out(pre.clone(), val.clone())?;
    if !pre.intact(val) {
        return Err(not_intact_iterate(val));
    }
    match val.bare() {
        JqVal::Arr(a) => {
            for (i, e) in a.iter().enumerate() {
                recurse_paths(&pre.push(JqVal::num(i as f64), e), e, out)?;
            }
            Ok(())
        }
        JqVal::Obj(m) => {
            for (k, e) in m.iter() {
                recurse_paths(&pre.push(JqVal::Str(k.clone()), e), e, out)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn recurse_paths_f(
    it: &Interp,
    f: &Filter,
    input: &JqVal,
    pre: &Tracked,
    val: &JqVal,
    env: &Env,
    out: PathSink,
) -> R<()> {
    out(pre.clone(), val.clone())?;
    eval_paths(it, f, input, pre, val, env, &mut |p, v| {
        recurse_paths_f(it, f, input, &p, &v, env, out)
    })
}

/// Follow a `crate::jqval::Seg` path — the shape `crate::jq`'s translated
/// `Field` op carries — through a jq value.
///
/// The point is the RETURN: an object that comes back through this keeps its
/// document key order, where routing the same lookup through `serde_json` gives
/// it back alphabetised. Measured against `yq -o=json`, `.nested` over a YAML
/// mapping came back re-sorted before this existed.
pub fn get_seg_path(v: &JqVal, segs: &[crate::jqval::Seg]) -> Result<JqVal, String> {
    let mut cur = v.clone();
    for seg in segs {
        if matches!(cur, JqVal::Null) {
            return Ok(JqVal::Null);
        }
        let idx = match seg {
            crate::jqval::Seg::Key(k) => JqVal::str(k.as_str()),
            crate::jqval::Seg::Index(i) => JqVal::num(*i as f64),
        };
        cur = index_value(&cur, &idx).map_err(|e| e.to_message())?;
    }
    Ok(cur)
}

/// `getpath`, as a value operation. A path through a non-container yields
/// `null` rather than an error, which is jq's rule.
fn get_path(v: &JqVal, segs: &[JqVal]) -> R<JqVal> {
    let mut cur = v.clone();
    for s in segs {
        if matches!(cur.bare(), JqVal::Null) {
            return Ok(JqVal::Null);
        }
        cur = index_value(&cur, s)?;
    }
    Ok(cur)
}

/// `setpath`. Missing containers are created — an object for a string segment,
/// an array (null-padded) for a numeric one — which is what makes
/// `null | setpath(["a",1];9)` produce `{"a":[null,9]}`.
fn set_path(v: &JqVal, segs: &[JqVal], newv: JqVal) -> R<JqVal> {
    let Some((seg, rest)) = segs.split_first() else {
        return Ok(newv);
    };
    // A YAML container being written INTO keeps its own metadata: `.a.b = 5`
    // over a commented document must not strip the document's comments. The
    // rebuilt container is re-boxed with the metadata the old one carried.
    let rebox = |built: JqVal| match v.meta() {
        Some(m) => JqVal::wrap(built, m.clone()),
        None => built,
    };
    let v = v.bare();
    match seg.bare() {
        JqVal::Str(k) => {
            let mut m = match v {
                JqVal::Obj(m) => m.as_ref().clone(),
                JqVal::Null => Vec::new(),
                other => {
                    return Err(JqErr::msg(format!(
                        "Cannot index {} with string{}",
                        other.type_name(),
                        paren_of(seg)
                    )))
                }
            };
            let old = m
                .iter()
                .find(|(ek, _)| ek == k)
                .map_or(JqVal::Null, |(_, x)| x.clone());
            let sub = set_path(&old, rest, newv)?;
            match m.iter_mut().find(|(ek, _)| ek == k) {
                Some(slot) => slot.1 = sub,
                None => m.push((k.clone(), sub)),
            }
            Ok(rebox(JqVal::obj(m)))
        }
        JqVal::Num(n, _) => {
            let mut a = match v {
                JqVal::Arr(a) => a.as_ref().clone(),
                JqVal::Null => Vec::new(),
                other => {
                    return Err(JqErr::msg(format!(
                        "Cannot index {} with number{}",
                        other.type_name(),
                        paren_of(seg)
                    )))
                }
            };
            // `jv_set` + `jv_array_set` (src/jv_aux.c, src/jv.c): NaN refuses, the
            // index is clamped to C `int` and truncated, a negative one counts
            // from the end, and one past `INT_MAX >> 2` refuses rather than
            // allocating the gap.
            if n.is_nan() {
                return Err(JqErr::msg("Cannot set array element at NaN index"));
            }
            let mut i = n.clamp(f64::from(i32::MIN), f64::from(i32::MAX)).trunc();
            if i < 0.0 {
                i += a.len() as f64;
                if i < 0.0 {
                    return Err(JqErr::msg("Out of bounds negative array index"));
                }
            }
            if i > f64::from(i32::MAX >> 2) {
                return Err(JqErr::msg("Array index too large"));
            }
            let i = i as usize;
            while a.len() <= i {
                a.push(JqVal::Null);
            }
            a[i] = set_path(&a[i].clone(), rest, newv)?;
            Ok(rebox(JqVal::arr(a)))
        }
        JqVal::Obj(_) => {
            // A `{"start":…,"end":…}` segment replaces an array SLICE.
            let (lo, hi) = (
                seg.obj_get("start").cloned().unwrap_or(JqVal::Null),
                seg.obj_get("end").cloned().unwrap_or(JqVal::Null),
            );
            let a = match v {
                JqVal::Arr(a) => a.as_ref().clone(),
                JqVal::Null => Vec::new(),
                JqVal::Str(_) => return Err(JqErr::msg("Cannot update string slices")),
                other => {
                    return Err(JqErr::msg(format!(
                        "Cannot index {} with object ({{\"start\":{},\"end\":{}}})",
                        other.type_name(),
                        render(&lo),
                        render(&hi)
                    )))
                }
            };
            let (s, e) = slice_bounds(seg.obj_get("start"), seg.obj_get("end"), a.len())?;
            let cur = JqVal::arr(a[s..e].to_vec());
            let sub = set_path(&cur, rest, newv)?;
            let JqVal::Arr(repl) = sub else {
                return Err(JqErr::msg(
                    "A slice of an array can only be assigned another array",
                ));
            };
            let mut out = a[..s].to_vec();
            out.extend(repl.iter().cloned());
            out.extend(a[e..].iter().cloned());
            Ok(rebox(JqVal::arr(out)))
        }
        other => Err(JqErr::msg(format!(
            "Invalid path component {}",
            other.type_name()
        ))),
    }
}

/// `delpaths` — a port of `jv_delpaths` (src/jv_aux.c). The paths are sorted,
/// an empty path deletes everything, and `delpaths_sorted` walks them grouped
/// by their leading key so that every key of one container is removed in a
/// single `jv_dels` call, against the ORIGINAL indices.
fn del_paths(v: &JqVal, mut paths: Vec<Vec<JqVal>>) -> R<JqVal> {
    paths.sort_by(|a, b| cmp_sort(&JqVal::arr(a.clone()), &JqVal::arr(b.clone())));
    match paths.first() {
        None => Ok(v.clone()),
        Some(p) if p.is_empty() => Ok(JqVal::Null),
        Some(_) => delpaths_sorted(v, &paths, 0),
    }
}

/// `delpaths_sorted`: `paths` share their first `start` keys and are sorted,
/// so the paths through one key are adjacent. A path that ENDS at a key deletes
/// it whole; the others recurse into the value under it.
fn delpaths_sorted(object: &JqVal, paths: &[Vec<JqVal>], start: usize) -> R<JqVal> {
    let mut object = object.clone();
    let mut delkeys = Vec::new();
    let mut i = 0;
    while i < paths.len() {
        let key = &paths[i][start];
        let delkey = paths[i].len() == start + 1;
        let mut j = i + 1;
        while j < paths.len() && cmp_vals(key, &paths[j][start]) == Ordering::Equal {
            j += 1;
        }
        if delkey {
            delkeys.push(key.clone());
        } else {
            let sub = index_value(&object, key)?;
            if !matches!(sub.bare(), JqVal::Null) {
                let newsub = delpaths_sorted(&sub, &paths[i..j], start + 1)?;
                object = set_path(&object, std::slice::from_ref(key), newsub)?;
            }
        }
        i = j;
    }
    jv_dels(&object, &delkeys)
}

/// `jv_dels`: remove every key in `keys` from one container at once. On an
/// array the indices all refer to the original positions — a NaN index
/// deletes nothing, a negative one counts from the end, and a slice object
/// removes its `parse_slice` range. The container keeps its comments and
/// anchor, as in `set_path`.
fn jv_dels(t: &JqVal, keys: &[JqVal]) -> R<JqVal> {
    let rebox = |built: JqVal| match t.meta() {
        Some(m) => JqVal::wrap(built, m.clone()),
        None => built,
    };
    match t.bare() {
        _ if keys.is_empty() => Ok(t.clone()),
        JqVal::Null => Ok(t.clone()),
        JqVal::Arr(a) => {
            let len = a.len();
            let mut neg = Vec::new();
            let mut nonneg = Vec::new();
            let mut slices = Vec::new();
            for key in keys {
                match key.bare() {
                    JqVal::Num(n, _) if n.is_nan() => {}
                    JqVal::Num(n, _) if *n < 0.0 => neg.push(*n),
                    JqVal::Num(n, _) => nonneg.push(*n),
                    k @ JqVal::Obj(_) => {
                        slices.push(slice_bounds(k.obj_get("start"), k.obj_get("end"), len)?)
                    }
                    k => {
                        return Err(JqErr::msg(format!(
                            "Cannot delete {} element of array",
                            k.type_name()
                        )))
                    }
                }
            }
            // The C walks both sorted key lists alongside the array; `(int)`
            // truncates each index toward zero.
            let (mut ni, mut pi) = (0, 0);
            let mut out = Vec::with_capacity(len);
            for (i, e) in a.iter().enumerate() {
                let i = i as i64;
                let mut del = false;
                while ni < neg.len() {
                    let delidx = len as i64 + neg[ni] as i64;
                    del |= i == delidx;
                    if i < delidx {
                        break;
                    }
                    ni += 1;
                }
                while pi < nonneg.len() {
                    let delidx = nonneg[pi] as i64;
                    del |= i == delidx;
                    if i < delidx {
                        break;
                    }
                    pi += 1;
                }
                del |= slices.iter().any(|&(s, e)| s as i64 <= i && i < e as i64);
                if !del {
                    out.push(e.clone());
                }
            }
            Ok(rebox(JqVal::arr(out)))
        }
        JqVal::Obj(m) => {
            let mut dead: Vec<&str> = Vec::with_capacity(keys.len());
            for k in keys {
                match k.bare() {
                    JqVal::Str(s) => dead.push(s),
                    k => {
                        return Err(JqErr::msg(format!(
                            "Cannot delete {} field of object",
                            k.type_name()
                        )))
                    }
                }
            }
            Ok(rebox(JqVal::obj(
                m.iter()
                    .filter(|(ek, _)| !dead.contains(&&**ek))
                    .cloned()
                    .collect(),
            )))
        }
        other => Err(JqErr::msg(format!(
            "Cannot delete fields from {}",
            other.type_name()
        ))),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Assignment
// ─────────────────────────────────────────────────────────────────────────────

/// The assignment family. Every form is defined in terms of `path`/`setpath`,
/// which is how jq defines them: `a = b` sets every path `a` names to each value
/// `b` produces, `a |= f` maps `f` over the value at each such path, and
/// `a op= b` is `a |= . op ($input | b)` with `b` seeing the ORIGINAL input.
fn eval_assign(
    it: &Interp,
    op: AssignOp,
    lhs: &Filter,
    rhs: &Filter,
    input: &JqVal,
    env: &Env,
    out: Sink,
) -> R<()> {
    // yq's metadata assignment, recognised here because `anchor` and friends are
    // VALUE filters rather than paths — `eval_paths` would refuse them as a
    // left-hand side outright. `anchor = "x"` sets it on `.`; the whole-document
    // edit yq spells `.a anchor = "x"` is `.a |= (anchor = "x")`, which reaches
    // this with `.` bound to the node at the path.
    if op == AssignOp::Set {
        if let Some((path, name)) = meta_assign_target(lhs) {
            return eval(it, rhs, input, env, &mut |rv| match path {
                None => out(set_meta(input, name, &rv)),
                Some(p) => {
                    let mut cur = input.clone();
                    let mut paths = Vec::new();
                    eval_paths(
                        it,
                        p,
                        input,
                        &Tracked::root(input),
                        input,
                        env,
                        &mut |pp, v| {
                            let pp = intact_path(pp, &v)?;
                            paths.push(pp);
                            Ok(())
                        },
                    )?;
                    for pp in paths {
                        let at = get_path(&cur, &pp)?;
                        let set = set_meta(&at, name, &rv);
                        cur = if pp.is_empty() {
                            set
                        } else {
                            set_path(&cur, &pp, set)?
                        };
                    }
                    out(cur)
                }
            });
        }
    }
    if op == AssignOp::Update {
        let mut cur = input.clone();
        let mut paths = Vec::new();
        eval_paths(
            it,
            lhs,
            input,
            &Tracked::root(input),
            input,
            env,
            &mut |p, v| {
                let p = intact_path(p, &v)?;
                paths.push(p);
                Ok(())
            },
        )?;
        // jq's `_modify` DELETES a path whose update produced nothing — but it
        // collects those paths and deletes them all at the END. Deleting as it
        // went shifted every later array index down by one, so
        // `[1,2,3] | .[] |= empty` answered `[2]` instead of `[]`.
        let mut dead = Vec::new();
        for p in paths {
            let old = get_path(&cur, &p)?;
            let mut first = None;
            eval(it, rhs, &old, env, &mut |v| {
                if first.is_none() {
                    first = Some(v);
                }
                Ok(())
            })?;
            match first {
                None => dead.push(p),
                Some(v) => cur = set_path(&cur, &p, v)?,
            }
        }
        if !dead.is_empty() {
            cur = del_paths(&cur, dead)?;
        }
        return out(cur);
    }
    // The remaining forms evaluate the right-hand side against the ORIGINAL
    // input, once per output value, and each output value gives one result.
    eval(it, rhs, input, env, &mut |rv| {
        let mut cur = input.clone();
        let mut paths = Vec::new();
        eval_paths(
            it,
            lhs,
            input,
            &Tracked::root(input),
            input,
            env,
            &mut |p, v| {
                let p = intact_path(p, &v)?;
                paths.push(p);
                Ok(())
            },
        )?;
        for p in paths {
            let newv = match op {
                AssignOp::Set => rv.clone(),
                AssignOp::Alt => {
                    let old = get_path(&cur, &p)?;
                    if old.truthy() {
                        old
                    } else {
                        rv.clone()
                    }
                }
                _ => {
                    let old = get_path(&cur, &p)?;
                    let bop = match op {
                        AssignOp::Add => BinOp::Add,
                        AssignOp::Sub => BinOp::Sub,
                        AssignOp::Mul => BinOp::Mul,
                        AssignOp::Div => BinOp::Div,
                        AssignOp::Mod => BinOp::Mod,
                        _ => unreachable!("Set/Alt/Update handled above"),
                    };
                    binop(bop, &old, &rv)?
                }
            };
            cur = set_path(&cur, &p, newv)?;
        }
        out(cur)
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Calls
// ─────────────────────────────────────────────────────────────────────────────

/// Resolve a call to a user function or closure argument: its body plus the
/// environment that body must run in.
fn bind_call(
    _it: &Interp,
    node: &Rc<FuncNode>,
    args: &[Rc<Filter>],
    caller: &Env,
) -> R<(Rc<Filter>, Env)> {
    match &node.kind {
        FnKind::Arg(f, aenv) => Ok((f.clone(), aenv.clone())),
        FnKind::User(def) => {
            // The body sees the definition's environment PLUS the function node
            // itself, which is what makes a recursive `def` need no fixed point.
            let mut env = Env {
                vars: node.vars.clone(),
                funcs: Some(node.clone()),
            };
            for (p, a) in def.params.iter().zip(args) {
                env = Env {
                    vars: env.vars.clone(),
                    funcs: Some(Rc::new(FuncNode {
                        name: p.clone(),
                        arity: 0,
                        kind: FnKind::Arg(a.clone(), caller.clone()),
                        next: env.funcs.clone(),
                        vars: env.vars.clone(),
                    })),
                };
            }
            Ok((def.body.clone(), env))
        }
    }
}

fn eval_call(
    it: &Interp,
    name: &str,
    args: &[Rc<Filter>],
    input: &JqVal,
    env: &Env,
    out: Sink,
) -> R<()> {
    // `pick/1` is the ONE spelling jq and yq both define, and they take
    // different arguments: jq wants path expressions (`pick(.a, .b)`) and yq
    // wants an array of keys (`pick(["a","b"])`), which jq refuses outright.
    // The Rust arm decides by looking at the argument and calls jq's own
    // definition for jq's form — so the decision has to happen BEFORE the
    // definition lookup, or the definition always wins and yq's form is an
    // error.
    let collision = name == "pick" && args.len() == 1;
    if !collision {
        if let Some(node) = env.find_fn(name, args.len()) {
            let (body, benv) = bind_call(it, &node, args, env)?;
            return eval(it, &body, input, &benv, out);
        }
    }
    builtin(it, name, args, input, env, out)
}

/// Evaluate `f` and require exactly one output — the shape every builtin that
/// takes a VALUE argument (rather than a filter) needs.
fn one(it: &Interp, f: &Filter, input: &JqVal, env: &Env) -> R<JqVal> {
    let mut got = None;
    eval(it, f, input, env, &mut |v| {
        if got.is_none() {
            // Unboxed: an ARGUMENT to a jq builtin is a value, and every builtin
            // reached through here is a value operation. The yq operators that
            // need the node itself are dispatched before `builtin` unboxes its
            // input, so none of them come through this path.
            got = Some(v.bare().clone());
        }
        Ok(())
    })?;
    got.ok_or_else(|| JqErr::msg("argument produced no value"))
}

/// A string input, or jq's FIXED message for this builtin — the C builtins that
/// say e.g. "trim input must be a string" do not name the offending value.
fn str_or(v: &JqVal, msg: &str) -> R<Rc<str>> {
    match v.bare() {
        JqVal::Str(s) => Ok(s.clone()),
        _ => Err(JqErr::msg(msg)),
    }
}

/// A libm argument, refused with jq's `<type> (<value>) number required`.
fn num_required(v: &JqVal) -> R<f64> {
    v.as_f64().ok_or_else(|| {
        let b = v.bare();
        JqErr::msg(format!("{}{} number required", b.type_name(), paren_of(b)))
    })
}

/// `utf8bytelength`, with jq's "only strings have UTF-8 byte length" refusal.
fn utf8_len_input(v: &JqVal) -> R<f64> {
    match v.bare() {
        JqVal::Str(s) => Ok(s.len() as f64),
        other => Err(JqErr::msg(format!(
            "{}{} only strings have UTF-8 byte length",
            other.type_name(),
            paren_of(other)
        ))),
    }
}

fn want_str(v: &JqVal, who: &str) -> R<Rc<str>> {
    match v.bare() {
        JqVal::Str(s) => Ok(s.clone()),
        other => Err(JqErr::msg(format!(
            "{}{} cannot be {who}",
            other.type_name(),
            paren_of(other)
        ))),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Builtins implemented in Rust
//
// Everything that jq implements in C lives here; everything jq defines in
// `src/builtin.jq` lives in `PRELUDE` below, transcribed from those definitions
// so the semantics come from jq's own source rather than from a paraphrase.
// ─────────────────────────────────────────────────────────────────────────────

/// The libm entry points jq exposes that the `libc` crate does not declare on
/// every platform. Each is a pure `double -> double` (or `double, double`) C
/// function from `<math.h>`; declaring them here is the same binding `libc`
/// would provide, with no state and no allocation.
mod libm {
    extern "C" {
        pub fn lgamma(x: f64) -> f64;
        pub fn tgamma(x: f64) -> f64;
        pub fn erf(x: f64) -> f64;
        pub fn erfc(x: f64) -> f64;
        pub fn j0(x: f64) -> f64;
        pub fn j1(x: f64) -> f64;
        pub fn y0(x: f64) -> f64;
        pub fn y1(x: f64) -> f64;
        pub fn jn(n: i32, x: f64) -> f64;
        pub fn yn(n: i32, x: f64) -> f64;
        pub fn frexp(x: f64, exp: *mut i32) -> f64;
        pub fn modf(x: f64, iptr: *mut f64) -> f64;
        pub fn remainder(x: f64, y: f64) -> f64;
        pub fn fdim(x: f64, y: f64) -> f64;
        pub fn fmod(x: f64, y: f64) -> f64;
        pub fn nextafter(x: f64, y: f64) -> f64;
    }
}

/// The builtins whose arguments are jq VALUE parameters, and the order jq
/// enumerates their combinations in. `Some(true)`: a C function, whose LAST
/// argument varies slowest (`[pow(2,3;2,4)]` is `[4,9,16,81]`, measured against
/// jq 1.8.2). `Some(false)`: a `def f($a; $b)` from jq's `builtin.jq`, which
/// binds left to right so the FIRST varies slowest (`[range(1,2;3,4)]` is
/// `[1,2,1,2,3,2,2,3]`). Builtins whose arguments are closures (`limit`,
/// `sub`'s replacement, `path`) are not listed: they consume the generator
/// themselves.
fn value_param_order(name: &str, arity: usize) -> Option<bool> {
    match (name, arity) {
        ("range", 2) | ("range", 3) => Some(false),
        ("split", 1)
        | ("startswith", 1)
        | ("endswith", 1)
        | ("_strindices", 1)
        | ("setpath", 2)
        | ("delpaths", 1)
        | ("has", 1)
        | ("contains", 1)
        | ("bsearch", 1)
        | ("strftime", 1)
        | ("strflocaltime", 1)
        | ("strptime", 1)
        | ("format", 1)
        | ("halt_error", 1)
        | ("_match_impl", 3)
        | ("fma", 3) => Some(true),
        (
            "pow" | "atan2" | "fmin" | "fmax" | "ldexp" | "copysign" | "drem" | "fdim" | "fmod"
            | "hypot" | "nextafter" | "nexttoward" | "remainder" | "scalb" | "scalbln" | "jn"
            | "yn",
            2,
        ) => Some(true),
        _ => None,
    }
}

/// Bind each argument of a value-parameter builtin in `order` (lazily, so an
/// argument that raises after yielding still lets the earlier combinations
/// answer first) and call it once per combination with literal arguments.
#[allow(clippy::too_many_arguments)]
fn value_param_product(
    it: &Interp,
    name: &str,
    args: &[Rc<Filter>],
    order: &[usize],
    bound: &mut Vec<JqVal>,
    input: &JqVal,
    env: &Env,
    out: Sink,
) -> R<()> {
    let Some((&i, rest)) = order.split_first() else {
        let lits: Vec<Rc<Filter>> = bound
            .iter()
            .map(|v| Rc::new(Filter::Lit(v.clone())))
            .collect();
        return builtin(it, name, &lits, input, env, out);
    };
    eval(it, &args[i], input, env, &mut |v| {
        bound[i] = v.bare().clone();
        value_param_product(it, name, args, rest, bound, input, env, out)
    })
}

fn builtin(
    it: &Interp,
    name: &str,
    args: &[Rc<Filter>],
    input: &JqVal,
    env: &Env,
    out: Sink,
) -> R<()> {
    // The yq surface is the ONLY thing that may look at the node box, so it is
    // dispatched first, with `input` still wrapped. Everything below is a jq
    // builtin whose answer is about the VALUE, so it runs against the unboxed
    // one — that is what keeps a commented YAML scalar behaving in `sort`,
    // `tostring` and `+` exactly as the same scalar read from JSON does.
    if is_yq_builtin(name, args.len()) {
        return yq_builtin(it, name, args, input, env, out);
    }
    // A VALUE parameter that yields several values runs the builtin once per
    // combination, as jq does (`has("a","b")` is `true, false`). Once every
    // argument is a literal the call proceeds below as a plain one.
    if let Some(last_outer) = value_param_order(name, args.len()) {
        if !args.iter().all(|a| matches!(**a, Filter::Lit(_))) {
            let order: Vec<usize> = if last_outer {
                (0..args.len()).rev().collect()
            } else {
                (0..args.len()).collect()
            };
            let mut bound = vec![JqVal::Null; args.len()];
            return value_param_product(it, name, args, &order, &mut bound, input, env, out);
        }
    }
    let input = input.bare();
    match (name, args.len()) {
        ("empty", 0) => Ok(()),
        ("error", 0) => Err(JqErr::Err(input.clone())),
        ("error", 1) => eval(it, &args[0], input, env, &mut |m| Err(JqErr::Err(m))),
        ("not", 0) => out(JqVal::Bool(!input.truthy())),
        ("type", 0) => out(JqVal::str(input.type_name())),

        ("length", 0) => out(match input {
            JqVal::Null => JqVal::num(0.0),
            JqVal::Bool(_) => {
                return Err(JqErr::msg(format!(
                    "{}{} has no length",
                    input.type_name(),
                    paren_of(input)
                )))
            }
            JqVal::Num(n, _) => JqVal::num(n.abs()),
            JqVal::Str(s) => JqVal::num(s.chars().count() as f64),
            JqVal::Arr(a) => JqVal::num(a.len() as f64),
            JqVal::Obj(m) => JqVal::num(m.len() as f64),
            JqVal::Node(_) => unreachable!("bare() never returns a Node"),
        }),
        ("utf8bytelength", 0) => out(JqVal::num(utf8_len_input(input)?)),

        ("keys", 0) | ("keys_unsorted", 0) => {
            let mut ks = match input {
                JqVal::Obj(m) => m
                    .iter()
                    .map(|(k, _)| JqVal::Str(k.clone()))
                    .collect::<Vec<_>>(),
                JqVal::Arr(a) => (0..a.len()).map(|i| JqVal::num(i as f64)).collect(),
                other => {
                    return Err(JqErr::msg(format!(
                        "{}{} has no keys",
                        other.type_name(),
                        paren_of(other)
                    )))
                }
            };
            if name == "keys" {
                ks.sort_by(cmp_sort);
            }
            out(JqVal::arr(ks))
        }
        ("has", 1) => {
            let k = one(it, &args[0], input, env)?;
            out(JqVal::Bool(match (input, &k) {
                (JqVal::Obj(_), JqVal::Str(s)) => input.obj_get(s).is_some(),
                (JqVal::Arr(a), JqVal::Num(n, _)) => *n >= 0.0 && (*n as usize) < a.len(),
                (a, b) => {
                    return Err(JqErr::msg(format!(
                        "Cannot check whether {} has a {} key",
                        a.type_name(),
                        b.type_name()
                    )))
                }
            }))
        }
        ("contains", 1) => {
            let b = one(it, &args[0], input, env)?;
            out(JqVal::Bool(contains(input, &b)?))
        }

        ("tostring", 0) => out(JqVal::str(render_raw(input))),
        ("tojson", 0) => out(JqVal::str(render(input))),
        ("fromjson", 0) => {
            let s = want_str(input, "parsed as JSON")?;
            // jq's own reader (`jv_parse.c`), so what it accepts and the
            // refusal it words are both jq's.
            out(crate::jvparse::parse(&s)
                .map_err(|e| JqErr::msg(format!("{e} (while parsing '{s}')")))?)
        }
        ("tonumber", 0) => match input {
            JqVal::Num(..) => out(input.clone()),
            // jq 1.8 keeps the text as the number's literal, so `"1.000"` is
            // printed `1.000` and `"1e2"` `1E+2`, as a literal in a program is.
            JqVal::Str(s) => match s.parse::<f64>() {
                Ok(n) => out(num_from_literal(n, s)),
                Err(_) => Err(JqErr::msg(format!(
                    "{}{} cannot be parsed as a number",
                    input.type_name(),
                    paren_of(input)
                ))),
            },
            other => Err(JqErr::msg(format!(
                "{}{} cannot be parsed as a number",
                other.type_name(),
                paren_of(other)
            ))),
        },
        ("explode", 0) => out(JqVal::arr(
            str_or(input, "explode input must be a string")?
                .chars()
                .map(|c| JqVal::num(c as u32 as f64))
                .collect(),
        )),
        ("implode", 0) => {
            let JqVal::Arr(a) = input else {
                return Err(JqErr::msg("implode input must be an array"));
            };
            let mut s = String::with_capacity(a.len());
            for e in a.iter() {
                let n = e.as_f64().ok_or_else(|| {
                    JqErr::msg(format!(
                        "{}{} can't be imploded, unicode codepoint needs to be numeric",
                        e.type_name(),
                        paren_of(e)
                    ))
                })?;
                // jq 1.8 writes U+FFFD for a code point that is not a Unicode
                // scalar value -- negative, a surrogate, or past U+10FFFF.
                let scalar = (0.0..=f64::from(u32::MAX)).contains(&n).then_some(n as u32);
                s.push(scalar.and_then(char::from_u32).unwrap_or('\u{FFFD}'));
            }
            out(JqVal::str(s))
        }
        ("ascii_downcase", 0) => out(JqVal::str(
            str_or(input, "explode input must be a string")?.to_ascii_lowercase(),
        )),
        ("ascii_upcase", 0) => out(JqVal::str(
            str_or(input, "explode input must be a string")?.to_ascii_uppercase(),
        )),
        ("startswith", 1) | ("endswith", 1) => {
            let pre = one(it, &args[0], input, env)?;
            match (input, &pre) {
                (JqVal::Str(s), JqVal::Str(p)) => out(JqVal::Bool(if name == "startswith" {
                    s.starts_with(&**p)
                } else {
                    s.ends_with(&**p)
                })),
                _ => Err(JqErr::msg(format!("{name}() requires string inputs"))),
            }
        }
        ("ltrim", 0) => out(JqVal::str(
            str_or(input, "trim input must be a string")?.trim_start(),
        )),
        ("rtrim", 0) => out(JqVal::str(
            str_or(input, "trim input must be a string")?.trim_end(),
        )),
        ("trim", 0) => out(JqVal::str(
            str_or(input, "trim input must be a string")?.trim(),
        )),
        ("split", 1) => {
            let sep = one(it, &args[0], input, env)?;
            let s = str_or(input, "split input and separator must be strings")?;
            let sep = str_or(&sep, "split input and separator must be strings")?;
            out(JqVal::arr(split_str(&s, &sep)))
        }
        ("_strindices", 1) => {
            let needle = one(it, &args[0], input, env)?;
            let (h, n) = (
                want_str(input, "searched")?,
                want_str(&needle, "searched for")?,
            );
            let mut hits = Vec::new();
            if !n.is_empty() {
                let mut from = 0usize;
                // jq 1.8 reports CODE-POINT offsets (1.7 reported bytes), and
                // overlapping hits count, so the scan resumes one character —
                // not one byte — past each hit.
                while let Some(off) = h[from..].find(&*n) {
                    let at = from + off;
                    hits.push(JqVal::num(cp_index(&h, at) as f64));
                    from = at + h[at..].chars().next().map_or(1, char::len_utf8);
                }
            }
            out(JqVal::arr(hits))
        }

        ("sort", 0) => {
            let a = want_arr(input, "sorted")?;
            let mut v = a.as_ref().clone();
            v.sort_by(cmp_sort);
            out(JqVal::arr(v))
        }
        // jq 1.8.2's `def reverse: [.[length - 1 - range(0;length)]];` -- so a
        // non-array with a length of zero (`null`, `""`, `{}`, `0`) is `[]`, and
        // any other input fails on its first INDEX (`"abc"` is `Cannot index
        // string with number (2)`) or on `length` itself (`true`).
        ("reverse", 0) => match input {
            JqVal::Arr(a) => out(JqVal::arr(a.iter().rev().cloned().collect())),
            other => {
                let mut len = 0.0;
                builtin(it, "length", &[], other, env, &mut |n| {
                    len = n.as_f64().unwrap_or(0.0);
                    Ok(())
                })?;
                if len > 0.0 {
                    index_value(other, &JqVal::num(len - 1.0))?;
                }
                out(JqVal::arr(Vec::new()))
            }
        },
        ("unique", 0) => {
            let mut v = want_arr(input, "sorted")?.as_ref().clone();
            v.sort_by(cmp_sort);
            v.dedup_by(|b, a| eq_vals(a, b));
            out(JqVal::arr(v))
        }
        ("sort_by", 1) | ("group_by", 1) | ("unique_by", 1) => {
            let mut keyed = keyed_elements(
                it,
                &args[0],
                input,
                env,
                "cannot be sorted, as they are not both arrays",
            )?;
            keyed.sort_by(|a, b| cmp_sort(&a.0, &b.0));
            match name {
                "sort_by" => out(JqVal::arr(keyed.into_iter().map(|(_, v)| v).collect())),
                "unique_by" => {
                    let mut seen: Option<JqVal> = None;
                    let mut res = Vec::new();
                    for (k, v) in keyed {
                        if seen.as_ref().is_none_or(|s| !eq_vals(s, &k)) {
                            res.push(v);
                            seen = Some(k);
                        }
                    }
                    out(JqVal::arr(res))
                }
                _ => {
                    let mut groups: Vec<JqVal> = Vec::new();
                    let mut cur: Vec<JqVal> = Vec::new();
                    let mut seen: Option<JqVal> = None;
                    for (k, v) in keyed {
                        if seen.as_ref().is_some_and(|s| !eq_vals(s, &k)) {
                            groups.push(JqVal::arr(std::mem::take(&mut cur)));
                        }
                        seen = Some(k);
                        cur.push(v);
                    }
                    if seen.is_some() {
                        groups.push(JqVal::arr(cur));
                    }
                    out(JqVal::arr(groups))
                }
            }
        }
        ("min_by", 1) | ("max_by", 1) => {
            let keyed = keyed_elements(it, &args[0], input, env, "cannot be iterated over")?;
            // Measured against jq 1.8.2: `min_by` keeps the FIRST minimum and
            // `max_by` the LAST maximum, so the comparisons are not symmetric.
            let best = if name == "min_by" {
                keyed.into_iter().reduce(|a, b| {
                    if cmp_vals(&b.0, &a.0) == Ordering::Less {
                        b
                    } else {
                        a
                    }
                })
            } else {
                keyed.into_iter().reduce(|a, b| {
                    if cmp_vals(&b.0, &a.0) == Ordering::Less {
                        a
                    } else {
                        b
                    }
                })
            };
            out(best.map_or(JqVal::Null, |(_, v)| v))
        }
        ("min", 0) | ("max", 0) => {
            // jq's `minmax_by(x, x)` names the input twice.
            let JqVal::Arr(a) = input else {
                return Err(JqErr::msg(format!(
                    "{}{} and {}{} cannot be iterated over",
                    input.type_name(),
                    paren_of(input),
                    input.type_name(),
                    paren_of(input)
                )));
            };
            let best = if name == "min" {
                a.iter().cloned().reduce(|x, y| {
                    if cmp_vals(&y, &x) == Ordering::Less {
                        y
                    } else {
                        x
                    }
                })
            } else {
                a.iter().cloned().reduce(|x, y| {
                    if cmp_vals(&y, &x) == Ordering::Less {
                        x
                    } else {
                        y
                    }
                })
            };
            out(best.unwrap_or(JqVal::Null))
        }

        ("range", 2) | ("range", 3) => {
            let from = one(it, &args[0], input, env)?;
            let upto = one(it, &args[1], input, env)?;
            let Some(by) = args.get(2) else {
                // range/2 is jq's RANGE opcode (src/execute.c): both bounds must
                // be numbers, and it stops on the raw C `x >= upto`, so a NaN
                // on either side never stops (`limit(3; range(0; nan))` is
                // `[0,1,2]`).
                let (Some(mut x), Some(u)) = (from.as_f64(), upto.as_f64()) else {
                    return Err(JqErr::msg("Range bounds must be numeric"));
                };
                while !(x >= u) {
                    out(JqVal::num(x))?;
                    x += 1.0;
                }
                return Ok(());
            };
            // range/3 is jq-coded (src/builtin.jq): `if $by > 0 then
            // $init|while(. < $upto; . + $by) elif $by < 0 then
            // $init|while(. > $upto; . + $by) else empty end`. Its compares
            // and its `+` are jq's, so any types are accepted, NaN orders below
            // every number, and a bad `$by` raises from the `+`.
            let by = one(it, by, input, env)?;
            let zero = JqVal::num(0.0);
            let keep_going = match cmp_vals(&by, &zero) {
                Ordering::Greater => Ordering::Less,
                Ordering::Less => Ordering::Greater,
                Ordering::Equal => return Ok(()),
            };
            let mut x = from;
            while cmp_vals(&x, &upto) == keep_going {
                out(x.clone())?;
                x = binop(BinOp::Add, &x, &by)?;
            }
            Ok(())
        }

        ("floor", 0)
        | ("ceil", 0)
        | ("round", 0)
        | ("sqrt", 0)
        | ("fabs", 0)
        | ("log", 0)
        | ("log2", 0)
        | ("log10", 0)
        | ("exp", 0)
        | ("exp2", 0)
        | ("exp10", 0)
        | ("trunc", 0)
        | ("cbrt", 0)
        | ("sin", 0)
        | ("cos", 0)
        | ("tan", 0)
        | ("asin", 0)
        | ("acos", 0)
        | ("atan", 0)
        | ("sinh", 0)
        | ("cosh", 0)
        | ("tanh", 0)
        | ("nearbyint", 0)
        | ("significand", 0)
        | ("logb", 0)
        | ("acosh", 0)
        | ("asinh", 0)
        | ("atanh", 0)
        | ("expm1", 0)
        | ("log1p", 0)
        | ("rint", 0)
        | ("gamma", 0)
        | ("lgamma", 0)
        | ("tgamma", 0)
        | ("erf", 0)
        | ("erfc", 0)
        | ("j0", 0)
        | ("j1", 0)
        | ("y0", 0)
        | ("y1", 0) => {
            let n = input.as_f64().ok_or_else(|| {
                JqErr::msg(format!(
                    "{}{} number required",
                    input.type_name(),
                    paren_of(input)
                ))
            })?;
            out(JqVal::num(match name {
                "floor" => n.floor(),
                "ceil" => n.ceil(),
                "round" => n.round(),
                "sqrt" => n.sqrt(),
                "fabs" => n.abs(),
                "log" => n.ln(),
                "log2" => n.log2(),
                "log10" => n.log10(),
                "exp" => n.exp(),
                "exp2" => n.exp2(),
                "exp10" => 10f64.powf(n),
                "trunc" => n.trunc(),
                "cbrt" => n.cbrt(),
                "sin" => n.sin(),
                "cos" => n.cos(),
                "tan" => n.tan(),
                "asin" => n.asin(),
                "acos" => n.acos(),
                "atan" => n.atan(),
                "sinh" => n.sinh(),
                "cosh" => n.cosh(),
                "tanh" => n.tanh(),
                "significand" => {
                    if n == 0.0 {
                        0.0
                    } else {
                        n / 2f64.powi(n.abs().log2().floor() as i32)
                    }
                }
                "acosh" => n.acosh(),
                "asinh" => n.asinh(),
                "atanh" => n.atanh(),
                "expm1" => n.exp_m1(),
                "log1p" => n.ln_1p(),
                // `rint` and `nearbyint` round half to EVEN under the default
                // rounding mode, which is not `round`'s half-away-from-zero.
                "rint" | "nearbyint" => n.round_ties_even(),
                // SAFETY: each of these is a pure `double -> double` libm call.
                "lgamma" => unsafe { libm::lgamma(n) },
                // jq's `gamma` is `tgamma`, not the historical C alias for
                // `lgamma`: measured, `0.5 | gamma` is 1.7724538509055159.
                "gamma" | "tgamma" => unsafe { libm::tgamma(n) },
                "erf" => unsafe { libm::erf(n) },
                "erfc" => unsafe { libm::erfc(n) },
                "j0" => unsafe { libm::j0(n) },
                "j1" => unsafe { libm::j1(n) },
                "y0" => unsafe { libm::y0(n) },
                "y1" => unsafe { libm::y1(n) },
                _ => n.abs().log2().floor(),
            }))
        }
        // `frexp` and `modf` split a double into two parts, so they answer with a
        // two-element array rather than a number.
        ("frexp", 0) | ("modf", 0) | ("lgamma_r", 0) => {
            let n = input
                .as_f64()
                .ok_or_else(|| JqErr::msg(format!("{name} requires a number")))?;
            let (a, b) = match name {
                "frexp" => {
                    let mut e: i32 = 0;
                    // SAFETY: `e` is a live, correctly typed out-parameter.
                    let m = unsafe { libm::frexp(n, &mut e) };
                    (m, f64::from(e))
                }
                "modf" => {
                    let mut i: f64 = 0.0;
                    // SAFETY: same — one out-parameter, one return value.
                    let f = unsafe { libm::modf(n, &mut i) };
                    (f, i)
                }
                // `lgamma_r` is the reentrant `lgamma` — the log-magnitude plus
                // the SIGN of the gamma function. It is not exported on every
                // platform, so the sign is taken from `tgamma` directly, which is
                // what `signgam` records.
                _ => {
                    // SAFETY: pure libm calls on a plain double.
                    let (v, g) = unsafe { (libm::lgamma(n), libm::tgamma(n)) };
                    (v, if g < 0.0 { -1.0 } else { 1.0 })
                }
            };
            out(JqVal::arr(vec![JqVal::num(a), JqVal::num(b)]))
        }
        ("isfinite", 0) => out(JqVal::Bool(input.as_f64().is_some_and(f64::is_finite))),
        ("format", 1) => {
            let f = one(it, &args[0], input, env)?;
            let f = want_str(&f, "used as a format name")?;
            out(JqVal::str(apply_format(&f, input)?))
        }
        ("pow", 2)
        | ("atan2", 2)
        | ("fmin", 2)
        | ("fmax", 2)
        | ("ldexp", 2)
        | ("copysign", 2)
        | ("drem", 2)
        | ("fdim", 2)
        | ("fmod", 2)
        | ("hypot", 2)
        | ("nextafter", 2)
        | ("nexttoward", 2)
        | ("remainder", 2)
        | ("scalb", 2)
        | ("scalbln", 2)
        | ("jn", 2)
        | ("yn", 2) => {
            let a = num_required(&one(it, &args[0], input, env)?)?;
            let b = num_required(&one(it, &args[1], input, env)?)?;
            out(JqVal::num(match name {
                "pow" => a.powf(b),
                "atan2" => a.atan2(b),
                "fmin" => a.min(b),
                "fmax" => a.max(b),
                "hypot" => a.hypot(b),
                "copysign" => a.copysign(b),
                // SAFETY: pure libm calls on plain doubles.
                "drem" | "remainder" => unsafe { libm::remainder(a, b) },
                "fdim" => unsafe { libm::fdim(a, b) },
                "fmod" => unsafe { libm::fmod(a, b) },
                "nextafter" | "nexttoward" => unsafe { libm::nextafter(a, b) },
                "jn" => unsafe { libm::jn(a as i32, b) },
                "yn" => unsafe { libm::yn(a as i32, b) },
                // `ldexp`/`scalb`/`scalbln` all scale the FIRST argument by a
                // power of two. Measured against jq 1.8.2: `ldexp(2;3)` is 16 and
                // `scalb(3;2)` is 12, so both are `a * 2^b` — not C's
                // `ldexp(value, exp)` argument order.
                _ => a * 2f64.powi(b as i32),
            }))
        }
        ("fma", 3) => {
            let g = |i: usize| -> R<f64> { num_required(&one(it, &args[i], input, env)?) };
            out(JqVal::num(g(0)?.mul_add(g(1)?, g(2)?)))
        }
        ("infinite", 0) => out(JqVal::num(f64::INFINITY)),
        ("nan", 0) => out(JqVal::num(f64::NAN)),
        ("isnan", 0) => out(JqVal::Bool(input.as_f64().is_some_and(f64::is_nan))),
        ("isinfinite", 0) => out(JqVal::Bool(input.as_f64().is_some_and(f64::is_infinite))),
        ("isnormal", 0) => out(JqVal::Bool(input.as_f64().is_some_and(f64::is_normal))),

        // Each path is emitted as it is found, so one that is not intact refuses
        // AFTER the paths before it have gone out.
        ("path", 1) => eval_paths(
            it,
            &args[0],
            input,
            &Tracked::root(input),
            input,
            env,
            &mut |p, v| out(JqVal::arr(intact_path(p, &v)?)),
        ),
        ("getpath", 1) => eval(it, &args[0], input, env, &mut |p| {
            let JqVal::Arr(segs) = &p else {
                return Err(JqErr::msg("Path must be specified as an array"));
            };
            out(get_path(input, segs)?)
        }),
        ("setpath", 2) => {
            let p = one(it, &args[0], input, env)?;
            let v = one(it, &args[1], input, env)?;
            let JqVal::Arr(segs) = &p else {
                return Err(JqErr::msg("Path must be specified as an array"));
            };
            out(set_path(input, segs, v)?)
        }
        ("delpaths", 1) => {
            let p = one(it, &args[0], input, env)?;
            let JqVal::Arr(list) = &p else {
                return Err(JqErr::msg("Paths must be specified as an array"));
            };
            let mut paths = Vec::with_capacity(list.len());
            for e in list.iter() {
                match e {
                    JqVal::Arr(segs) => paths.push(segs.as_ref().clone()),
                    other => {
                        return Err(JqErr::msg(format!(
                            "Path must be specified as array, not {}",
                            other.type_name()
                        )))
                    }
                }
            }
            out(del_paths(input, paths)?)
        }
        ("_flatten", 1) => {
            let d = one(it, &args[0], input, env)?
                .as_f64()
                .ok_or_else(|| JqErr::msg("flatten depth must be a number"))?;
            if d < 0.0 {
                return Err(JqErr::msg("flatten depth must not be negative"));
            }
            // jq's `flatten` is `reduce .[] as $i (…)`, so its TOP level iterates
            // an object as well as an array (`{"a":[1,[2]]} | flatten` is
            // `[1,2]`); only the RECURSION is array-only. Same rule here, and the
            // refusal is the iterate error jq raises, not an array-only one.
            let top: Vec<JqVal> = match input.bare() {
                JqVal::Arr(a) => a.as_ref().clone(),
                JqVal::Obj(m) => m.iter().map(|(_, v)| v.clone()).collect(),
                other => {
                    return Err(JqErr::msg(format!(
                        "Cannot iterate over {}{}",
                        other.type_name(),
                        paren_of(other)
                    )))
                }
            };
            let mut res = Vec::new();
            flatten_into(&top, d as i64, &mut res);
            out(JqVal::arr(res))
        }

        ("_match_impl", 3) => {
            let re = one(it, &args[0], input, env)?;
            let flags = one(it, &args[1], input, env)?;
            let testmode = one(it, &args[2], input, env)?;
            out(regex_match(input, &re, &flags, testmode.truthy())?)
        }
        ("splits_impl", 2) | ("_split_re", 2) => {
            let re = one(it, &args[0], input, env)?;
            let flags = one(it, &args[1], input, env)?;
            out(regex_split(input, &re, &flags)?)
        }
        ("sub_impl", 3) => {
            // The replacement is a FILTER run with the capture object as `.`, so
            // it must stay unevaluated until each match is known.
            let re = one(it, &args[0], input, env)?;
            let flags = one(it, &args[2], input, env)?;
            regex_sub(it, input, &re, &args[1], &flags, env, out)
        }

        ("env", 0) => out(it.env_object()),
        // `gen_builtin_list` lists with `block_list_funcs(builtins, 1)`:
        // names starting with `_` are internal and omitted.
        ("builtins", 0) => out(JqVal::arr(
            builtin_names()
                .into_iter()
                .filter(|n| !n.starts_with('_'))
                .map(JqVal::str)
                .collect(),
        )),
        ("input", 0) => match it.next_input() {
            Some(v) => out(v),
            // jq 1.8's `f_input` raises the bare word `break`, which is what
            // its `def inputs` catches.
            None => Err(JqErr::msg("break")),
        },
        ("inputs", 0) => loop {
            match it.next_input() {
                Some(v) => out(v)?,
                None => return Ok(()),
            }
        },
        ("input_line_number", 0) => out(JqVal::num(it.line.get())),
        ("debug", 0) => {
            eprintln!("[\"DEBUG:\",{}]", render(input));
            out(input.clone())
        }
        ("debug", 1) => {
            eval(it, &args[0], input, env, &mut |m| {
                eprintln!("[\"DEBUG:\",{}]", render(&m));
                Ok(())
            })?;
            out(input.clone())
        }
        ("stderr", 0) => {
            eprint!("{}", render(input));
            out(input.clone())
        }
        ("halt", 0) => Err(JqErr::Halt(0, None)),
        ("halt_error", 0) => Err(JqErr::Halt(5, Some(input.clone()))),
        ("halt_error", 1) => {
            let code = one(it, &args[0], input, env)?
                .as_f64()
                .ok_or_else(|| JqErr::msg("halt_error/1: number required"))?;
            Err(JqErr::Halt(code as i32, Some(input.clone())))
        }
        ("input_filename", 0) => out(JqVal::Null),
        // jq's module-system introspection. arb has no jq module search path —
        // its own `import` is the arb preset system — so the two path builtins
        // report where the program came from and the search list is empty.
        ("get_jq_origin", 0) => out(JqVal::str("arb")),
        ("get_prog_origin", 0) => out(JqVal::str(".")),
        ("get_search_list", 0) => out(JqVal::arr(Vec::new())),
        ("modulemeta", 0) => Err(JqErr::msg(format!(
            "module not found: {}",
            render_raw(input)
        ))),
        ("have_literal_numbers", 0) => out(JqVal::Bool(true)),
        // jq 1.8.2 is built with decNumber, and arb keeps its number model:
        // literals survive unmodified values, negate exactly and compare as
        // decimals (`num_from_literal`, `negate_num`, `cmp_decimal`).
        ("have_decnum", 0) => out(JqVal::Bool(true)),
        ("$__loc__", 0) => out(JqVal::obj(vec![
            (Rc::from("file"), JqVal::str("<top-level>")),
            (Rc::from("line"), JqVal::num(1.0)),
        ])),

        ("now", 0) => out(JqVal::num(unix_now())),
        ("mktime", 0) => out(JqVal::num(mktime(input)? as f64)),
        ("gmtime", 0) | ("localtime", 0) => {
            let t = input
                .as_f64()
                .ok_or_else(|| JqErr::msg(format!("{name}() requires numeric inputs")))?;
            out(broken_down(t, name == "localtime")?)
        }
        ("strftime", 1) | ("strflocaltime", 1) => {
            let f = one(it, &args[0], input, env)?;
            let f = str_or(&f, &format!("{name}/1 requires a string format"))?;
            out(JqVal::str(strftime_val(
                input,
                &f,
                name == "strflocaltime",
            )?))
        }
        ("strptime", 1) => {
            let f = one(it, &args[0], input, env)?;
            let f = str_or(&f, "strptime/1 requires string inputs and arguments")?;
            let s = str_or(input, "strptime/1 requires string inputs and arguments")?;
            out(strptime_val(&s, &f)?)
        }

        _ => Err(JqErr::msg(format!("{name}/{} is not defined", args.len()))),
    }
}

fn want_arr(v: &JqVal, who: &str) -> R<Rc<Vec<JqVal>>> {
    match v.bare() {
        JqVal::Arr(a) => Ok(a.clone()),
        other => Err(JqErr::msg(format!(
            "{}{} cannot be {who}, as it is not an array",
            other.type_name(),
            paren_of(other)
        ))),
    }
}

/// Pair every element of the input array with `[f]` evaluated over it — the key
/// array jq's `_sort_by_impl` family sorts on. An OBJECT survives jq's
/// `map([f])` (it maps the values) and is refused by the C implementation with
/// both operands named: `pair_msg` is that implementation's wording.
fn keyed_elements(
    it: &Interp,
    f: &Filter,
    input: &JqVal,
    env: &Env,
    pair_msg: &str,
) -> R<Vec<(JqVal, JqVal)>> {
    // jq runs `map([f])` first, so a scalar input fails as an ITERATION.
    if !matches!(input.bare(), JqVal::Arr(_) | JqVal::Obj(_)) {
        let b = input.bare();
        return Err(JqErr::msg(format!(
            "Cannot iterate over {}{}",
            b.type_name(),
            paren_of(b)
        )));
    }
    if let JqVal::Obj(m) = input.bare() {
        let mut keys = Vec::with_capacity(m.len());
        for (_, e) in m.iter() {
            let mut key = Vec::new();
            eval(it, f, e, env, &mut |k| {
                key.push(k);
                Ok(())
            })?;
            keys.push(JqVal::arr(key));
        }
        let keys = JqVal::arr(keys);
        return Err(JqErr::msg(format!(
            "{}{} and {}{} {pair_msg}",
            input.bare().type_name(),
            paren_of(input),
            keys.type_name(),
            paren_of(&keys)
        )));
    }
    let a = want_arr(input, "sorted")?;
    let mut keyed = Vec::with_capacity(a.len());
    for e in a.iter() {
        let mut key = Vec::new();
        eval(it, f, e, env, &mut |k| {
            key.push(k);
            Ok(())
        })?;
        keyed.push((JqVal::arr(key), e.clone()));
    }
    Ok(keyed)
}

fn flatten_into(a: &[JqVal], depth: i64, out: &mut Vec<JqVal>) {
    for e in a {
        // Whether an element RECURSES is about its value; what comes out is the
        // element itself, box and all.
        match e.bare() {
            JqVal::Arr(inner) if depth > 0 => flatten_into(inner, depth - 1, out),
            _ => out.push(e.clone()),
        }
    }
}

/// jq's `contains` builtin (`f_contains`, src/builtin.c): the two TOP-LEVEL
/// values must share a jv kind — `true` and `false` are DIFFERENT kinds there,
/// so `true | contains(false)` raises — and only then is the recursive check run.
fn contains(a: &JqVal, b: &JqVal) -> R<bool> {
    let (a, b) = (a.bare(), b.bare());
    if jv_kind(a) != jv_kind(b) {
        return Err(JqErr::msg(format!(
            "{}{} and {}{} cannot have their containment checked",
            a.type_name(),
            paren_of(a),
            b.type_name(),
            paren_of(b)
        )));
    }
    contains_at(a, b, 0).ok_or_else(|| JqErr::msg("Containment check too deep"))
}

/// jq's `MAX_CONTAINS_DEPTH` (src/jv.c).
const MAX_CONTAINS_DEPTH: usize = 10_000;

/// Port of `jvp_contains` (src/jv.c). NESTED values of different kinds are
/// simply not contained — no error below the top level. `None` is jq's `-1`
/// ("too deep").
fn contains_at(a: &JqVal, b: &JqVal, depth: usize) -> Option<bool> {
    if depth > MAX_CONTAINS_DEPTH {
        return None;
    }
    let (a, b) = (a.bare(), b.bare());
    if jv_kind(a) != jv_kind(b) {
        return Some(false);
    }
    match (a, b) {
        // `jvp_object_contains`: every key of `b` must be present in `a` with a
        // containing value; a missing key reads as jq's INVALID, which no kind
        // matches.
        (JqVal::Obj(_), JqVal::Obj(bm)) => {
            for (k, bv) in bm.iter() {
                match a.obj_get(k) {
                    Some(av) => match contains_at(av, bv, depth + 1) {
                        Some(true) => {}
                        r => return r,
                    },
                    None => return Some(false),
                }
            }
            Some(true)
        }
        // `jvp_array_contains`: every element of `b` must be contained by SOME
        // element of `a`; a too-deep answer stops the scan.
        (JqVal::Arr(aa), JqVal::Arr(ba)) => {
            for bv in ba.iter() {
                let mut hit = Some(false);
                for av in aa.iter() {
                    hit = contains_at(av, bv, depth + 1);
                    if hit != Some(false) {
                        break;
                    }
                }
                if hit != Some(true) {
                    return hit;
                }
            }
            Some(true)
        }
        (JqVal::Str(x), JqVal::Str(y)) => Some(x.contains(&**y)),
        (x, y) => Some(eq_vals(x, y)),
    }
}

/// jq's `jv_kind`: like `type`, except `true` and `false` are distinct kinds.
fn jv_kind(v: &JqVal) -> (&'static str, bool) {
    (v.type_name(), matches!(v.bare(), JqVal::Bool(true)))
}
// ─────────────────────────────────────────────────────────────────────────────
// Regex builtins
// ─────────────────────────────────────────────────────────────────────────────

/// Compiled engines keyed by (pattern, translated flags), with the compile
/// FAILURE cached alongside so a bad pattern raises on every call.
type ReCache = std::collections::HashMap<(String, String), Result<Rc<regex::Regex>, String>>;

/// Compile a jq regex + flag string. jq's flags are Oniguruma's; the ones with a
/// `regex`-crate equivalent are translated and the rest are refused by name
/// rather than ignored, so a program never silently gets different matching.
fn compile_re(pat: &str, flags: &str) -> R<(Rc<regex::Regex>, bool)> {
    thread_local! {
        /// Compiled engines by (pattern, flags).
        ///
        /// A regex builtin runs once PER RECORD, so without this a
        /// `scan("[0-9]+")` over a stream re-parses and re-compiles the same
        /// pattern for every line. Measured over 50,000 records:
        /// `[.msg | scan("[0-9]+")]` took 1.873s against `jq`'s 0.365s, and
        /// `select(.msg | test("payload"))` 0.376s against 0.234s.
        ///
        /// The FAILURE is cached alongside the engine, so an invalid pattern
        /// still errors on every call rather than only on the first.
        static RE_CACHE: RefCell<ReCache> = RefCell::new(ReCache::new());
    }
    let mut global = false;
    let mut prefix = String::new();
    for f in flags.chars() {
        match f {
            'g' => global = true,
            'i' => prefix.push('i'),
            'x' => prefix.push('x'),
            // Oniguruma's SINGLELINE: `^`/`$` anchor to the whole string,
            // which is already this engine's default — so `s` adds nothing
            // (it is NOT dot-matches-newline; that is `m`, Oniguruma's
            // MULTILINE). `p` is both, i.e. just dot-all here.
            's' => {}
            'm' | 'p' => prefix.push('s'),
            'n' => {}
            'l' => {}
            other => {
                return Err(JqErr::msg(format!(
                    "{other} is not a valid modifier string"
                )))
            }
        }
    }
    let key = (pat.to_string(), prefix.clone());
    let cached = RE_CACHE.with(|c| c.borrow().get(&key).cloned());
    let entry = match cached {
        Some(e) => e,
        None => {
            let src = if prefix.is_empty() {
                pat.to_string()
            } else {
                format!("(?{prefix}){pat}")
            };
            let e = regex::Regex::new(&src)
                .map(Rc::new)
                .map_err(|e| format!("{pat} (while regex-compiling): {e}"));
            RE_CACHE.with(|c| c.borrow_mut().insert(key, e.clone()));
            e
        }
    };
    // Per-MATCH state stays unshared: `regex::Regex` is `Sync` and every search
    // allocates its own captures, so sharing the compiled engine shares only the
    // immutable program.
    entry.map(|re| (re, global)).map_err(JqErr::msg)
}

/// Byte offset -> code-point offset. jq reports both offsets and lengths in code
/// points, so every byte index a `regex` match reports has to be converted.
fn cp_index(s: &str, byte: usize) -> usize {
    s[..byte].chars().count()
}

/// `f_match`'s argument checks (src/builtin.c): the regex and the modifiers
/// each refuse a non-string with `<type> (<value>) is not a string`; null
/// modifiers mean none.
fn re_args(re: &JqVal, flags: &JqVal) -> R<(Rc<str>, String)> {
    let not_string =
        |v: &JqVal| JqErr::msg(format!("{}{} is not a string", v.type_name(), paren_of(v)));
    let pat = match re.bare() {
        JqVal::Str(s) => s.clone(),
        other => return Err(not_string(other)),
    };
    let fl = match flags.bare() {
        JqVal::Null => String::new(),
        JqVal::Str(s) => s.to_string(),
        other => return Err(not_string(other)),
    };
    Ok((pat, fl))
}

/// jq's `_match_impl`: an array of match objects, or a boolean in test mode.
/// jq's match loop, transcribed from `f_match`: search from `start`, and after
/// a match resume at its END, or one character past it when it was empty
/// (`start <= end`, so an empty match AT the end of the string still counts).
/// This differs from `Regex::captures_iter`, which refuses an empty match
/// adjacent to the previous one: jq's `"aaa" | gsub("a*";"X")` is `XX` and
/// `"baaab" | gsub("a*";"X")` is `XbXXbX`. Measured against jq 1.8.2.
fn jq_match_iter<'h>(rx: &regex::Regex, s: &'h str, global: bool) -> Vec<regex::Captures<'h>> {
    let mut hits = Vec::new();
    let mut start = 0usize;
    while start <= s.len() {
        let Some(caps) = rx.captures_at(s, start) else {
            break;
        };
        let m = caps.get(0).expect("group 0 always participates");
        start = if m.is_empty() {
            m.end() + s[m.end()..].chars().next().map_or(1, char::len_utf8)
        } else {
            m.end()
        };
        hits.push(caps);
        if !global {
            break;
        }
    }
    hits
}

fn regex_match(input: &JqVal, re: &JqVal, flags: &JqVal, testmode: bool) -> R<JqVal> {
    let s = match input {
        JqVal::Str(s) => s.clone(),
        other => {
            return Err(JqErr::msg(format!(
                "{}{} cannot be matched, as it is not a string",
                other.type_name(),
                paren_of(other)
            )))
        }
    };
    let (pat, fl) = re_args(re, flags)?;
    let (rx, global) = compile_re(&pat, &fl)?;
    if testmode {
        return Ok(JqVal::Bool(rx.is_match(&s)));
    }
    let names: Vec<Option<&str>> = rx.capture_names().collect();
    let mut hits = Vec::new();
    for caps in jq_match_iter(&rx, &s, global) {
        let whole = caps.get(0).expect("group 0 always participates");
        let zero_width = whole.is_empty();
        let mut cap_list = Vec::new();
        for (gi, name) in names.iter().enumerate().skip(1) {
            let name = (Rc::from("name"), name.map_or(JqVal::Null, JqVal::str));
            // jq builds a non-participating group's object in a different key
            // order (`offset, string, length`) than a matched one, and key
            // order is visible in the output.
            cap_list.push(JqVal::obj(match caps.get(gi) {
                // A ZERO-WIDTH match builds a participating group in the
                // non-participating order too (f_match's zero-width branch).
                Some(m) if zero_width => vec![
                    (
                        Rc::from("offset"),
                        JqVal::num(cp_index(&s, m.start()) as f64),
                    ),
                    (Rc::from("string"), JqVal::str("")),
                    (Rc::from("length"), JqVal::num(0.0)),
                    name,
                ],
                Some(m) => vec![
                    (
                        Rc::from("offset"),
                        JqVal::num(cp_index(&s, m.start()) as f64),
                    ),
                    (
                        Rc::from("length"),
                        JqVal::num(m.as_str().chars().count() as f64),
                    ),
                    (Rc::from("string"), JqVal::str(m.as_str())),
                    name,
                ],
                None => vec![
                    (Rc::from("offset"), JqVal::num(-1.0)),
                    (Rc::from("string"), JqVal::Null),
                    (Rc::from("length"), JqVal::num(0.0)),
                    name,
                ],
            }));
        }
        hits.push(JqVal::obj(vec![
            (
                Rc::from("offset"),
                JqVal::num(cp_index(&s, whole.start()) as f64),
            ),
            (
                Rc::from("length"),
                JqVal::num(whole.as_str().chars().count() as f64),
            ),
            (Rc::from("string"), JqVal::str(whole.as_str())),
            (Rc::from("captures"), JqVal::arr(cap_list)),
        ]));
    }
    Ok(JqVal::arr(hits))
}

/// jq's regex `split/2`: the pieces BETWEEN matches, always global. jq spells
/// it `match($re; $flags + "g")`, so the flags are joined with jq's `+` (and
/// refused by it) before the input and the regex are checked.
fn regex_split(input: &JqVal, re: &JqVal, flags: &JqVal) -> R<JqVal> {
    let flags = binop(BinOp::Add, flags, &JqVal::str("g"))?;
    let s = want_str(input, "matched, as it is not a string")?;
    let (pat, fl) = re_args(re, &flags)?;
    let (rx, _) = compile_re(&pat, &fl)?;
    let mut parts = Vec::new();
    let mut last = 0usize;
    for caps in jq_match_iter(&rx, &s, true) {
        let m = caps.get(0).expect("group 0 always participates");
        parts.push(JqVal::str(&s[last..m.start()]));
        last = m.end();
    }
    parts.push(JqVal::str(&s[last..]));
    Ok(JqVal::arr(parts))
}

/// jq's `sub`/`gsub`, transcribed from jq 1.8's `def sub($re; s; $flags)`. The
/// replacement is a FILTER evaluated with the named-capture object as `.`, and
/// it is a generator — but its outputs are combined POSITIONALLY across
/// matches, not as a cartesian product: output `k` of every match feeds result
/// `k`, so `"aaa" | [gsub("a"; "b","c")]` is `["bbb","ccc"]`. A match that yields
/// fewer outputs than an earlier one leaves the extra results un-extended, and
/// no result at all falls back to the input (jq's trailing `// $in`).
fn regex_sub(
    it: &Interp,
    input: &JqVal,
    re: &JqVal,
    repl: &Filter,
    flags: &JqVal,
    env: &Env,
    out: Sink,
) -> R<()> {
    let s = want_str(input, "matched, as it is not a string")?;
    let (pat, fl) = re_args(re, flags)?;
    let (rx, global) = compile_re(&pat, &fl)?;
    let names: Vec<Option<String>> = rx.capture_names().map(|n| n.map(str::to_string)).collect();
    let mut spans = Vec::new();
    for caps in jq_match_iter(&rx, &s, global) {
        let whole = caps.get(0).expect("group 0 always participates");
        let mut obj = Vec::new();
        for (gi, name) in names.iter().enumerate().skip(1) {
            if let Some(n) = name {
                obj.push((
                    Rc::from(n.as_str()),
                    caps.get(gi).map_or(JqVal::Null, |m| JqVal::str(m.as_str())),
                ));
            }
        }
        spans.push((whole.start(), whole.end(), JqVal::obj(obj)));
    }
    let mut results: Vec<String> = Vec::new();
    let mut previous = 0usize;
    for (start, end, caps) in &spans {
        let gap = &s[previous..*start];
        let mut inserts = Vec::new();
        eval(it, repl, caps, env, &mut |r| {
            inserts.push(r);
            Ok(())
        })?;
        // jq joins with `$gap + $inserts[$ix]`, so a non-string replacement
        // fails as that ADDITION does (`string ("") and number (1) cannot be
        // added`); a successful `string + x` is always a string.
        for (ix, r) in inserts.iter().enumerate() {
            let piece = render_raw(&binop(BinOp::Add, &JqVal::str(gap), r)?);
            match results.get_mut(ix) {
                Some(acc) => acc.push_str(&piece),
                None => results.push(piece),
            }
        }
        previous = *end;
    }
    if results.is_empty() {
        return out(input.clone());
    }
    for r in results {
        out(JqVal::str(format!("{r}{}", &s[previous..])))?;
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Date builtins
// ─────────────────────────────────────────────────────────────────────────────

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// jq's `tm2jv`: `[year, month0, mday, hour, min, sec, wday, yday]`, where
/// `sec` carries the sub-second fraction `fsecs - floor(fsecs)`.
fn tm2jv(tm: &libc::tm, fsecs: f64) -> JqVal {
    JqVal::arr(vec![
        JqVal::num(f64::from(tm.tm_year) + 1900.0),
        JqVal::num(f64::from(tm.tm_mon)),
        JqVal::num(f64::from(tm.tm_mday)),
        JqVal::num(f64::from(tm.tm_hour)),
        JqVal::num(f64::from(tm.tm_min)),
        JqVal::num(f64::from(tm.tm_sec) + (fsecs - fsecs.floor())),
        JqVal::num(f64::from(tm.tm_wday)),
        JqVal::num(f64::from(tm.tm_yday)),
    ])
}

/// jq's `f_gmtime`/`f_localtime`. The whole seconds are the input TRUNCATED
/// (`time_t secs = fsecs`), and the fraction is taken against the floor, so
/// `-1.5` is `23:59:59.5`, as jq answers.
fn broken_down(t: f64, local: bool) -> R<JqVal> {
    let tt = t as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        if local {
            !libc::localtime_r(&tt, &mut tm).is_null()
        } else {
            !libc::gmtime_r(&tt, &mut tm).is_null()
        }
    };
    if !ok {
        return Err(JqErr::msg(
            "error converting number of seconds since epoch to datetime",
        ));
    }
    Ok(tm2jv(&tm, t))
}

/// jq's `jv2tm`: up to eight numeric fields in `tm` order — a missing one stays
/// 0, each is clamped to `int` — then normalized the way jq does, through
/// `timegm` for UTC or `mktime` (DST unknown) for local time. `None` when a
/// field is not a number, or is NaN.
fn jv2tm(v: &JqVal, local: bool) -> Option<libc::tm> {
    let JqVal::Arr(a) = v.bare() else {
        return None;
    };
    let mut f = [0i32; 8];
    for (i, (slot, x)) in f.iter_mut().zip(a.iter()).enumerate() {
        let d = x.as_f64().filter(|d| !d.is_nan())?;
        // The year is offset BEFORE the clamp; `as` saturates, which is jq's
        // INT_MIN/INT_MAX clamp.
        *slot = if i == 0 { d - 1900.0 } else { d } as i32;
    }
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = f[0];
    tm.tm_mon = f[1];
    tm.tm_mday = f[2];
    tm.tm_hour = f[3];
    tm.tm_min = f[4];
    tm.tm_sec = f[5];
    tm.tm_wday = f[6];
    tm.tm_yday = f[7];
    unsafe {
        if local {
            tm.tm_isdst = -1;
            libc::mktime(&mut tm);
        } else {
            libc::timegm(&mut tm);
        }
    }
    Some(tm)
}

fn mktime(v: &JqVal) -> R<i64> {
    if !matches!(v, JqVal::Arr(_)) {
        return Err(JqErr::msg("mktime requires array inputs"));
    }
    let mut tm =
        jv2tm(v, false).ok_or_else(|| JqErr::msg("mktime requires parsed datetime inputs"))?;
    // `timegm` is the UTC counterpart of `mktime`; jq uses it so a broken-down
    // time round-trips through `gmtime` exactly.
    match unsafe { libc::timegm(&mut tm) } {
        -1 => Err(JqErr::msg("invalid gmtime representation")),
        t => Ok(t),
    }
}

/// Expand the directives libc would answer from the process's LOCAL zone, for
/// `strftime`, which formats UTC: `%Z`/`%z` read `UTC`/`+0000` and `%s` is the
/// UTC epoch. jq gets the same by switching `TZ` to UTC around the call on
/// macOS; changing the environment is not thread-safe here, so the three are
/// expanded instead. `%%` and every other directive are left for libc.
fn utc_zone_directives(fmt: &str, epoch: i64) -> String {
    let mut out = String::with_capacity(fmt.len());
    let mut cs = fmt.chars();
    while let Some(c) = cs.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match cs.next() {
            Some('Z') => out.push_str("UTC"),
            Some('z') => out.push_str("+0000"),
            Some('s') => out.push_str(&epoch.to_string()),
            Some(d) => {
                out.push('%');
                out.push(d);
            }
            None => out.push('%'),
        }
    }
    out
}

/// jq's `f_strftime`/`f_strflocaltime`: a number goes through `gmtime` (or
/// `localtime`) first, an array through `jv2tm`.
fn strftime_val(v: &JqVal, fmt: &str, local: bool) -> R<String> {
    let name = if local { "strflocaltime" } else { "strftime" };
    let bad_input = || JqErr::msg(format!("{name}/1 requires parsed datetime inputs"));
    let tm = match v.bare() {
        JqVal::Num(n, _) => jv2tm(&broken_down(*n, local)?, local),
        JqVal::Arr(_) => jv2tm(v, local),
        _ => return Err(bad_input()),
    }
    .ok_or_else(bad_input)?;
    let fmt = if local {
        fmt.to_string()
    } else {
        utc_zone_directives(fmt, unsafe { libc::timegm(&mut tm.clone()) })
    };
    let cfmt = std::ffi::CString::new(fmt).map_err(|_| JqErr::msg("bad format string"))?;
    let mut buf = vec![0u8; 512];
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), cfmt.as_ptr(), &tm) };
    buf.truncate(n);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// jq's `set_tm_wday`: Gauss's day-of-week, from year, month and day alone.
fn set_tm_wday(tm: &mut libc::tm) {
    let century = (1900 + tm.tm_year) / 100;
    let mut year = (1900 + tm.tm_year) % 100;
    if tm.tm_mon < 2 {
        year -= 1;
    }
    // March is 1, …, January 11, February 12.
    let mut mon = tm.tm_mon - 1;
    if mon < 1 {
        mon += 12;
    }
    let mut wday = (tm.tm_mday
        + (2.6 * f64::from(mon) - 0.2).floor() as i32
        + year
        + (f64::from(year) / 4.0).floor() as i32
        + (f64::from(century) / 4.0).floor() as i32
        - 2 * century)
        % 7;
    if wday < 0 {
        wday += 7;
    }
    tm.tm_wday = wday;
}

/// jq's `set_tm_yday`: the day of the year from month and day, with jq's
/// bounds folding of an out-of-range month.
fn set_tm_yday(tm: &mut libc::tm) {
    const D: [i32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let year = 1900 + tm.tm_year;
    let leap = tm.tm_mon > 1 && ((year % 4 == 0 && year % 100 != 0) || year % 400 == 0);
    let mut mon = tm.tm_mon.abs();
    if mon > 11 {
        mon %= 12;
    }
    tm.tm_yday = D[mon as usize] + i32::from(leap) + tm.tm_mday - 1;
}

/// jq's `f_strptime`. Fields the format does not set stay as jq zeroes them
/// (`"10:30" | strptime("%H:%M")` is year 1900, month 0, day 0), and wday/yday
/// are always derived from year/month/day — jq's macOS branch, which this
/// follows on every platform so the answer does not depend on the libc. Input
/// left over after the format is allowed when it starts with whitespace, and is
/// appended to the result as a string, as jq does.
fn strptime_val(s: &str, fmt: &str) -> R<JqVal> {
    let cs = std::ffi::CString::new(s).map_err(|_| JqErr::msg("bad date string"))?;
    let cf = std::ffi::CString::new(fmt).map_err(|_| JqErr::msg("bad format string"))?;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let end = unsafe { libc::strptime(cs.as_ptr(), cf.as_ptr(), &mut tm) };
    let rest = (!end.is_null()).then(|| {
        unsafe { std::ffi::CStr::from_ptr(end) }
            .to_string_lossy()
            .into_owned()
    });
    let Some(rest) =
        rest.filter(|r| r.starts_with([' ', '\t', '\n', '\x0b', '\x0c', '\r']) || r.is_empty())
    else {
        return Err(JqErr::msg(format!(
            "date \"{s}\" does not match format \"{fmt}\""
        )));
    };
    set_tm_wday(&mut tm);
    set_tm_yday(&mut tm);
    let JqVal::Arr(mut a) = tm2jv(&tm, 0.0) else {
        unreachable!("tm2jv builds an array")
    };
    if !rest.is_empty() {
        Rc::make_mut(&mut a).push(JqVal::str(rest));
    }
    Ok(JqVal::Arr(a))
}

// ─────────────────────────────────────────────────────────────────────────────
// Prelude — jq's own `src/builtin.jq`, transcribed
//
// These are the builtins jq itself writes in jq rather than in C. Keeping them
// as jq source (rather than re-deriving them in Rust) is what makes the corner
// cases match: `from_entries`' six accepted key spellings, `limit`'s early
// `break`, `walk`'s post-order, `tostream`'s path trick. Definitions are ordered
// so each one only refers to those above it, which is what the environment chain
// makes visible.
// ─────────────────────────────────────────────────────────────────────────────

const PRELUDE: &str = r#"
def error(msg): msg|error;
def halt_error: halt_error(5);
def map(f): [.[] | f];
def select(f): if f then . else empty end;
def recurse(f): def r: ., (f | r); r;
def recurse(f; cond): def r: ., (f | select(cond) | r); r;
def recurse: recurse(.[]?);
def values: select(. != null);
def nulls: select(. == null);
def booleans: select(type == "boolean");
def numbers: select(type == "number");
def strings: select(type == "string");
def arrays: select(type == "array");
def objects: select(type == "object");
def iterables: select(type |. == "array" or . == "object");
def scalars: select(type |. != "array" and . != "object");
def finites: select(type == "number" and (isinfinite or isnan | not));
def normals: select(isnormal);
def to_entries: [keys_unsorted[] as $k | {key: $k, value: .[$k]}];
def from_entries: reduce .[] as $x ({};
  . + { ($x | .key // .Key // .name // .Name):
        ($x | if has("value") then .value elif has("Value") then .Value else null end) });
def with_entries(f): to_entries | map(f) | from_entries;
def add: reduce .[] as $x (null; . + $x);
def add(f): reduce f as $x (null; . + $x);
def join($x): reduce .[] as $i (null;
    (if . == null then "" else . + $x end) +
    ($i | if . == null then "" elif type == "boolean" or type == "number" then tojson else . end)) // "";
def flatten: _flatten(1e9);
def flatten($x): _flatten($x);
def ltrimstr($left): if startswith($left) then .[($left|length):] else . end;
def rtrimstr($right): if endswith($right) then .[:length - ($right|length)] else . end;
def range($x): range(0; $x);
def isempty(g): label $go | (g|false, break $go), true;
def first(f): label $out | (f | ., break $out);
def first: .[0];
def last(f): reduce f as $x (null; [$x]) | values | .[0];
def last: .[-1];
def any: reduce .[] as $x (false; . or $x);
def all: reduce .[] as $x (true; . and $x);
def any(y): reduce (.[]|y) as $x (false; . or $x);
def all(y): reduce (.[]|y) as $x (true; . and $x);
def any(g; y): isempty(first(g|select(y))) | not;
def all(g; y): isempty(first(g|y|select(.|not)));
def limit($n; f): if $n > 0 then label $out | foreach f as $item ($n; . - 1; $item, if . <= 0 then break $out else empty end)
                  elif $n == 0 then empty
                  else error("limit doesn't support negative count") end;
def skip($n; f): if $n > 0 then foreach f as $item ($n; . - 1; if . < 0 then $item else empty end)
                 elif $n == 0 then f
                 else error("skip doesn't support negative count") end;
def nth($n): .[$n];
def nth($n; f): if $n < 0 then error("nth doesn't support negative indices") else first(skip($n; f)) end;
def until(cond; update): def _until: if cond then . else (update | _until) end; _until;
def while(cond; update): def _while: if cond then ., (update | _while) else empty end; _while;
def repeat(f): def _repeat: f, _repeat; _repeat;
def in(xs): . as $x | xs | has($x);
def inside(xs): . as $x | xs | contains($x);
def combinations: if length == 0 then [] else .[0][] as $x | (.[1:] | combinations) as $w | [$x] + $w end;
def combinations(n): . as $dot | [range(n)] | map($dot) | combinations;
def map_values(f): .[] |= f;
def walk(f): def w: if type == "object" then map_values(w) elif type == "array" then map(w) else . end | f; w;
def del(f): delpaths([path(f)]);
def paths: path(..) | select(length > 0);
def paths(node_filter): path(..|select(node_filter)) | select(length > 0);
def leaf_paths: paths(scalars);
def pick(pathexps): . as $top | reduce path(pathexps) as $p (null; setpath($p; $top | getpath($p)));
def transpose: if . == [] then [] else . as $in | (map(length) | max) as $max
  | [range(0; $max) as $j | [range(0; $in|length) as $i | $in[$i][$j]]] end;
def env: $ENV;
def isfinite: type == "number" and (isinfinite | not);
def trimstr($val): ltrimstr($val) | rtrimstr($val);
def toboolean: if type == "boolean" then .
  elif type == "string" and (. == "true" or . == "false") then . == "true"
  else error("\(type) (\(tojson)) cannot be parsed as a boolean") end;
def JOIN($idx; idx_expr): [.[] | [., $idx[idx_expr]]];
def JOIN($idx; stream; idx_expr): stream | [., $idx[idx_expr]];
def JOIN($idx; stream; idx_expr; join_expr): stream | [., $idx[idx_expr]] | join_expr;
def bsearch($target):
  if type != "array" then error("\(type) (\(tojson)) cannot be searched from")
  elif length == 0 then -1
  elif length == 1 then (if $target > .[0] then -2 elif $target == .[0] then 0 else -1 end)
  else . as $in
    | (length - 1) as $rhs
    | [0, $rhs]
    | until(.[0] > .[1];
        (((.[1] + .[0]) / 2) | floor) as $mid
        | $in[$mid] as $monkey
        | if $monkey == $target then [$mid, $mid - 1]
          elif $monkey < $target then [($mid + 1), .[1]]
          else [.[0], ($mid - 1)] end)
    | if $in[.[0]] == $target then .[0]
      elif .[0] > $rhs then (-2 - $rhs)
      else (-1 - .[0]) end
  end;
def toarray: if type == "array" then . else [.] end;
def abs: if . < 0 then - . else . end;
def isvalid(f): try (f|true) catch false;
def indices($i): if type == "array" and ($i|type) == "array" then .[$i]
                 elif type == "array" then .[[$i]]
                 elif type == "string" and ($i|type) == "string" then _strindices($i)
                 else .[$i] end;
def index($i): indices($i) | .[0];
def rindex($i): indices($i) | .[-1:][0];
def tostream: path(def r: (.[]?|r), .; r) as $p | getpath($p)
  | reduce path(.[]?) as $q ([$p, .]; [$p+$q]);
def fromstream(f): { x: null, e: false } as $init
  | foreach f as $i ($init;
      if .e then $init else . end
      | if $i | length == 2 then setpath(["e"]; $i[0] | length == 0) | setpath(["x"] + $i[0]; $i[1])
        else setpath(["e"]; $i[0] | length == 1) end;
      if .e then .x else empty end);
def truncate_stream(stream): . as $n | null | stream | . as $input
  | if (.[0]|length) > $n then setpath([0]; .[0][$n:]) else empty end;
def match($regex; $flags): _match_impl($regex; $flags; false) | .[];
def match($val): ($val|type) as $vt
  | if $vt == "string" then match($val; null)
    elif $vt == "array" and ($val|length) > 1 then match($val[0]; $val[1])
    elif $vt == "array" and ($val|length) > 0 then match($val[0]; null)
    else error($vt + " not a string or array") end;
def test($regex; $flags): _match_impl($regex; $flags; true);
def test($val): ($val|type) as $vt
  | if $vt == "string" then test($val; null)
    elif $vt == "array" and ($val|length) > 1 then test($val[0]; $val[1])
    elif $vt == "array" and ($val|length) > 0 then test($val[0]; null)
    else error($vt + " not a string or array") end;
def capture($re; $flags): match($re; $flags)
  | reduce (.captures | .[] | select(.name != null) | { (.name): .string }) as $pair ({}; . + $pair);
def capture($val): ($val|type) as $vt
  | if $vt == "string" then capture($val; null)
    elif $vt == "array" and ($val|length) > 1 then capture($val[0]; $val[1])
    elif $vt == "array" and ($val|length) > 0 then capture($val[0]; null)
    else error($vt + " not a string or array") end;
def scan($re; $flags): match($re; "g" + $flags)
  | if (.captures | length) > 0 then [.captures | .[] | .string] else .string end;
def scan($re): scan($re; null);
def split($re; $flags): _split_re($re; $flags);
def splits($re; $flags): split($re; $flags) | .[];
def splits($re): splits($re; null);
def sub($re; str): sub_impl($re; str; "");
def sub($re; str; $flags): sub_impl($re; str; $flags);
def gsub($re; str): sub_impl($re; str; "g");
def gsub($re; str; $flags): sub_impl($re; str; $flags + "g");
def ascii(i): [i] | implode;
def todate(f): strftime(f);
def todateiso8601: strftime("%Y-%m-%dT%H:%M:%SZ");
def todate: todateiso8601;
def fromdateiso8601: strptime("%Y-%m-%dT%H:%M:%SZ") | mktime;
def fromdate: fromdateiso8601;
def date: todate;
def IN(source): any(source == .; .);
def IN(src; s): any(src == s; .);
def INDEX(stream; idx_expr): reduce stream as $row ({}; .[$row|idx_expr|tostring] = $row);
def INDEX(idx_expr): INDEX(.[]; idx_expr);
.
"#;

thread_local! {
    /// The prelude is parsed once per thread and every compiled program shares
    /// the resulting environment, so a per-line query pays for it exactly once.
    static PRELUDE_ENV: Env = build_prelude();
}

fn build_prelude() -> Env {
    let mut env = Env::default();
    let mut f = match parse(PRELUDE) {
        Ok(f) => f,
        // The prelude is a compile-time constant of this crate: a parse failure
        // is a bug in this file, not in user input, so it fails loudly here
        // rather than silently degrading every query.
        Err(e) => panic!("jqlang: prelude does not parse: {e}"),
    };
    while let Filter::Def(def, rest) = f {
        env = env.define(def);
        f = *rest;
    }
    env
}

fn prelude_env() -> Env {
    PRELUDE_ENV.with(Clone::clone)
}

/// jq's `builtins`: every callable name as `name/arity`.
fn builtin_names() -> Vec<String> {
    // The Rust half, listed explicitly: these have no `def` to walk.
    const NATIVE: &[&str] = &[
        "empty/0",
        "error/0",
        "error/1",
        "not/0",
        "type/0",
        "length/0",
        "utf8bytelength/0",
        "keys/0",
        "keys_unsorted/0",
        "has/1",
        "contains/1",
        "tostring/0",
        "tojson/0",
        "fromjson/0",
        "tonumber/0",
        "explode/0",
        "implode/0",
        "ascii_downcase/0",
        "ascii_upcase/0",
        "startswith/1",
        "endswith/1",
        "ltrim/0",
        "rtrim/0",
        "trim/0",
        "split/1",
        "_strindices/1",
        "sort/0",
        "reverse/0",
        "unique/0",
        "sort_by/1",
        "group_by/1",
        "unique_by/1",
        "min_by/1",
        "max_by/1",
        "min/0",
        "max/0",
        "range/2",
        "range/3",
        "floor/0",
        "ceil/0",
        "round/0",
        "sqrt/0",
        "fabs/0",
        "log/0",
        "log2/0",
        "log10/0",
        "exp/0",
        "exp2/0",
        "exp10/0",
        "trunc/0",
        "cbrt/0",
        "sin/0",
        "cos/0",
        "tan/0",
        "asin/0",
        "acos/0",
        "atan/0",
        "sinh/0",
        "cosh/0",
        "tanh/0",
        "nearbyint/0",
        "significand/0",
        "logb/0",
        "pow/2",
        "atan2/2",
        "fmin/2",
        "fmax/2",
        "ldexp/2",
        "infinite/0",
        "nan/0",
        "isnan/0",
        "isinfinite/0",
        "isnormal/0",
        "path/1",
        "getpath/1",
        "setpath/2",
        "delpaths/1",
        "_flatten/1",
        "_match_impl/3",
        "_split_re/2",
        "sub_impl/3",
        "env/0",
        "builtins/0",
        "input/0",
        "inputs/0",
        "input_line_number/0",
        "debug/0",
        "debug/1",
        "stderr/0",
        "halt/0",
        "halt_error/0",
        "halt_error/1",
        "input_filename/0",
        "have_literal_numbers/0",
        "have_decnum/0",
        "now/0",
        "mktime/0",
        "gmtime/0",
        "localtime/0",
        "strftime/1",
        "strflocaltime/1",
        "strptime/1",
        "acosh/0",
        "asinh/0",
        "atanh/0",
        "expm1/0",
        "log1p/0",
        "rint/0",
        "gamma/0",
        "lgamma/0",
        "tgamma/0",
        "erf/0",
        "erfc/0",
        "j0/0",
        "j1/0",
        "y0/0",
        "y1/0",
        "frexp/0",
        "modf/0",
        "lgamma_r/0",
        "isfinite/0",
        "format/1",
        "copysign/2",
        "drem/2",
        "fdim/2",
        "fmod/2",
        "hypot/2",
        "nextafter/2",
        "nexttoward/2",
        "remainder/2",
        "scalb/2",
        "scalbln/2",
        "jn/2",
        "yn/2",
        "fma/3",
        "get_jq_origin/0",
        "get_prog_origin/0",
        "get_search_list/0",
        "modulemeta/0",
    ];
    let mut names: Vec<String> = NATIVE.iter().map(|s| (*s).to_string()).collect();
    // The yq half of the superset claim. Listed from the same table `builtin`
    // dispatches from, so a name can never be callable but unlisted (or listed
    // but not callable) — `yq_superset_probe` measures exactly this set.
    names.extend(yq_builtin_names());
    prelude_env().walk_fn_names(&mut names);
    names.sort();
    names.dedup();
    names
}

impl Env {
    fn walk_fn_names(&self, out: &mut Vec<String>) {
        let mut cur = self.funcs.clone();
        while let Some(n) = cur {
            out.push(format!("{}/{}", n.name, n.arity));
            cur = n.next.clone();
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The yq surface
//
// arb's docs claim a jq/xpath/css/yq superset. `superset_probe` measures the jq
// leg by containment — every `name/arity` jq defines must exist here — and
// `yq_superset_probe` does the same for yq's own operator index. This section is
// what closes that leg: the ~60 operators yq has that jq has no equivalent for.
//
// Three groups, and only the first needs the node box:
//
//   * NODE METADATA — `anchor`, `alias`, `tag`, `style`, the three comments,
//     `key`, `is_key`, `path`, `parent`, `line`, `column`, `kind`,
//     `document_index`, `filename`, `fileIndex`. These read (and, through
//     `anchor = "x"`, write) `crate::ynode::NodeMeta`, which is exactly the
//     metadata a jq value has no slot for.
//   * ENCODERS — `to_json`/`from_json` and the yaml/xml/props/csv/tsv family,
//     plus `env`/`strenv`/`envsubst` and the `load` family. Pure text; see
//     `crate::yqfmt`.
//   * RESHAPING — `pick`/`omit`/`with`/`sort_keys`/`pivot`/`shuffle`/`ireduce`/
//     `eval`/`ref`/`splitDoc` and the `downcase`/`upcase`/`to_string`/
//     `to_number` spellings.
//
// Where a spelling had to change, it is because yq's grammar is not jq's and
// the difference is stated rather than papered over — see `ireduce` and `ref`.
// ─────────────────────────────────────────────────────────────────────────────

/// Every yq `name/arity` this engine defines. The single source of truth: both
/// [`is_yq_builtin`] and the `builtins` listing read it, so a name can never be
/// dispatchable but unlisted (or listed but undispatchable).
const YQ_BUILTINS: &[(&str, usize)] = &[
    // node metadata, read
    ("anchor", 0),
    ("alias", 0),
    ("tag", 0),
    ("style", 0),
    ("kind", 0),
    ("line", 0),
    ("column", 0),
    ("head_comment", 0),
    ("headComment", 0),
    ("line_comment", 0),
    ("lineComment", 0),
    ("foot_comment", 0),
    ("footComment", 0),
    ("comments", 0),
    ("key", 0),
    ("is_key", 0),
    ("parent", 0),
    ("path", 0),
    ("document_index", 0),
    ("documentIndex", 0),
    ("di", 0),
    ("filename", 0),
    ("fileIndex", 0),
    ("splitDoc", 0),
    ("split_doc", 0),
    ("explode", 1),
    // encoders
    ("to_json", 0),
    ("to_json", 1),
    ("from_json", 0),
    ("to_yaml", 0),
    ("to_yaml", 1),
    ("from_yaml", 0),
    ("to_xml", 0),
    ("to_xml", 1),
    ("from_xml", 0),
    ("to_props", 0),
    ("from_props", 0),
    ("to_csv", 0),
    ("from_csv", 0),
    ("to_tsv", 0),
    ("from_tsv", 0),
    // environment and files
    ("env", 1),
    ("strenv", 1),
    ("envsubst", 0),
    ("load", 1),
    ("load_str", 1),
    ("load_props", 1),
    ("load_xml", 1),
    // dates
    ("format_datetime", 1),
    ("from_unix", 0),
    ("to_unix", 0),
    ("tz", 1),
    ("with_dtf", 2),
    // reshaping
    ("omit", 1),
    ("with", 2),
    ("ref", 2),
    ("sort_keys", 1),
    ("sortKeys", 1),
    ("shuffle", 0),
    ("pivot", 0),
    ("pick", 1),
    ("ireduce", 2),
    ("eval", 1),
    ("downcase", 0),
    ("upcase", 0),
    ("to_string", 0),
    ("to_number", 0),
];

/// Does arb define `src` as a FUNCTION NAME at some arity?
///
/// Asked when a one-word jq program fails to compile, to tell an unknown verb
/// apart from a known name called at the wrong arity. Both references report the
/// latter as an arity problem — `jq 'ltrimstr'` says "ltrimstr/0 is not
/// defined", `yq -n 'ref'` says "'ref' expects 2 args" — so answering "unknown
/// verb" for a name that exists is wrong information, not merely a worse
/// message.
pub fn defines_name(src: &str) -> bool {
    let word = src.trim();
    if word.is_empty() || !word.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return false;
    }
    if YQ_BUILTINS.iter().any(|&(n, _)| n == word) {
        return true;
    }
    let prefix = format!("{word}/");
    builtin_names().iter().any(|n| n.starts_with(&prefix))
}

/// Is `name/arity` one of the yq operators dispatched by [`yq_builtin`]?
///
/// Checked BEFORE `builtin` unboxes its input, because the metadata group is the
/// only code in the engine allowed to see a `JqVal::Node`.
fn is_yq_builtin(name: &str, arity: usize) -> bool {
    YQ_BUILTINS.iter().any(|&(n, a)| n == name && a == arity)
}

/// The names for the `builtins` listing, in jq's `name/arity` spelling.
fn yq_builtin_names() -> Vec<String> {
    YQ_BUILTINS
        .iter()
        .map(|(n, a)| format!("{n}/{a}"))
        .collect()
}

/// Split a metadata assignment's left-hand side into the path it selects (or
/// `None` for `.` itself) and the metadata field being written.
fn meta_assign_target(lhs: &Filter) -> Option<(Option<&Filter>, &str)> {
    match lhs {
        Filter::Call(name, args) if args.is_empty() && is_meta_setter(name) => Some((None, name)),
        Filter::Pipe(p, tail) => match &**tail {
            Filter::Call(name, args) if args.is_empty() && is_meta_setter(name) => {
                Some((Some(&**p), name))
            }
            _ => None,
        },
        _ => None,
    }
}

/// The metadata accessors that may stand on the LEFT of `=`.
///
/// yq spells the assignment as a postfix on a path (`.a anchor = "x"`). arb's
/// grammar is jq's, where `|` binds loosest, so the DOCUMENT-preserving spelling
/// is `.a |= (anchor = "x")`: the update operator applies the edit at the path
/// and hands the whole document back, which is what yq's postfix does. A bare
/// `anchor = "x"` sets it on `.` and yields that node alone. Both reach here
/// through [`eval_assign`].
fn is_meta_setter(name: &str) -> bool {
    matches!(
        name,
        "anchor"
            | "tag"
            | "style"
            | "head_comment"
            | "headComment"
            | "line_comment"
            | "lineComment"
            | "foot_comment"
            | "footComment"
            | "comments"
    )
}

/// Apply a metadata assignment to one node.
fn set_meta(node: &JqVal, name: &str, val: &JqVal) -> JqVal {
    let text: Rc<str> = match val.bare() {
        JqVal::Str(s) => s.clone(),
        JqVal::Null => Rc::from(""),
        other => Rc::from(render_raw(other).as_str()),
    };
    // Setting a CORE-SCHEMA tag converts the value, which is what the tag means:
    // `.a tag = "!!str"` on `a: 1` gives `a: "1"` in yq, not `a: !!str 1`. Once
    // converted the tag is implicit, so it is not written out as well.
    if name == "tag" {
        if let Some(v) = coerce_to_tag(node.bare(), &text) {
            let implicit = crate::ynode::implicit_tag(&v) == &*text;
            let mut meta = node.meta().cloned().unwrap_or_default();
            meta.tag = if implicit { Rc::from("") } else { text.clone() };
            meta.raw = Rc::from("");
            meta.blank = false;
            return JqVal::wrap(v, meta);
        }
    }
    node.with_meta(|m| match name {
        "anchor" => m.anchor = text.clone(),
        "tag" => m.tag = text.clone(),
        "style" => m.style = crate::ynode::Style::parse(&text),
        "head_comment" | "headComment" => m.head = text.clone(),
        "line_comment" | "lineComment" => m.line = text.clone(),
        "foot_comment" | "footComment" => m.foot = text.clone(),
        // `... comments = ""` is yq's spelling for "strip every comment here".
        "comments" => {
            m.head = text.clone();
            m.line = text.clone();
            m.foot = text.clone();
        }
        _ => {}
    })
}

/// Convert a value to the type a core-schema tag names, or `None` for a tag with
/// no conversion (a local `!mytag`, or one already matching).
fn coerce_to_tag(v: &JqVal, tag: &str) -> Option<JqVal> {
    match tag {
        "!!str" => Some(JqVal::str(render_raw(v))),
        "!!int" => v
            .as_f64()
            .or_else(|| match v {
                JqVal::Str(s) => s.trim().parse().ok(),
                _ => None,
            })
            .map(|n| JqVal::num(n.trunc())),
        "!!float" => v
            .as_f64()
            .or_else(|| match v {
                JqVal::Str(s) => s.trim().parse().ok(),
                _ => None,
            })
            .map(JqVal::num),
        "!!bool" => match v {
            JqVal::Bool(_) => Some(v.clone()),
            JqVal::Str(s) => match &**s {
                "true" => Some(JqVal::Bool(true)),
                "false" => Some(JqVal::Bool(false)),
                _ => None,
            },
            _ => None,
        },
        "!!null" => Some(JqVal::Null),
        _ => None,
    }
}

/// Read a file, reporting yq's own message shape on failure.
fn read_file(path: &str) -> R<String> {
    std::fs::read_to_string(path).map_err(|e| JqErr::msg(format!("failed to load {path}: {e}")))
}

fn yq_builtin(
    it: &Interp,
    name: &str,
    args: &[Rc<Filter>],
    input: &JqVal,
    env: &Env,
    out: Sink,
) -> R<()> {
    let meta = input.meta().cloned().unwrap_or_default();
    let str_arg = |i: usize| -> R<Rc<str>> {
        let v = one(it, &args[i], input, env)?;
        Ok(match v.bare() {
            JqVal::Str(s) => s.clone(),
            other => Rc::from(render_raw(other).as_str()),
        })
    };
    match (name, args.len()) {
        // ── node metadata ───────────────────────────────────────────────────
        ("anchor", 0) => out(JqVal::Str(meta.anchor)),
        ("alias", 0) => out(JqVal::Str(meta.alias)),
        ("tag", 0) => out(JqVal::str(if meta.tag.is_empty() {
            crate::ynode::implicit_tag(input)
        } else {
            return out(JqVal::Str(meta.tag));
        })),
        ("style", 0) => out(JqVal::str(meta.style.name())),
        ("kind", 0) => out(JqVal::str(crate::ynode::kind_of(input))),
        ("line", 0) => out(JqVal::num(meta.line_no.max(1) as f64)),
        ("column", 0) => out(JqVal::num(meta.col_no.max(1) as f64)),
        ("head_comment", 0) | ("headComment", 0) => out(JqVal::Str(meta.head)),
        ("line_comment", 0) | ("lineComment", 0) => out(JqVal::Str(meta.line)),
        ("foot_comment", 0) | ("footComment", 0) => out(JqVal::Str(meta.foot)),
        // Read back, `comments` is every comment on the node, in the order they
        // appear on the page.
        ("comments", 0) => {
            let all: Vec<&str> = [&*meta.head, &*meta.line, &*meta.foot]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect();
            out(JqVal::str(all.join("\n")))
        }
        ("key", 0) => out(match &meta.key {
            Some(k) => (**k).clone(),
            // A node with no key (a document root, a sequence item) has none;
            // yq answers null there too.
            None => JqVal::Null,
        }),
        ("is_key", 0) => out(JqVal::Bool(meta.is_key)),
        ("path", 0) => out(JqVal::arr(meta.path.as_ref().clone())),
        ("parent", 0) => {
            // The document the node was READ from, walked to the path one step
            // short of the node. See `crate::ynode` for what this does not
            // promise once a value has been moved.
            let segs = meta.path.as_ref();
            match (it.current_doc(), segs.split_last()) {
                (Some(doc), Some((_, up))) => out(get_path(&doc, up)?),
                _ => out(JqVal::Null),
            }
        }
        ("document_index", 0) | ("documentIndex", 0) | ("di", 0) => {
            out(JqVal::num(meta.doc as f64))
        }
        ("filename", 0) => out(JqVal::str(if meta.file.is_empty() {
            Rc::from("-")
        } else {
            meta.file.clone()
        })),
        ("fileIndex", 0) => out(JqVal::num(meta.file_index as f64)),
        // arb's stream is already one document per output value, and `out.yaml`
        // separates them with `---`. `splitDoc` therefore has nothing left to
        // do — it is the identity here, not a stub: the split it asks for has
        // already happened by the time a value reaches it.
        ("splitDoc", 0) | ("split_doc", 0) => out(input.clone()),
        // `explode(f)`: drop the anchor/alias metadata under `f`, so an aliased
        // node is written out in full instead of as `*name`.
        ("explode", 1) => {
            let mut cur = input.clone();
            let mut paths = Vec::new();
            eval_paths(
                it,
                &args[0],
                input,
                &Tracked::root(input),
                input,
                env,
                &mut |p, v| {
                    let p = intact_path(p, &v)?;
                    paths.push(p);
                    Ok(())
                },
            )?;
            for p in paths {
                let at = get_path(&cur, &p)?;
                let flat = explode_node(&at);
                cur = if p.is_empty() {
                    flat
                } else {
                    set_path(&cur, &p, flat)?
                };
            }
            out(cur)
        }

        // ── encoders ────────────────────────────────────────────────────────
        ("to_json", 0) => out(JqVal::str(json_indented(input, 2))),
        ("to_json", 1) => {
            let n = one(it, &args[0], input, env)?.as_f64().unwrap_or(2.0);
            out(JqVal::str(json_indented(input, n.max(0.0) as usize)))
        }
        ("from_json", 0) => {
            let s = want_str(input, "parsed as JSON")?;
            out(parse_json(&s).map_err(JqErr::msg)?)
        }
        ("to_yaml", 0) => out(JqVal::str(crate::ynode::emit_doc(
            input,
            crate::ynode::Emit::default(),
        ))),
        ("to_yaml", 1) => {
            let n = one(it, &args[0], input, env)?.as_f64().unwrap_or(2.0);
            out(JqVal::str(crate::ynode::emit_doc(
                input,
                crate::ynode::Emit {
                    indent: n.max(0.0) as usize,
                },
            )))
        }
        ("from_yaml", 0) => {
            let s = want_str(input, "parsed as YAML")?;
            out(crate::yaml::documents(&s)
                .into_iter()
                .next()
                .unwrap_or(JqVal::Null))
        }
        ("to_xml", 0) => out(JqVal::str(crate::yqfmt::to_xml(input, 2))),
        ("to_xml", 1) => {
            let n = one(it, &args[0], input, env)?.as_f64().unwrap_or(2.0);
            out(JqVal::str(crate::yqfmt::to_xml(input, n.max(0.0) as usize)))
        }
        ("from_xml", 0) => out(crate::yqfmt::from_xml(&want_str(input, "parsed as XML")?)),
        ("to_props", 0) => out(JqVal::str(crate::yqfmt::to_props(input))),
        ("from_props", 0) => out(crate::yqfmt::from_props(&want_str(
            input,
            "parsed as properties",
        )?)),
        ("to_csv", 0) => out(JqVal::str(crate::yqfmt::to_delim(input, ','))),
        ("to_tsv", 0) => out(JqVal::str(crate::yqfmt::to_delim(input, '\t'))),
        ("from_csv", 0) => out(crate::yqfmt::from_delim(
            &want_str(input, "parsed as CSV")?,
            ',',
        )),
        ("from_tsv", 0) => out(crate::yqfmt::from_delim(
            &want_str(input, "parsed as TSV")?,
            '\t',
        )),

        // ── environment and files ───────────────────────────────────────────
        // `env(NAME)` resolves the value the way YAML would (`env(PORT)` is a
        // number); `strenv(NAME)` always answers a string. That difference is
        // the whole reason yq has both.
        ("env", 1) => {
            let n = str_arg(0)?;
            out(match std::env::var(&*n) {
                Ok(v) => crate::yaml::documents(&v)
                    .into_iter()
                    .next()
                    .unwrap_or(JqVal::Null),
                Err(_) => JqVal::Null,
            })
        }
        ("strenv", 1) => {
            let n = str_arg(0)?;
            out(JqVal::str(std::env::var(&*n).unwrap_or_default()))
        }
        ("envsubst", 0) => out(JqVal::str(crate::yqfmt::envsubst(&want_str(
            input, "expanded",
        )?))),
        ("load", 1) => {
            let path = str_arg(0)?;
            let text = read_file(&path)?;
            out(crate::yaml::documents_from(&text, &path, 0)
                .into_iter()
                .next()
                .unwrap_or(JqVal::Null))
        }
        ("load_str", 1) => out(JqVal::str(read_file(&str_arg(0)?)?)),
        ("load_props", 1) => out(crate::yqfmt::from_props(&read_file(&str_arg(0)?)?)),
        ("load_xml", 1) => out(crate::yqfmt::from_xml(&read_file(&str_arg(0)?)?)),

        // ── dates ───────────────────────────────────────────────────────────
        ("format_datetime", 1) => {
            let layout = str_arg(0)?;
            let secs = to_unix_secs(input)?;
            out(JqVal::str(crate::yqfmt::format_go(secs, &layout, true)))
        }
        ("from_unix", 0) => {
            let secs = input.as_f64().unwrap_or(0.0);
            out(JqVal::str(crate::yqfmt::format_go(
                secs,
                "2006-01-02T15:04:05Z07:00",
                false,
            )))
        }
        ("to_unix", 0) => out(JqVal::num(to_unix_secs(input)?)),
        ("tz", 1) => {
            // Only UTC is a zone this build can resolve without a tzdata
            // dependency; any other name is answered in UTC and says so.
            let zone = str_arg(0)?;
            let secs = to_unix_secs(input)?;
            let utc = matches!(&*zone, "UTC" | "utc" | "Z" | "GMT" | "");
            out(JqVal::str(crate::yqfmt::format_go(
                secs,
                "2006-01-02T15:04:05Z07:00",
                utc,
            )))
        }
        // `with_dtf(layout; f)` runs `f` with `layout` as the date format. arb
        // has no dynamically scoped format, so the layout is applied to `f`'s
        // result the way yq's own `with_dtf` applies it to what `f` produces.
        ("with_dtf", 2) => {
            let layout = str_arg(0)?;
            eval(it, &args[1], input, env, &mut |v| match v.bare() {
                JqVal::Num(n, _) => out(JqVal::str(crate::yqfmt::format_go(*n, &layout, true))),
                other => out(other.clone()),
            })
        }

        // ── reshaping ───────────────────────────────────────────────────────
        // `pick(["a","b"])` is yq's spelling and `pick(.a, .b)` is jq's. jq
        // REFUSES the array form ("Invalid path expression with result
        // [\"a\"]"), so answering it is a superset extension rather than a
        // conflict, and the jq form still reaches jq's own definition below.
        ("pick", 1) => {
            let keys = one(it, &args[0], input, env)?;
            let JqVal::Arr(want) = keys.bare() else {
                return builtin_jq_pick(it, args, input, env, out);
            };
            if !want.iter().all(|k| matches!(k.bare(), JqVal::Str(_))) {
                return builtin_jq_pick(it, args, input, env, out);
            }
            let JqVal::Obj(m) = input.bare() else {
                return out(input.clone());
            };
            // In the ORDER ASKED FOR, which is what yq answers with
            // (`pick(["c","a"])` is `{c: …, a: …}`), and a key the object does
            // not have contributes nothing.
            let kept: Vec<(Rc<str>, JqVal)> = want
                .iter()
                .filter_map(|k| match k.bare() {
                    JqVal::Str(name) => input.obj_lookup(name).map(|v| (name.clone(), v.clone())),
                    _ => None,
                })
                .collect();
            let _ = m;
            out(match input.meta() {
                Some(mm) => JqVal::wrap(JqVal::obj(kept), mm.clone()),
                None => JqVal::obj(kept),
            })
        }
        ("omit", 1) => {
            let keys = one(it, &args[0], input, env)?;
            let JqVal::Arr(drop) = keys.bare() else {
                return Err(JqErr::msg("omit expects an array of keys"));
            };
            let JqVal::Obj(m) = input.bare() else {
                return out(input.clone());
            };
            let kept: Vec<(Rc<str>, JqVal)> = m
                .iter()
                .filter(|(k, _)| {
                    !drop
                        .iter()
                        .any(|d| matches!(d.bare(), JqVal::Str(s) if s == k))
                })
                .cloned()
                .collect();
            out(match input.meta() {
                Some(mm) => JqVal::wrap(JqVal::obj(kept), mm.clone()),
                None => JqVal::obj(kept),
            })
        }
        // `with(p; f)` and `ref(p; f)` are both "update at a path", which is what
        // yq's two spellings do — `ref` binds a mutable handle and `with` scopes
        // one, and in a model without mutable handles both are `p |= f`.
        ("with", 2) | ("ref", 2) => {
            let update = Filter::Assign(
                AssignOp::Update,
                Box::new((*args[0]).clone()),
                Box::new((*args[1]).clone()),
            );
            eval(it, &update, input, env, out)
        }
        ("sort_keys", 1) | ("sortKeys", 1) => {
            // yq's argument selects WHERE to sort: `sort_keys(.)` is this level,
            // `sort_keys(..)` is every level.
            let deep = matches!(&*args[0], Filter::RecurseDefault);
            out(crate::yqfmt::sort_keys(input, deep))
        }
        ("shuffle", 0) => out(crate::yqfmt::shuffle(input)),
        ("pivot", 0) => out(crate::yqfmt::pivot(input)),
        // yq writes this as `.[] as $item ireduce (0; . + $item)`. arb's grammar
        // is jq's, so the stream is the input's own elements and `$item` is bound
        // for the body — `[1,2,3] | ireduce(0; . + $item)` is 6, the same answer
        // yq gives for the same reduction.
        ("ireduce", 2) => {
            let mut acc = one(it, &args[0], input, env)?;
            let JqVal::Arr(items) = input.bare() else {
                return out(acc);
            };
            for e in items.iter() {
                let benv = env.bind(Rc::from("item"), e.clone());
                acc = one(it, &args[1], &acc, &benv)?;
            }
            out(acc)
        }
        ("eval", 1) => {
            let src = str_arg(0)?;
            let f = parse(&src).map_err(JqErr::msg)?;
            eval(it, &f, input, env, out)
        }
        ("downcase", 0) => out(JqVal::str(
            want_str(input, "downcased")?.to_lowercase().as_str(),
        )),
        ("upcase", 0) => out(JqVal::str(
            want_str(input, "upcased")?.to_uppercase().as_str(),
        )),
        // The SOURCE spelling where the reader kept one: `padded: 007` is the
        // string `007`, not `7`, which is what yq answers.
        ("to_string", 0) => out(JqVal::str(
            match input.meta().filter(|m| !m.raw.is_empty()) {
                Some(m) => m.raw.to_string(),
                None => render_raw(input),
            },
        )),
        ("to_number", 0) => match input.bare() {
            JqVal::Num(..) => out(input.bare().clone()),
            JqVal::Str(s) => match s.trim().parse::<f64>() {
                Ok(n) => out(num_from_literal(n, s.trim())),
                Err(_) => Err(JqErr::msg(format!("cannot convert '{s}' to a number"))),
            },
            other => Err(JqErr::msg(format!(
                "cannot convert {} to a number",
                other.type_name()
            ))),
        },
        _ => Err(JqErr::msg(format!("{name} is not a yq operator"))),
    }
}

/// jq's own `pick(pathexps)`, reached when the argument is not yq's array of
/// keys. Defined in the prelude, so it is called rather than reimplemented.
fn builtin_jq_pick(it: &Interp, args: &[Rc<Filter>], input: &JqVal, env: &Env, out: Sink) -> R<()> {
    let node = env
        .find_fn("pick", 1)
        .ok_or_else(|| JqErr::msg("pick/1 is not defined"))?;
    let (body, benv) = bind_call(it, &node, args, env)?;
    eval(it, &body, input, &benv, out)
}

/// Drop anchor/alias metadata through a whole subtree, which is what `explode`
/// means: the document that comes out has no `&name`/`*name` left in it.
fn explode_node(v: &JqVal) -> JqVal {
    let stripped = match v.bare() {
        JqVal::Arr(a) => JqVal::arr(a.iter().map(explode_node).collect()),
        JqVal::Obj(m) => JqVal::obj(
            m.iter()
                .map(|(k, val)| (k.clone(), explode_node(val)))
                .collect(),
        ),
        other => other.clone(),
    };
    match v.meta() {
        Some(m) => {
            let mut m = m.clone();
            m.anchor = Rc::from("");
            m.alias = Rc::from("");
            JqVal::wrap(stripped, m)
        }
        None => stripped,
    }
}

/// The Unix second count a value denotes: a number is already one, a string is
/// read as RFC-3339.
fn to_unix_secs(v: &JqVal) -> R<f64> {
    match v.bare() {
        JqVal::Num(n, _) => Ok(*n),
        JqVal::Str(s) => crate::yqfmt::parse_rfc3339(s)
            .ok_or_else(|| JqErr::msg(format!("cannot parse '{s}' as a date"))),
        other => Err(JqErr::msg(format!(
            "cannot read {} as a date",
            other.type_name()
        ))),
    }
}

/// `to_json(n)`: jq's own compact rendering when `n` is 0, and an indented one
/// otherwise. Reuses `render` for the compact case so the two can never drift.
pub fn render_indented(v: &JqVal, indent: usize) -> String {
    json_indented(v, indent).trim_end_matches('\n').to_string()
}

fn json_indented(v: &JqVal, indent: usize) -> String {
    if indent == 0 {
        return render(v);
    }
    let mut out = String::new();
    write_indented(&mut out, v, 0, indent);
    out.push('\n');
    out
}

fn write_indented(out: &mut String, v: &JqVal, depth: usize, step: usize) {
    let pad = |out: &mut String, d: usize| out.push_str(&" ".repeat(d * step));
    match v.bare() {
        JqVal::Arr(a) if !a.is_empty() => {
            out.push_str("[\n");
            for (i, e) in a.iter().enumerate() {
                pad(out, depth + 1);
                write_indented(out, e, depth + 1, step);
                if i + 1 < a.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            pad(out, depth);
            out.push(']');
        }
        JqVal::Obj(m) if !m.is_empty() => {
            out.push_str("{\n");
            for (i, (k, val)) in m.iter().enumerate() {
                pad(out, depth + 1);
                out.push_str(&render(&JqVal::Str(k.clone())));
                out.push_str(": ");
                write_indented(out, val, depth + 1, step);
                if i + 1 < m.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            pad(out, depth);
            out.push('}');
        }
        other => out.push_str(&render(other)),
    }
}

/// Names the resolver must accept that `builtins` does not list, because jq does
/// not list them either: its own internal helpers.
const NATIVE_ONLY: &[&str] = &[
    "_match_impl/3",
    "_split_re/2",
    "sub_impl/3",
    "_flatten/1",
    "_strindices/1",
];

/// Reject a call to an undefined function or a reference to an unbound variable,
/// the way jq's compiler does. `funcs` holds `name/arity` keys.
fn check_names(
    f: &Filter,
    funcs: &std::collections::HashSet<String>,
    vars: &std::collections::HashSet<String>,
) -> Result<(), String> {
    match f {
        Filter::Identity | Filter::RecurseDefault | Filter::Lit(_) | Filter::Format(_) => Ok(()),
        Filter::Str(pieces, _) => {
            for p in pieces {
                if let StrPiece::Interp(src) = p {
                    check_names(&parse(src)?, funcs, vars)?;
                }
            }
            Ok(())
        }
        Filter::Field(a, _)
        | Filter::Iterate(a)
        | Filter::Optional(a)
        | Filter::IndexOpt(a)
        | Filter::Neg(a) => check_names(a, funcs, vars),
        Filter::Index(a, b)
        | Filter::Pipe(a, b)
        | Filter::Comma(a, b)
        | Filter::Bin(_, a, b)
        | Filter::And(a, b)
        | Filter::Or(a, b)
        | Filter::Alt(a, b)
        | Filter::Assign(_, a, b) => {
            check_names(a, funcs, vars)?;
            check_names(b, funcs, vars)
        }
        Filter::Slice(a, lo, hi) => {
            check_names(a, funcs, vars)?;
            for x in [lo, hi].into_iter().flatten() {
                check_names(x, funcs, vars)?;
            }
            Ok(())
        }
        Filter::If(arms, els) => {
            for (c, t) in arms {
                check_names(c, funcs, vars)?;
                check_names(t, funcs, vars)?;
            }
            match els {
                Some(e) => check_names(e, funcs, vars),
                None => Ok(()),
            }
        }
        Filter::Try(a, h) => {
            check_names(a, funcs, vars)?;
            match h {
                Some(x) => check_names(x, funcs, vars),
                None => Ok(()),
            }
        }
        Filter::Reduce(src, pat, init, upd) => {
            check_names(src, funcs, vars)?;
            check_names(init, funcs, vars)?;
            let inner = with_pattern_vars(vars, std::slice::from_ref(pat));
            check_names(upd, funcs, &inner)
        }
        Filter::Foreach(src, pat, init, upd, ext) => {
            check_names(src, funcs, vars)?;
            check_names(init, funcs, vars)?;
            let inner = with_pattern_vars(vars, std::slice::from_ref(pat));
            check_names(upd, funcs, &inner)?;
            match ext {
                Some(e) => check_names(e, funcs, &inner),
                None => Ok(()),
            }
        }
        Filter::Bind(src, pats, body) => {
            check_names(src, funcs, vars)?;
            let inner = with_pattern_vars(vars, pats);
            check_names(body, funcs, &inner)
        }
        Filter::Label(name, body) => {
            let mut inner = vars.clone();
            inner.insert(format!("*label*{name}"));
            check_names(body, funcs, &inner)
        }
        Filter::Break(name) => {
            if vars.contains(&format!("*label*{name}")) {
                Ok(())
            } else {
                Err(format!("jq: $*label-{name} is not defined"))
            }
        }
        Filter::Var(name) => {
            if vars.contains(&**name) {
                Ok(())
            } else {
                Err(format!("jq: ${name} is not defined"))
            }
        }
        Filter::Call(name, args) => {
            let key = format!("{name}/{}", args.len());
            if !funcs.contains(&key) {
                return Err(format!("jq: {key} is not defined"));
            }
            for a in args {
                check_names(a, funcs, vars)?;
            }
            Ok(())
        }
        Filter::Def(def, rest) => {
            let mut outer = funcs.clone();
            outer.insert(format!("{}/{}", def.name, def.params.len()));
            let mut inner = outer.clone();
            for p in &def.params {
                inner.insert(format!("{p}/0"));
            }
            // A `$p` parameter is desugared into `p as $p | …`, so the variable
            // it binds is introduced by that `Bind` and needs nothing here.
            check_names(&def.body, &inner, vars)?;
            check_names(rest, &outer, vars)
        }
        Filter::Object(entries) => {
            for ObjEntry::KeyVal(k, v) in entries {
                check_names(k, funcs, vars)?;
                check_names(v, funcs, vars)?;
            }
            Ok(())
        }
        Filter::Array(inner) => match inner {
            Some(x) => check_names(x, funcs, vars),
            None => Ok(()),
        },
    }
}

fn with_pattern_vars(
    vars: &std::collections::HashSet<String>,
    pats: &[Pattern],
) -> std::collections::HashSet<String> {
    let mut out = vars.clone();
    let mut names = Vec::new();
    for p in pats {
        collect_pattern_vars(p, &mut names);
    }
    out.extend(names.into_iter().map(|n| n.to_string()));
    out
}

/// Visit every sub-filter of `f` exactly once. Used by the whole-program
/// questions (`reads_input_stream`) that do not care about structure, only about
/// whether some node appears.
fn for_each_child(f: &Filter, visit: &mut dyn FnMut(&Filter)) {
    match f {
        Filter::Identity
        | Filter::RecurseDefault
        | Filter::Lit(_)
        | Filter::Format(_)
        | Filter::Var(_)
        | Filter::Break(_)
        | Filter::Str(..) => {}
        Filter::Field(a, _)
        | Filter::Iterate(a)
        | Filter::Optional(a)
        | Filter::IndexOpt(a)
        | Filter::Neg(a) => {
            visit(a);
        }
        Filter::Index(a, b)
        | Filter::Pipe(a, b)
        | Filter::Comma(a, b)
        | Filter::Bin(_, a, b)
        | Filter::And(a, b)
        | Filter::Or(a, b)
        | Filter::Alt(a, b)
        | Filter::Assign(_, a, b) => {
            visit(a);
            visit(b);
        }
        Filter::Slice(a, lo, hi) => {
            visit(a);
            for x in [lo, hi].into_iter().flatten() {
                visit(x);
            }
        }
        Filter::If(arms, els) => {
            for (c, t) in arms {
                visit(c);
                visit(t);
            }
            if let Some(e) = els {
                visit(e);
            }
        }
        Filter::Try(a, h) => {
            visit(a);
            if let Some(x) = h {
                visit(x);
            }
        }
        Filter::Reduce(src, _, init, upd) => {
            visit(src);
            visit(init);
            visit(upd);
        }
        Filter::Foreach(src, _, init, upd, ext) => {
            visit(src);
            visit(init);
            visit(upd);
            if let Some(e) = ext {
                visit(e);
            }
        }
        Filter::Bind(src, _, body) => {
            visit(src);
            visit(body);
        }
        Filter::Label(_, body) => visit(body),
        Filter::Call(_, args) => args.iter().for_each(|a| visit(a)),
        Filter::Def(def, rest) => {
            visit(&def.body);
            visit(rest);
        }
        Filter::Object(entries) => {
            for ObjEntry::KeyVal(k, v) in entries {
                visit(k);
                visit(v);
            }
        }
        Filter::Array(inner) => {
            if let Some(x) = inner {
                visit(x);
            }
        }
    }
}
