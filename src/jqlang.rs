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

/// jq's total order over values (`sort`, `<`, `group_by`, `unique`).
///
/// Ported from jq 1.8.2 `src/jv.c:jv_cmp`. Objects compare by their SORTED key
/// list first and only then by the values at those keys, which is why
/// `{"a":1} < {"b":0}` even though `1 > 0`.
pub fn cmp_vals(a: &JqVal, b: &JqVal) -> Ordering {
    let (ra, rb) = (a.order_rank(), b.order_rank());
    if ra != rb {
        return ra.cmp(&rb);
    }
    // Order is a property of the VALUE. A YAML node's comment or anchor must not
    // reorder a sort, so both sides are unboxed before the comparison.
    let (a, b) = (a.bare(), b.bare());
    match (a, b) {
        (JqVal::Num(x, _), JqVal::Num(y, _)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (JqVal::Str(x), JqVal::Str(y)) => x.cmp(y),
        (JqVal::Arr(x), JqVal::Arr(y)) => {
            for (ea, eb) in x.iter().zip(y.iter()) {
                let c = cmp_vals(ea, eb);
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
                let c = cmp_vals(
                    a.obj_get(k).unwrap_or(&JqVal::Null),
                    b.obj_get(k).unwrap_or(&JqVal::Null),
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

/// Build a number value from its source text, keeping the literal only when jq
/// would print something other than the double's own shortest form.
pub(crate) fn num_from_literal(n: f64, text: &str) -> JqVal {
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
        // `.name` — but not `..`, and not a `.` that begins a number (`.5` is
        // not jq syntax, so a digit here still falls through to the operator).
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
        if c.is_ascii_digit() {
            let start = i;
            while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '.') {
                // A `.` only continues the number when a DIGIT follows it —
                // otherwise `1.foo` would swallow the field access, and `..` in
                // `1..2` would vanish.
                if cs[i] == '.' && !matches!(cs.get(i + 1), Some(d) if d.is_ascii_digit()) {
                    break;
                }
                i += 1;
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
        // `TERM as PATTERNS | BODY` binds looser than `,` and consumes the rest
        // of the pipeline as its body.
        if self.is_kw("as") {
            self.i += 1;
            let mut pats = vec![self.pattern()?];
            while self.eat_op("?//") {
                pats.push(self.pattern()?);
            }
            self.want_op("|")?;
            let body = self.pipe()?;
            return Ok(Filter::Bind(Box::new(lhs), pats, Box::new(body)));
        }
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
                        // `{$a}` — bind `.a` to `$a`.
                        Some(Tok::Var(v)) => {
                            self.i += 1;
                            let name: Rc<str> = Rc::from(v.as_str());
                            if self.eat_op(":") {
                                let sub = self.pattern()?;
                                out.push((Filter::Var(name), Some(sub), None));
                            } else {
                                out.push((Filter::Lit(JqVal::str(v.clone())), None, Some(name)));
                            }
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
        let mut lhs = self.alt()?;
        while self.eat_op(",") {
            let rhs = self.alt()?;
            lhs = Filter::Comma(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
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
    /// A term plus its postfix chain (`.k`, `[e]`, `[]`, `[a:b]`, `?`) and the
    /// `as`-binding that may follow it.
    ///
    /// `Term "as" Patterns '|' Exp` is jq's own production: the SOURCE is a
    /// term, and the BODY is a full expression running to the end of the
    /// pipeline. Measured: `[1, 2 as $x | $x]` is `[1,2]`, so the `,` is outside
    /// the binding and only `2` is bound.
    fn postfix(&mut self) -> Result<Filter, String> {
        let mut f = self.term()?;
        loop {
            if self.eat_op("?") {
                f = Filter::Optional(Box::new(f));
                continue;
            }
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
            break;
        }
        if self.is_kw("as") {
            self.i += 1;
            let mut pats = vec![self.pattern()?];
            while self.eat_op("?//") {
                pats.push(self.pattern()?);
            }
            self.want_op("|")?;
            let body = self.pipe()?;
            return Ok(Filter::Bind(Box::new(f), pats, Box::new(body)));
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
                let body = self.postfix()?;
                let handler = if self.eat_kw("catch") {
                    Some(Box::new(self.postfix()?))
                } else {
                    None
                };
                Ok(Filter::Try(Box::new(body), handler))
            }
            "reduce" | "foreach" => {
                let is_reduce = name == "reduce";
                self.i += 1;
                let src = self.postfix_no_bind()?;
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

    /// `reduce`/`foreach` take a Term as their SOURCE, and that term must not
    /// swallow the `as` that follows it — which `postfix` would, since `as` is
    /// part of its own production. Same chain, `as` handling removed.
    fn postfix_no_bind(&mut self) -> Result<Filter, String> {
        let save_as = self.t.len();
        let _ = save_as;
        let mut f = self.term()?;
        loop {
            if self.eat_op("?") {
                f = Filter::Optional(Box::new(f));
                continue;
            }
            if let Some(Tok::Field(name)) = self.peek() {
                let name: Rc<str> = Rc::from(name.as_str());
                self.i += 1;
                f = Filter::Field(Box::new(f), name);
                continue;
            }
            if self.is_op(".") && matches!(self.t.get(self.i + 1), Some(Tok::Op("["))) {
                self.i += 1;
                continue;
            }
            if self.is_op("[") {
                self.i += 1;
                f = self.bracket_suffix(f)?;
                continue;
            }
            break;
        }
        Ok(f)
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
                Some(Tok::Var(v)) => {
                    self.i += 1;
                    (
                        Filter::Lit(JqVal::str(v.clone())),
                        Some(Filter::Var(Rc::from(v.as_str()))),
                    )
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
            if self.eat_op(",") {
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
    /// What `input_line_number` reports: the 1-based index of the line being
    /// evaluated, which is what jq reports while reading a multi-line stream.
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
    pub fn next_input(&self) -> Option<JqVal> {
        let line = self.inputs.borrow_mut().pop_front()?;
        Some(parse_json(&line).unwrap_or_else(|_| JqVal::str(line.as_str())))
    }
    /// Set what `input_line_number` reports for the value about to be run.
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
        Filter::Field(base, name) => eval(it, base, input, env, &mut |v| {
            out(index_value(&v, &JqVal::Str(name.clone()))?)
        }),
        Filter::Index(base, idx) => eval(it, base, input, env, &mut |v| {
            eval(it, idx, input, env, &mut |i| out(index_value(&v, &i)?))
        }),
        Filter::Slice(base, lo, hi) => eval(it, base, input, env, &mut |v| {
            eval_opt(it, lo.as_deref(), input, env, &mut |lo_v| {
                eval_opt(it, hi.as_deref(), input, env, &mut |hi_v| {
                    out(slice_value(&v, &lo_v, &hi_v)?)
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
            other => Err(JqErr::msg(format!(
                "Cannot iterate over {}{}",
                other.type_name(),
                paren_of(other)
            ))),
        }),
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
            JqVal::Num(n, _) => out(JqVal::num(-n)),
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
        // `a // b`: every TRUTHY output of `a`, with its errors suppressed; only
        // if there were none does `b` run.
        Filter::Alt(a, b) => {
            let mut any = false;
            let r = eval(it, a, input, env, &mut |v| {
                if v.truthy() {
                    any = true;
                    out(v).map_err(wrap_downstream)
                } else {
                    Ok(())
                }
            });
            if let Err(e) = r {
                match unwrap_downstream(e) {
                    Ok(real) => return Err(real),
                    Err(JqErr::Err(_)) => {}
                    Err(other) => return Err(other),
                }
            }
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

/// The `(value)` suffix jq appends to a type in an error message.
///
/// jq truncates it through `jv_dump_string_trunc` with a 30-byte buffer: a dump
/// of 30 characters or more keeps its first 25, then `...`, then the dump's LAST
/// character so the bracket still closes. Measured against jq 1.8.2:
/// `[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15]` reports as
/// `[1,2,3,4,5,6,7,8,9,10,11,...]`, and a 26-character dump is not truncated.
fn paren_of(v: &JqVal) -> String {
    let s = render(v.bare());
    let n = s.chars().count();
    if n < 30 {
        return format!(" ({s})");
    }
    let head: String = s.chars().take(25).collect();
    let tail = s.chars().last().unwrap_or(' ');
    format!(" ({head}...{tail})")
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
        // reaches `index` when the object form is written out.
        (_, JqVal::Obj(m)) if m.len() == 2 && v.obj_get("start").is_none() => {
            let (s, e) = (
                m.iter()
                    .find(|(k, _)| &**k == "start")
                    .map(|(_, x)| x.clone()),
                m.iter()
                    .find(|(k, _)| &**k == "end")
                    .map(|(_, x)| x.clone()),
            );
            match (s, e) {
                (Some(s), Some(e)) => slice_value(v, &s, &e),
                _ => Err(index_err(v, idx)),
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

/// jq's slice: clamped, negative-from-the-end, over arrays and strings, with
/// `null` slicing to `null`.
fn slice_value(v: &JqVal, lo: &JqVal, hi: &JqVal) -> R<JqVal> {
    let bounds = |len: usize| -> (usize, usize) {
        let conv = |b: &JqVal, dflt: f64| -> f64 {
            match b.bare() {
                JqVal::Num(n, _) => {
                    let n = if *n < 0.0 { n + len as f64 } else { *n };
                    n.clamp(0.0, len as f64)
                }
                _ => dflt,
            }
        };
        let s = conv(lo, 0.0) as usize;
        let e = conv(hi, len as f64) as usize;
        (s, e.max(s))
    };
    match v.bare() {
        JqVal::Null => Ok(JqVal::Null),
        JqVal::Arr(a) => {
            let (s, e) = bounds(a.len());
            Ok(JqVal::arr(a[s..e].to_vec()))
        }
        JqVal::Str(s) => {
            // jq slices a string by CODE POINT, not by byte.
            let cs: Vec<char> = s.chars().collect();
            let (a, b) = bounds(cs.len());
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
                let times = if *n <= 0.0 { 0 } else { *n as usize };
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
                Ok(JqVal::num((xi % yi) as f64))
            }
            _ => Err(bad("divided")),
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
    if sep.is_empty() {
        return vec![JqVal::str(s)];
    }
    s.split(sep).map(JqVal::str).collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Strings, formats and object construction
// ─────────────────────────────────────────────────────────────────────────────

/// Build an interpolated string. Each `\(…)` is a GENERATOR, so `"\(1,2)"`
/// yields two strings; the pieces are walked left to right with the leftmost
/// interpolation varying slowest, which is the order jq emits.
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
        let Some(p) = b.pieces.get(i) else {
            return out(JqVal::str(acc));
        };
        match p {
            StrPiece::Lit(s) => {
                let next = format!("{acc}{s}");
                go(b, i + 1, &next, out)
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
                    let next = format!("{acc}{piece}");
                    go(b, i + 1, &next, out)
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
        0,
        "",
        out,
    )
}

/// jq's `@name` format strings.
fn apply_format(name: &str, v: &JqVal) -> R<String> {
    let v = v.bare();
    match name {
        "text" => Ok(render_raw(v)),
        "json" => Ok(render(v)),
        "base64" => Ok(b64_encode(render_raw(v).as_bytes())),
        "base64d" => {
            let raw = b64_decode(&render_raw(v))
                .ok_or_else(|| JqErr::msg(format!("{} is not valid base64 data", render(v))))?;
            Ok(String::from_utf8_lossy(&raw).into_owned())
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
        "csv" | "tsv" => {
            let JqVal::Arr(a) = v else {
                return Err(JqErr::msg(format!(
                    "{}{} cannot be {}-formatted, only an array can be",
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
                    JqVal::Num(..) => render(e),
                    JqVal::Str(s) if name == "csv" => format!("\"{}\"", s.replace('"', "\"\"")),
                    JqVal::Str(s) => s
                        .replace('\\', "\\\\")
                        .replace('\t', "\\t")
                        .replace('\n', "\\n")
                        .replace('\r', "\\r"),
                    other => {
                        return Err(JqErr::msg(format!(
                            "{}{} is not valid in a {name} row",
                            other.type_name(),
                            paren_of(other)
                        )))
                    }
                });
            }
            Ok(cells.join(if name == "csv" { "," } else { "\t" }))
        }
        "html" => {
            let s = render_raw(v);
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                match c {
                    '&' => out.push_str("&amp;"),
                    '<' => out.push_str("&lt;"),
                    '>' => out.push_str("&gt;"),
                    '\'' => out.push_str("&apos;"),
                    '"' => out.push_str("&quot;"),
                    c => out.push(c),
                }
            }
            Ok(out)
        }
        "sh" => {
            let one = |x: &JqVal| -> R<String> {
                match x {
                    JqVal::Str(s) => Ok(format!("'{}'", s.replace('\'', r"'\''"))),
                    JqVal::Null | JqVal::Bool(_) | JqVal::Num(..) => Ok(render(x)),
                    other => Err(JqErr::msg(format!(
                        "{}{} can not be escaped for shell",
                        other.type_name(),
                        paren_of(other)
                    ))),
                }
            };
            match v {
                JqVal::Arr(a) => Ok(a.iter().map(one).collect::<R<Vec<_>>>()?.join(" ")),
                other => one(other),
            }
        }
        other => Err(JqErr::msg(format!("{other} is not a valid format"))),
    }
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

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut acc = 0u32;
    let mut bits = 0u32;
    let mut out = Vec::new();
    for c in s.bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = B64.iter().position(|&x| x == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
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

/// Where a path expression's results go: the path itself and the value at it.
type PathSink<'a> = &'a mut dyn FnMut(Vec<JqVal>, JqVal) -> R<()>;

/// Evaluate `f` as a PATH expression — the subset of jq where every output is
/// reachable by a sequence of key/index steps from the input. This is what
/// `path`, `del`, `paths`, `pick` and every assignment operator are built on.
///
/// `input` is the `.` that index/condition sub-filters see; `val` is the value
/// reached so far and `pre` the path that reached it.
fn eval_paths(
    it: &Interp,
    f: &Filter,
    input: &JqVal,
    pre: &[JqVal],
    val: &JqVal,
    env: &Env,
    out: PathSink,
) -> R<()> {
    match f {
        Filter::Identity => out(pre.to_vec(), val.clone()),
        Filter::RecurseDefault => recurse_paths(pre, val, out),
        Filter::Field(base, name) => eval_paths(it, base, input, pre, val, env, &mut |p, v| {
            let key = JqVal::Str(name.clone());
            let next = index_value(&v, &key)?;
            let mut np = p;
            np.push(key);
            out(np, next)
        }),
        Filter::Index(base, idx) => eval_paths(it, base, input, pre, val, env, &mut |p, v| {
            eval(it, idx, input, env, &mut |i| {
                let next = index_value(&v, &i)?;
                let mut np = p.clone();
                np.push(i);
                out(np, next)
            })
        }),
        Filter::Slice(base, lo, hi) => eval_paths(it, base, input, pre, val, env, &mut |p, v| {
            eval_opt(it, lo.as_deref(), input, env, &mut |l| {
                eval_opt(it, hi.as_deref(), input, env, &mut |h| {
                    let next = slice_value(&v, &l, &h)?;
                    let mut np = p.clone();
                    np.push(JqVal::obj(vec![
                        (Rc::from("start"), l.clone()),
                        (Rc::from("end"), h.clone()),
                    ]));
                    out(np, next)
                })
            })
        }),
        Filter::Iterate(base) => {
            eval_paths(it, base, input, pre, val, env, &mut |p, v| match v.bare() {
                JqVal::Arr(a) => {
                    for (i, e) in a.iter().enumerate() {
                        let mut np = p.clone();
                        np.push(JqVal::num(i as f64));
                        out(np, e.clone())?;
                    }
                    Ok(())
                }
                JqVal::Obj(m) => {
                    for (k, e) in m.iter() {
                        let mut np = p.clone();
                        np.push(JqVal::Str(k.clone()));
                        out(np, e.clone())?;
                    }
                    Ok(())
                }
                JqVal::Null => Ok(()),
                other => Err(JqErr::msg(format!(
                    "Cannot iterate over {}{}",
                    other.type_name(),
                    paren_of(other)
                ))),
            })
        }
        Filter::Pipe(a, b) => eval_paths(it, a, input, pre, val, env, &mut |p, v| {
            eval_paths(it, b, &v, &p, &v, env, out)
        }),
        Filter::Comma(a, b) => {
            eval_paths(it, a, input, pre, val, env, out)?;
            eval_paths(it, b, input, pre, val, env, out)
        }
        Filter::Optional(inner) | Filter::Try(inner, None) => {
            match eval_paths(it, inner, input, pre, val, env, &mut |p, v| {
                out(p, v).map_err(wrap_downstream)
            }) {
                Ok(()) => Ok(()),
                Err(e) => match unwrap_downstream(e) {
                    Ok(real) => Err(real),
                    Err(JqErr::Err(_)) => Ok(()),
                    Err(other) => Err(other),
                },
            }
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
        Filter::Alt(a, b) => {
            let mut any = false;
            let r = eval_paths(it, a, input, pre, val, env, &mut |p, v| {
                if v.truthy() {
                    any = true;
                    out(p, v).map_err(wrap_downstream)
                } else {
                    Ok(())
                }
            });
            if let Err(e) = r {
                match unwrap_downstream(e) {
                    Ok(real) => return Err(real),
                    Err(JqErr::Err(_)) => {}
                    Err(other) => return Err(other),
                }
            }
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
        Filter::Call(name, args) => eval_call_paths(it, (name, args), input, pre, val, env, out),
        // `path(1)` and friends: jq reports the literal it could not turn into a
        // path rather than silently answering.
        other => Err(JqErr::msg(format!(
            "Invalid path expression with result {}",
            path_expr_label(it, other, val, env)
        ))),
    }
}

/// The value jq names in an "Invalid path expression" message: the first thing
/// the offending filter produces.
fn path_expr_label(it: &Interp, f: &Filter, val: &JqVal, env: &Env) -> String {
    let mut first = None;
    let _ = eval(it, f, val, env, &mut |v| {
        if first.is_none() {
            first = Some(v);
        }
        Ok(())
    });
    first.map_or_else(|| "null".to_string(), |v| render(&v))
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
    pre: &[JqVal],
    val: &JqVal,
    env: &Env,
    out: PathSink,
) -> R<()> {
    let Some(((cond, then), rest)) = node.arms.split_first() else {
        return match node.els {
            Some(e) => eval_paths(it, e, input, pre, val, env, out),
            None => out(pre.to_vec(), val.clone()),
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
    pre: &[JqVal],
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
                out(pre.to_vec(), val.clone())
            } else {
                Ok(())
            }
        }),
        ("getpath", 1) => eval(it, &args[0], val, env, &mut |p| {
            let JqVal::Arr(segs) = &p else {
                return Err(JqErr::msg("Path must be specified as an array"));
            };
            let mut np = pre.to_vec();
            np.extend(segs.iter().cloned());
            out(np, get_path(val, segs)?)
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
        ("last", 1) => {
            let mut acc = None;
            eval_paths(it, &args[0], input, pre, val, env, &mut |p, v| {
                acc = Some((p, v));
                Ok(())
            })?;
            match acc {
                Some((p, v)) => out(p, v),
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
            None => Err(JqErr::msg(format!(
                "Invalid path expression near attempt to call {name}"
            ))),
        },
    }
}

fn recurse_paths(pre: &[JqVal], val: &JqVal, out: PathSink) -> R<()> {
    out(pre.to_vec(), val.clone())?;
    match val {
        JqVal::Arr(a) => {
            for (i, e) in a.iter().enumerate() {
                let mut np = pre.to_vec();
                np.push(JqVal::num(i as f64));
                recurse_paths(&np, e, out)?;
            }
            Ok(())
        }
        JqVal::Obj(m) => {
            for (k, e) in m.iter() {
                let mut np = pre.to_vec();
                np.push(JqVal::Str(k.clone()));
                recurse_paths(&np, e, out)?;
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
    pre: &[JqVal],
    val: &JqVal,
    env: &Env,
    out: PathSink,
) -> R<()> {
    out(pre.to_vec(), val.clone())?;
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
                        "Cannot index {} with \"{k}\"",
                        other.type_name()
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
                        "Cannot index {} with number",
                        other.type_name()
                    )))
                }
            };
            let mut i = n.trunc();
            if i < 0.0 {
                i += a.len() as f64;
                if i < 0.0 {
                    return Err(JqErr::msg("Out of bounds negative array index"));
                }
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
                other => {
                    return Err(JqErr::msg(format!(
                        "Cannot update field at object index of {}",
                        other.type_name()
                    )))
                }
            };
            let len = a.len();
            let conv = |b: &JqVal, dflt: f64| match b.bare() {
                JqVal::Num(n, _) => {
                    let n = if *n < 0.0 { n + len as f64 } else { *n };
                    n.clamp(0.0, len as f64) as usize
                }
                _ => dflt as usize,
            };
            let s = conv(&lo, 0.0);
            let e = conv(&hi, len as f64).max(s);
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

/// Remove one path. Deleting from `null` is a no-op, as jq's `delpaths` is.
fn del_path(v: &JqVal, segs: &[JqVal]) -> R<JqVal> {
    let Some((seg, rest)) = segs.split_first() else {
        return Ok(JqVal::Null);
    };
    // As in `set_path`: the container that survives a deletion keeps its own
    // comments and anchor.
    let rebox = |built: JqVal| match v.meta() {
        Some(m) => JqVal::wrap(built, m.clone()),
        None => built,
    };
    let v = v.bare();
    if rest.is_empty() {
        return match (v, seg.bare()) {
            (JqVal::Null, _) => Ok(JqVal::Null),
            (JqVal::Obj(m), JqVal::Str(k)) => Ok(rebox(JqVal::obj(
                m.iter().filter(|(ek, _)| ek != k).cloned().collect(),
            ))),
            (JqVal::Arr(a), JqVal::Num(n, _)) => {
                let mut i = n.trunc();
                if i < 0.0 {
                    i += a.len() as f64;
                }
                if i < 0.0 || i >= a.len() as f64 {
                    return Ok(v.clone());
                }
                let i = i as usize;
                Ok(rebox(JqVal::arr(
                    a.iter()
                        .enumerate()
                        .filter(|(j, _)| *j != i)
                        .map(|(_, e)| e.clone())
                        .collect(),
                )))
            }
            (JqVal::Arr(a), JqVal::Obj(_)) => {
                let len = a.len();
                let conv = |b: Option<&JqVal>, dflt: usize| match b.map(JqVal::bare) {
                    Some(JqVal::Num(n, _)) => {
                        let n = if *n < 0.0 { n + len as f64 } else { *n };
                        n.clamp(0.0, len as f64) as usize
                    }
                    _ => dflt,
                };
                let s = conv(seg.obj_get("start"), 0);
                let e = conv(seg.obj_get("end"), len).max(s);
                let mut out = a[..s].to_vec();
                out.extend(a[e..].iter().cloned());
                Ok(rebox(JqVal::arr(out)))
            }
            (other, _) => Err(JqErr::msg(format!(
                "Cannot delete field at index of {}",
                other.type_name()
            ))),
        };
    }
    let child = index_value(v, seg)?;
    if matches!(child.bare(), JqVal::Null) {
        return Ok(v.clone());
    }
    Ok(rebox(set_path(v, &segs[..1], del_path(&child, rest)?)?))
}

/// `delpaths`. Paths are removed LONGEST/LAST first so that deleting `.[0]` does
/// not shift the index of a sibling path that has not been deleted yet.
fn del_paths(v: &JqVal, mut paths: Vec<Vec<JqVal>>) -> R<JqVal> {
    paths.sort_by(|a, b| cmp_vals(&JqVal::arr(b.clone()), &JqVal::arr(a.clone())));
    paths.dedup_by(|a, b| {
        cmp_vals(&JqVal::arr(a.clone()), &JqVal::arr(b.clone())) == Ordering::Equal
    });
    let mut cur = v.clone();
    for p in paths {
        cur = del_path(&cur, &p)?;
    }
    Ok(cur)
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
    // yq's metadata assignment. Its own spelling is a postfix on a path
    // (`.a anchor = "x"`); arb's grammar is jq's, so the same edit is written
    // `.a | anchor = "x"`, and a bare `anchor = "x"` sets it on `.`. Both are
    // recognised here because `anchor` is a VALUE filter, not a path, and
    // `eval_paths` would otherwise refuse the left-hand side outright.
    if op == AssignOp::Set {
        if let Some((path, name)) = meta_assign_target(lhs) {
            return eval(it, rhs, input, env, &mut |rv| match path {
                None => out(set_meta(input, name, &rv)),
                Some(p) => {
                    let mut cur = input.clone();
                    let mut paths = Vec::new();
                    eval_paths(it, p, input, &[], input, env, &mut |pp, _| {
                        paths.push(pp);
                        Ok(())
                    })?;
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
        eval_paths(it, lhs, input, &[], input, env, &mut |p, _| {
            paths.push(p);
            Ok(())
        })?;
        for p in paths {
            let old = get_path(&cur, &p)?;
            let mut first = None;
            eval(it, rhs, &old, env, &mut |v| {
                if first.is_none() {
                    first = Some(v);
                }
                Ok(())
            })?;
            cur = match first {
                // jq's `_modify` DELETES a path whose update produced nothing.
                None => del_paths(&cur, vec![p])?,
                Some(v) => set_path(&cur, &p, v)?,
            };
        }
        return out(cur);
    }
    // The remaining forms evaluate the right-hand side against the ORIGINAL
    // input, once per output value, and each output value gives one result.
    eval(it, rhs, input, env, &mut |rv| {
        let mut cur = input.clone();
        let mut paths = Vec::new();
        eval_paths(it, lhs, input, &[], input, env, &mut |p, _| {
            paths.push(p);
            Ok(())
        })?;
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
        ("utf8bytelength", 0) => out(JqVal::num(want_str(input, "counted in bytes")?.len() as f64)),

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
                ks.sort_by(cmp_vals);
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
            out(parse_json(&s).map_err(|e| JqErr::msg(format!("{e} (while parsing '{s}')")))?)
        }
        ("tonumber", 0) => match input {
            JqVal::Num(..) => out(input.clone()),
            JqVal::Str(s) => match s.trim().parse::<f64>() {
                Ok(n) => out(JqVal::num(n)),
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
            want_str(input, "exploded")?
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
                let n = e
                    .as_f64()
                    .ok_or_else(|| JqErr::msg("Unicode codepoint must be numeric"))?;
                s.push(char::from_u32(n as u32).ok_or_else(|| {
                    JqErr::msg(format!("Invalid codepoint literal {}", fmt_num(n)))
                })?);
            }
            out(JqVal::str(s))
        }
        ("ascii_downcase", 0) => out(JqVal::str(
            want_str(input, "downcased")?.to_ascii_lowercase(),
        )),
        ("ascii_upcase", 0) => out(JqVal::str(want_str(input, "upcased")?.to_ascii_uppercase())),
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
        ("ltrim", 0) => out(JqVal::str(want_str(input, "trimmed")?.trim_start())),
        ("rtrim", 0) => out(JqVal::str(want_str(input, "trimmed")?.trim_end())),
        ("trim", 0) => out(JqVal::str(want_str(input, "trimmed")?.trim())),
        ("split", 1) => {
            let sep = one(it, &args[0], input, env)?;
            let s = want_str(input, "split")?;
            let sep = want_str(&sep, "used as a separator")?;
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
                while let Some(off) = h[from..].find(&*n) {
                    hits.push(JqVal::num((from + off) as f64));
                    from += off + 1;
                }
            }
            out(JqVal::arr(hits))
        }

        ("sort", 0) => {
            let a = want_arr(input, "sorted")?;
            let mut v = a.as_ref().clone();
            v.sort_by(cmp_vals);
            out(JqVal::arr(v))
        }
        ("reverse", 0) => match input {
            JqVal::Arr(a) => out(JqVal::arr(a.iter().rev().cloned().collect())),
            JqVal::Str(s) => out(JqVal::str(s.chars().rev().collect::<String>())),
            JqVal::Null => out(JqVal::arr(Vec::new())),
            other => Err(JqErr::msg(format!(
                "Cannot reverse {}{}",
                other.type_name(),
                paren_of(other)
            ))),
        },
        ("sort_by", 1) | ("group_by", 1) | ("unique_by", 1) => {
            let mut keyed = keyed_elements(it, &args[0], input, env)?;
            keyed.sort_by(|a, b| cmp_vals(&a.0, &b.0));
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
            let keyed = keyed_elements(it, &args[0], input, env)?;
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
            let a = want_arr(input, "reduced")?;
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
            let by = match args.get(2) {
                Some(a) => one(it, a, input, env)?,
                None => JqVal::num(1.0),
            };
            let (f, u, b) = (
                from.as_f64()
                    .ok_or_else(|| JqErr::msg("Range bounds must be numeric"))?,
                upto.as_f64()
                    .ok_or_else(|| JqErr::msg("Range bounds must be numeric"))?,
                by.as_f64()
                    .ok_or_else(|| JqErr::msg("Range bounds must be numeric"))?,
            );
            if b == 0.0 {
                return Ok(());
            }
            let mut x = f;
            while if b > 0.0 { x < u } else { x > u } {
                out(JqVal::num(x))?;
                x += b;
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
                "round" | "nearbyint" => n.round(),
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
                // `rint` rounds half to EVEN under the default rounding mode,
                // which is not `f64::round`'s half-away-from-zero.
                "rint" => n.round_ties_even(),
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
            let a = one(it, &args[0], input, env)?
                .as_f64()
                .ok_or_else(|| JqErr::msg(format!("{name} requires numbers")))?;
            let b = one(it, &args[1], input, env)?
                .as_f64()
                .ok_or_else(|| JqErr::msg(format!("{name} requires numbers")))?;
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
            let g = |i: usize| -> R<f64> {
                one(it, &args[i], input, env)?
                    .as_f64()
                    .ok_or_else(|| JqErr::msg("fma requires numbers"))
            };
            out(JqVal::num(g(0)?.mul_add(g(1)?, g(2)?)))
        }
        ("infinite", 0) => out(JqVal::num(f64::INFINITY)),
        ("nan", 0) => out(JqVal::num(f64::NAN)),
        ("isnan", 0) => out(JqVal::Bool(input.as_f64().is_some_and(f64::is_nan))),
        ("isinfinite", 0) => out(JqVal::Bool(input.as_f64().is_some_and(f64::is_infinite))),
        ("isnormal", 0) => out(JqVal::Bool(input.as_f64().is_some_and(f64::is_normal))),

        ("path", 1) => {
            let mut res = Vec::new();
            eval_paths(it, &args[0], input, &[], input, env, &mut |p, _| {
                res.push(JqVal::arr(p));
                Ok(())
            })?;
            for p in res {
                out(p)?;
            }
            Ok(())
        }
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
                    _ => return Err(JqErr::msg("Path must be specified as an array")),
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
            let top: Vec<JqVal> = match input {
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
        ("builtins", 0) => out(JqVal::arr(
            builtin_names().into_iter().map(JqVal::str).collect(),
        )),
        ("input", 0) => match it.next_input() {
            Some(v) => out(v),
            None => Err(JqErr::msg("No more inputs")),
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
        ("have_decnum", 0) => out(JqVal::Bool(false)),
        ("$__loc__", 0) => out(JqVal::obj(vec![
            (Rc::from("file"), JqVal::str("<top-level>")),
            (Rc::from("line"), JqVal::num(1.0)),
        ])),

        ("now", 0) => out(JqVal::num(unix_now())),
        ("mktime", 0) => out(JqVal::num(mktime(input)? as f64)),
        ("gmtime", 0) | ("localtime", 0) => {
            let t = input
                .as_f64()
                .ok_or_else(|| JqErr::msg(format!("{name}() requires a number")))?;
            out(broken_down(t, name == "localtime"))
        }
        ("strftime", 1) | ("strflocaltime", 1) => {
            let f = one(it, &args[0], input, env)?;
            let f = want_str(&f, "used as a strftime format")?;
            out(JqVal::str(strftime_val(
                input,
                &f,
                name == "strflocaltime",
            )?))
        }
        ("strptime", 1) => {
            let f = one(it, &args[0], input, env)?;
            let f = want_str(&f, "used as a strptime format")?;
            let s = want_str(input, "parsed as a date")?;
            out(strptime_val(&s, &f)?)
        }

        _ => Err(JqErr::msg(format!("{name}/{} is not defined", args.len()))),
    }
}

fn want_arr(v: &JqVal, who: &str) -> R<Rc<Vec<JqVal>>> {
    match v {
        JqVal::Arr(a) => Ok(a.clone()),
        other => Err(JqErr::msg(format!(
            "{}{} cannot be {who}, as it is not an array",
            other.type_name(),
            paren_of(other)
        ))),
    }
}

/// Pair every element of the input array with `[f]` evaluated over it — the key
/// array jq's `_sort_by_impl` family sorts on.
fn keyed_elements(it: &Interp, f: &Filter, input: &JqVal, env: &Env) -> R<Vec<(JqVal, JqVal)>> {
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
        match e {
            JqVal::Arr(inner) if depth > 0 => flatten_into(inner, depth - 1, out),
            other => out.push(other.clone()),
        }
    }
}

/// jq's `contains`: recursive containment. Strings contain substrings, arrays
/// contain element-wise-contained elements, objects contain per-key.
fn contains(a: &JqVal, b: &JqVal) -> R<bool> {
    let (a, b) = (a.bare(), b.bare());
    Ok(match (a, b) {
        (JqVal::Obj(_), JqVal::Obj(bm)) => {
            for (k, bv) in bm.iter() {
                match a.obj_get(k) {
                    Some(av) if contains(av, bv)? => {}
                    _ => return Ok(false),
                }
            }
            true
        }
        (JqVal::Arr(aa), JqVal::Arr(ba)) => {
            for bv in ba.iter() {
                let mut hit = false;
                for av in aa.iter() {
                    if contains(av, bv)? {
                        hit = true;
                        break;
                    }
                }
                if !hit {
                    return Ok(false);
                }
            }
            true
        }
        (JqVal::Str(x), JqVal::Str(y)) => x.contains(&**y),
        (x, y) if x.type_name() == y.type_name() => eq_vals(x, y),
        (x, y) => {
            return Err(JqErr::msg(format!(
                "{}{} and {}{} cannot have their containment checked",
                x.type_name(),
                paren_of(x),
                y.type_name(),
                paren_of(y)
            )))
        }
    })
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
            's' => prefix.push('s'),
            'm' => prefix.push('s'),
            'p' => prefix.push_str("sm"),
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

fn re_args(re: &JqVal, flags: &JqVal) -> R<(Rc<str>, String)> {
    let pat = want_str(re, "matched, as it is not a string")?;
    let fl = match flags {
        JqVal::Null => String::new(),
        JqVal::Str(s) => s.to_string(),
        other => {
            return Err(JqErr::msg(format!(
                "{}{} is not a string",
                other.type_name(),
                paren_of(other)
            )))
        }
    };
    Ok((pat, fl))
}

/// jq's `_match_impl`: an array of match objects, or a boolean in test mode.
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
    for caps in rx.captures_iter(&s) {
        let whole = caps.get(0).expect("group 0 always participates");
        let mut cap_list = Vec::new();
        for (gi, name) in names.iter().enumerate().skip(1) {
            let (off, len, text) = match caps.get(gi) {
                Some(m) => (
                    cp_index(&s, m.start()) as f64,
                    m.as_str().chars().count() as f64,
                    JqVal::str(m.as_str()),
                ),
                None => (-1.0, 0.0, JqVal::Null),
            };
            cap_list.push(JqVal::obj(vec![
                (Rc::from("offset"), JqVal::num(off)),
                (Rc::from("length"), JqVal::num(len)),
                (Rc::from("string"), text),
                (Rc::from("name"), name.map_or(JqVal::Null, JqVal::str)),
            ]));
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
        if !global {
            break;
        }
    }
    Ok(JqVal::arr(hits))
}

/// jq's regex `split/2`: the pieces BETWEEN matches, always global.
fn regex_split(input: &JqVal, re: &JqVal, flags: &JqVal) -> R<JqVal> {
    let s = want_str(input, "split, as it is not a string")?;
    let (pat, fl) = re_args(re, flags)?;
    let (rx, _) = compile_re(&pat, &fl)?;
    let mut parts = Vec::new();
    let mut last = 0usize;
    for m in rx.find_iter(&s) {
        parts.push(JqVal::str(&s[last..m.start()]));
        last = m.end();
    }
    parts.push(JqVal::str(&s[last..]));
    Ok(JqVal::arr(parts))
}

/// Everything a `sub`/`gsub` rebuild carries unchanged from match to match.
struct SubBuild<'a> {
    it: &'a Interp,
    s: &'a str,
    spans: &'a [(usize, usize, JqVal)],
    repl: &'a Filter,
    env: &'a Env,
}

/// jq's `sub`/`gsub`. The replacement is a FILTER evaluated with the capture
/// object as `.`, and it is a generator — `"ab" | [sub("a"; "x","y")]` is
/// `["xb","yb"]` — so the matches are walked recursively and every combination
/// is emitted.
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
    for caps in rx.captures_iter(&s) {
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
        if !global {
            break;
        }
    }
    fn go(b: &SubBuild, i: usize, cursor: usize, acc: &str, out: Sink) -> R<()> {
        let Some((start, end, caps)) = b.spans.get(i) else {
            return out(JqVal::str(format!("{acc}{}", &b.s[cursor..])));
        };
        let head = format!("{acc}{}", &b.s[cursor..*start]);
        eval(b.it, b.repl, caps, b.env, &mut |r| {
            let piece = want_str(&r, "used as a replacement")?;
            let next = format!("{head}{piece}");
            go(b, i + 1, *end, &next, out)
        })
    }
    go(
        &SubBuild {
            it,
            s: &s,
            spans: &spans,
            repl,
            env,
        },
        0,
        0,
        "",
        out,
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Date builtins
// ─────────────────────────────────────────────────────────────────────────────

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// jq's broken-down time: `[year, month0, mday, hour, min, sec, wday, yday]`,
/// where `sec` carries the sub-second fraction of the input.
fn broken_down(t: f64, local: bool) -> JqVal {
    let secs = t.floor() as i64;
    let frac = t - t.floor();
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let tt = secs as libc::time_t;
    unsafe {
        if local {
            libc::localtime_r(&tt, &mut tm);
        } else {
            libc::gmtime_r(&tt, &mut tm);
        }
    }
    JqVal::arr(vec![
        JqVal::num(f64::from(tm.tm_year) + 1900.0),
        JqVal::num(f64::from(tm.tm_mon)),
        JqVal::num(f64::from(tm.tm_mday)),
        JqVal::num(f64::from(tm.tm_hour)),
        JqVal::num(f64::from(tm.tm_min)),
        JqVal::num(f64::from(tm.tm_sec) + frac),
        JqVal::num(f64::from(tm.tm_wday)),
        JqVal::num(f64::from(tm.tm_yday)),
    ])
}

fn to_tm(v: &JqVal) -> R<libc::tm> {
    let JqVal::Arr(a) = v else {
        return Err(JqErr::msg("not a valid time"));
    };
    if a.len() < 6 {
        return Err(JqErr::msg("not a valid time"));
    }
    let g = |i: usize| a[i].as_f64().unwrap_or(0.0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = (g(0) - 1900.0) as i32;
    tm.tm_mon = g(1) as i32;
    tm.tm_mday = g(2) as i32;
    tm.tm_hour = g(3) as i32;
    tm.tm_min = g(4) as i32;
    tm.tm_sec = g(5) as i32;
    tm.tm_wday = a.get(6).and_then(JqVal::as_f64).unwrap_or(0.0) as i32;
    tm.tm_yday = a.get(7).and_then(JqVal::as_f64).unwrap_or(0.0) as i32;
    Ok(tm)
}

fn mktime(v: &JqVal) -> R<i64> {
    if !matches!(v, JqVal::Arr(_)) {
        return Err(JqErr::msg("mktime requires array of 6 numbers"));
    }
    let mut tm = to_tm(v)?;
    // `timegm` is the UTC counterpart of `mktime`; jq uses it so a broken-down
    // time round-trips through `gmtime` exactly.
    Ok(unsafe { libc::timegm(&mut tm) })
}

fn strftime_val(v: &JqVal, fmt: &str, local: bool) -> R<String> {
    let tm = match v {
        JqVal::Num(n, _) => {
            let bd = broken_down(*n, local);
            to_tm(&bd)?
        }
        JqVal::Arr(_) => to_tm(v)?,
        other => {
            return Err(JqErr::msg(format!(
                "strftime/1 requires parsed datetime inputs, got {}{}",
                other.type_name(),
                paren_of(other)
            )))
        }
    };
    let cfmt = std::ffi::CString::new(fmt).map_err(|_| JqErr::msg("bad format string"))?;
    let mut buf = vec![0u8; 512];
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), cfmt.as_ptr(), &tm) };
    buf.truncate(n);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn strptime_val(s: &str, fmt: &str) -> R<JqVal> {
    let cs = std::ffi::CString::new(s).map_err(|_| JqErr::msg("bad date string"))?;
    let cf = std::ffi::CString::new(fmt).map_err(|_| JqErr::msg("bad format string"))?;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let end = unsafe { libc::strptime(cs.as_ptr(), cf.as_ptr(), &mut tm) };
    if end.is_null() {
        return Err(JqErr::msg(format!(
            "date \"{s}\" does not match format \"{fmt}\""
        )));
    }
    // `strptime` leaves wday/yday unset on most platforms; jq normalizes through
    // `timegm`+`gmtime` so the two trailing fields are always correct.
    let secs = unsafe { libc::timegm(&mut tm.clone()) };
    Ok(broken_down(secs as f64, false))
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
def add(f): reduce (.[]|f) as $x (null; . + $x);
def join($x): reduce .[] as $i (null;
    (if . == null then "" else . + $x end) +
    ($i | if . == null then "" elif type == "string" then . else tojson end)) // "";
def flatten: _flatten(1e9);
def flatten($x): _flatten($x);
def ltrimstr($left): if ($left|type) == "string" and startswith($left) then .[($left|length):] else . end;
def rtrimstr($right): if ($right|type) == "string" and endswith($right) then .[:length - ($right|length)] else . end;
def range($x): range(0; $x);
def isempty(g): label $go | (g|false, break $go), true;
def first(f): label $out | (f | ., break $out);
def first: .[0];
def last(f): reduce f as $x (null; $x);
def last: .[-1];
def any: reduce .[] as $x (false; . or $x);
def all: reduce .[] as $x (true; . and $x);
def any(y): reduce (.[]|y) as $x (false; . or $x);
def all(y): reduce (.[]|y) as $x (true; . and $x);
def any(g; y): isempty(first(g|select(y))) | not;
def all(g; y): isempty(first(g|y|select(.|not)));
def limit($n; f): if $n > 0 then label $out | foreach f as $item (0; .+1; $item, if . >= $n then break $out else empty end)
                  elif $n == 0 then empty
                  else f end;
def nth($n): .[$n];
def nth($n; f): if $n < 0 then error("Out of bounds negative array index") else last(limit($n + 1; f)) end;
def until(cond; update): def _until: if cond then . else (update | _until) end; _until;
def while(cond; update): def _while: if cond then ., (update | _while) else empty end; _while;
def repeat(f): def _repeat: f | (., _repeat); _repeat;
def in(xs): . as $x | xs | has($x);
def inside(xs): . as $x | xs | contains($x);
def combinations: if length == 0 then [] else .[0][] as $x | (.[1:] | combinations) as $w | [$x] + $w end;
def combinations(n): . as $dot | [range(n)] | map($dot) | combinations;
def map_values(f): .[] |= f;
def walk(f): def w: if type == "object" then map_values(w) elif type == "array" then map(w) else . end | f; w;
def unique: unique_by(.);
def del(f): delpaths([path(f)]);
def paths: path(..) | select(length > 0);
def paths(node_filter): . as $dot | paths | select(. as $p | $dot | getpath($p) | node_filter);
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
def skip($n; f): foreach f as $item (-1; . + 1; if . >= $n then $item else empty end);
def JOIN($idx; idx_expr): [.[] | [., $idx[idx_expr]]];
def JOIN($idx; stream; idx_expr): stream | [., $idx[idx_expr]];
def JOIN($idx; stream; idx_expr; join_expr): stream | [., $idx[idx_expr]] | join_expr;
def bsearch($target):
  if length == 0 then -1
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
def abs: if type == "number" and . < 0 then - . else . end;
def isvalid(f): try (f|true) catch false;
def indices($i): if type == "array" and ($i|type) == "array" then .[$i]
                 elif type == "array" then .[[$i]]
                 elif ($i|type) == "string" then _strindices($i)
                 else .[[$i]] end;
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
def scan($re; $flags): match($re; "g" + ($flags // ""))
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
/// yq spells the assignment as a postfix on a path (`.a anchor = "x"`); arb's
/// grammar is jq's, so the same edit is written `.a |= (anchor = "x")` or
/// `(.a | anchor) = "x"`. Both reach here through [`eval_assign`].
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
            eval_paths(it, &args[0], input, &[], input, env, &mut |p, _| {
                paths.push(p);
                Ok(())
            })?;
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
        ("to_string", 0) => out(JqVal::str(render_raw(input))),
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
        Filter::Field(a, _) | Filter::Iterate(a) | Filter::Optional(a) | Filter::Neg(a) => {
            check_names(a, funcs, vars)
        }
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
        Filter::Field(a, _) | Filter::Iterate(a) | Filter::Optional(a) | Filter::Neg(a) => {
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
