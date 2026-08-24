//! jq VALUE expressions — the bodies of `select(…)` / `map(…)` and a bare
//! arithmetic stage such as `.a + .b`.
//!
//! # Why this is not `crate::expr`
//!
//! arb's own expression language (SPEC §6) is f64-only: every value is a double,
//! a comparison yields 0/1, and an unresolvable field reads as NaN. That model is
//! right for arb's native `where`/`map` over a numeric line stream and WRONG for
//! jq, whose front-end promises jq's answers (SPEC §8). Routing jq bodies through
//! the f64 evaluator produced five distinct classes of divergence, all measured
//! against `jq 1.8.2`:
//!
//! | probe | jq | f64 evaluator |
//! |---|---|---|
//! | `[1,2,3] \| map(. > 1)` | `[false,true,true]` | `[0,1,1]` |
//! | `{"a":0} \| select(.a)` | `{"a":0}` (only `false`/`null` are falsy) | dropped |
//! | `{"a":"1"} \| select(.a == 1)` | dropped (`==` is type-strict) | kept |
//! | `{"a":"x","b":"y"} \| .a + .b` | `xy` | `null` (NaN) |
//! | `{"a":1} \| . + 3` | *error* — object and number | `null`, exit 0 |
//!
//! The last row is the one SPEC §8 rules out by name: a construct outside the
//! documented subset must be a hard error, "never silently reinterpreted".
//!
//! So this module evaluates over `serde_json::Value` — jq's own model — and
//! returns `Err` exactly where jq raises. The port target is jq 1.8.2's
//! `src/jv.c` / `src/execute.c` binary-operator table; every rule below was
//! additionally re-measured against that binary rather than taken from memory.
//!
//! Numbers are still f64 (SPEC §6 is unchanged); only the TYPE lattice widens.

use serde_json::{Map, Value};

/// One step of a jq path: an object key or an array index.
#[derive(Debug, Clone, PartialEq)]
pub enum Seg {
    Key(String),
    /// May be negative — jq counts `.[-1]` from the end.
    Index(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
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
    And,
    Or,
}

/// A jq value expression. Deliberately small: this is the subset SPEC §8 claims
/// for `select`/`map` bodies, not jq's full grammar. Anything else fails to
/// parse, which is how the "hard error" half of the contract is honoured.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// `.`, `.a`, `.a.b[0]`, `.["k"]` — a path relative to the current value.
    /// An empty segment list is the identity `.`.
    Path(Vec<Seg>),
    /// A JSON literal: a number, a string, `true`, `false` or `null`.
    Lit(Value),
    /// `[a, b, …]` — array construction over literal/path elements.
    Arr(Vec<Expr>),
    Neg(Box<Expr>),
    Bin(Op, Box<Expr>, Box<Expr>),
}

// ---- parser ---------------------------------------------------------------

struct P {
    c: Vec<char>,
    i: usize,
}

/// Parse a jq value expression. Errors are already `jq: …`-prefixed by the
/// caller, so the text here names only what went wrong.
pub fn parse(src: &str) -> Result<Expr, String> {
    let mut p = P {
        c: src.chars().collect(),
        i: 0,
    };
    let e = p.or_expr()?;
    p.ws();
    if p.i < p.c.len() {
        return Err(format!("unsupported expression `{src}`"));
    }
    Ok(e)
}

impl P {
    fn ws(&mut self) {
        while self.i < self.c.len() && self.c[self.i].is_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }

    /// Consume `lit` if it is next (after whitespace).
    fn eat(&mut self, lit: &str) -> bool {
        self.ws();
        let n = lit.chars().count();
        if self.c[self.i..].starts_with(&lit.chars().collect::<Vec<_>>()[..]) {
            self.i += n;
            return true;
        }
        false
    }

    /// Consume the word `w` only when it stands alone — `and` is an operator but
    /// `android` is not, and `.android` must not be split at the keyword either.
    fn eat_word(&mut self, w: &str) -> bool {
        self.ws();
        let save = self.i;
        if self.eat(w) {
            let next = self.peek();
            if !matches!(next, Some(c) if c.is_alphanumeric() || c == '_') {
                return true;
            }
            self.i = save;
        }
        false
    }

    fn or_expr(&mut self) -> Result<Expr, String> {
        let mut l = self.and_expr()?;
        while self.eat_word("or") {
            let r = self.and_expr()?;
            l = Expr::Bin(Op::Or, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn and_expr(&mut self) -> Result<Expr, String> {
        let mut l = self.cmp_expr()?;
        while self.eat_word("and") {
            let r = self.cmp_expr()?;
            l = Expr::Bin(Op::And, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    /// jq's comparison operators are non-associative, so exactly one may appear.
    fn cmp_expr(&mut self) -> Result<Expr, String> {
        let l = self.add_expr()?;
        self.ws();
        // Two-char forms first: `<=` must not be read as `<` then a stray `=`.
        for (lit, op) in [
            ("==", Op::Eq),
            ("!=", Op::Ne),
            ("<=", Op::Le),
            (">=", Op::Ge),
            ("<", Op::Lt),
            (">", Op::Gt),
        ] {
            if self.eat(lit) {
                let r = self.add_expr()?;
                return Ok(Expr::Bin(op, Box::new(l), Box::new(r)));
            }
        }
        Ok(l)
    }

    fn add_expr(&mut self) -> Result<Expr, String> {
        let mut l = self.mul_expr()?;
        loop {
            self.ws();
            let op = if self.eat("+") {
                Op::Add
            } else if self.peek() == Some('-') {
                self.i += 1;
                Op::Sub
            } else {
                return Ok(l);
            };
            let r = self.mul_expr()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn mul_expr(&mut self) -> Result<Expr, String> {
        let mut l = self.unary()?;
        loop {
            self.ws();
            let op = if self.eat("*") {
                Op::Mul
            } else if self.eat("%") {
                Op::Mod
            } else if self.peek() == Some('/') {
                // `//` is jq's alternative operator, which SPEC §8 lists as
                // out-of-subset. Reading it as two divisions would turn a
                // refusal into a parse of something else entirely.
                if self.c.get(self.i + 1) == Some(&'/') {
                    return Err("`//` (alternative operator) is not supported".into());
                }
                self.i += 1;
                Op::Div
            } else {
                return Ok(l);
            };
            let r = self.unary()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn unary(&mut self) -> Result<Expr, String> {
        self.ws();
        if self.peek() == Some('-') {
            self.i += 1;
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, String> {
        self.ws();
        let c = self.peek().ok_or("unexpected end of expression")?;
        match c {
            '(' => {
                self.i += 1;
                let e = self.or_expr()?;
                if !self.eat(")") {
                    return Err("unclosed `(`".into());
                }
                Ok(e)
            }
            '[' => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.peek() != Some(']') {
                    loop {
                        items.push(self.or_expr()?);
                        if !self.eat(",") {
                            break;
                        }
                    }
                }
                if !self.eat("]") {
                    return Err("unclosed `[`".into());
                }
                Ok(Expr::Arr(items))
            }
            '"' => Ok(Expr::Lit(Value::String(self.string()?))),
            '.' => self.path(),
            d if d.is_ascii_digit() => self.number(),
            _ => {
                // A bareword. Only jq's three value keywords are values; every
                // other word (`reduce`, `paths`, `env`, `not`, …) is a jq
                // construct arb does not implement, and must refuse rather than
                // resolve to something.
                let w = self.word();
                match w.as_str() {
                    "true" => Ok(Expr::Lit(Value::Bool(true))),
                    "false" => Ok(Expr::Lit(Value::Bool(false))),
                    "null" => Ok(Expr::Lit(Value::Null)),
                    "" => Err(format!("unexpected `{c}`")),
                    other => Err(format!("`{other}` is not supported")),
                }
            }
        }
    }

    fn word(&mut self) -> String {
        let start = self.i;
        while self.i < self.c.len() && (self.c[self.i].is_alphanumeric() || self.c[self.i] == '_') {
            self.i += 1;
        }
        self.c[start..self.i].iter().collect()
    }

    /// A double-quoted JSON string. Handed to serde_json so the escape rules are
    /// JSON's, not a hand-rolled approximation.
    fn string(&mut self) -> Result<String, String> {
        let start = self.i;
        self.i += 1; // opening quote
        while self.i < self.c.len() {
            match self.c[self.i] {
                '\\' => self.i += 2,
                '"' => {
                    self.i += 1;
                    let raw: String = self.c[start..self.i].iter().collect();
                    return serde_json::from_str::<String>(&raw)
                        .map_err(|_| format!("bad string literal {raw}"));
                }
                _ => self.i += 1,
            }
        }
        Err("unterminated string".into())
    }

    fn number(&mut self) -> Result<Expr, String> {
        let start = self.i;
        while self.i < self.c.len()
            && (self.c[self.i].is_ascii_digit() || matches!(self.c[self.i], '.' | 'e' | 'E'))
        {
            // An exponent may carry a sign; a `-` anywhere else ends the literal.
            if matches!(self.c[self.i], 'e' | 'E')
                && matches!(self.c.get(self.i + 1), Some('+' | '-'))
            {
                self.i += 2;
                continue;
            }
            self.i += 1;
        }
        let s: String = self.c[start..self.i].iter().collect();
        s.parse::<f64>()
            .map(|n| Expr::Lit(num(n)))
            .map_err(|_| format!("bad number `{s}`"))
    }

    /// `.`, `.a`, `.a.b`, `.[0]`, `.["k"]`, `.a[1]["b"]`.
    fn path(&mut self) -> Result<Expr, String> {
        let mut segs = Vec::new();
        self.i += 1; // the leading '.'
        if self.peek() == Some('.') {
            return Err("`..` (recursive descent) is not supported".into());
        }
        loop {
            match self.peek() {
                Some(c) if c.is_alphabetic() || c == '_' => segs.push(Seg::Key(self.word())),
                Some('[') => {
                    self.i += 1;
                    self.ws();
                    if self.peek() == Some('"') {
                        segs.push(Seg::Key(self.string()?));
                    } else {
                        let start = self.i;
                        while self.i < self.c.len() && self.c[self.i] != ']' {
                            self.i += 1;
                        }
                        let raw: String = self.c[start..self.i].iter().collect();
                        let n = raw
                            .trim()
                            .parse::<i64>()
                            .map_err(|_| format!("unsupported subscript `[{raw}]`"))?;
                        segs.push(Seg::Index(n));
                    }
                    if !self.eat("]") {
                        return Err("unclosed `[`".into());
                    }
                }
                _ => break,
            }
            // A further `.a` continues the path; anything else ends it.
            if self.peek() == Some('.')
                && matches!(self.c.get(self.i + 1), Some(c) if c.is_alphabetic() || *c == '_')
            {
                self.i += 1;
                continue;
            }
            if self.peek() == Some('[') {
                continue;
            }
            break;
        }
        Ok(Expr::Path(segs))
    }
}

// ---- evaluator ------------------------------------------------------------

/// Render a COMPUTED jq value as one output line, the way `jq -rc` prints it: a
/// string raw, everything else compact JSON.
///
/// Every number goes through `crate::query::fmt_num`, including the ones
/// nested inside a constructed array or object. That is the same invariant the
/// scalar path already keeps — `fmt_num` is the one place an arb number becomes
/// text (SPEC §6) — and letting `serde_json` print a nested number instead runs
/// it through a second, different float formatter: a literal `[1,2]` came back
/// out as `[1.0,2.0]`, because a computed number is always an f64 while a parsed
/// one keeps its integer representation.
pub fn render(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => compact(other),
    }
}

/// Compact JSON with `fmt_num` numbers.
fn compact(v: &Value) -> String {
    match v {
        Value::Number(n) => crate::query::fmt_num(n.as_f64().unwrap_or(0.0)),
        Value::Array(a) => {
            let parts: Vec<String> = a.iter().map(compact).collect();
            format!("[{}]", parts.join(","))
        }
        Value::Object(m) => {
            let parts: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{}:{}", Value::String(k.clone()), compact(v)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        other => other.to_string(),
    }
}

/// jq's type names, as they appear in its error messages.
pub fn tname(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// `type (value)` — the operand rendering jq uses inside a binary-op error.
fn show(v: &Value) -> String {
    format!("{} ({})", tname(v), v)
}

/// Build a JSON number from an f64. A non-finite double has no JSON spelling, so
/// it becomes `null` — the same mapping `fmt_num` applies on the render side.
fn num(n: f64) -> Value {
    serde_json::Number::from_f64(n)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// jq truthiness: everything except `false` and `null` is true. Notably `0`,
/// `""`, `[]` and `{}` are all TRUTHY, which is where the f64 evaluator (`0` is
/// false) disagreed.
pub fn truthy(v: &Value) -> bool {
    !matches!(v, Value::Null | Value::Bool(false))
}

/// Follow a path from `cur`. A miss is `null` (jq makes an absent key, an
/// out-of-range index and an explicit null indistinguishable); a TYPE mismatch
/// is an error, which is the half arb used to answer `null` to.
pub fn get_path(cur: &Value, segs: &[Seg]) -> Result<Value, String> {
    let mut v = cur.clone();
    for s in segs {
        v = match (&v, s) {
            (Value::Object(m), Seg::Key(k)) => m.get(k).cloned().unwrap_or(Value::Null),
            (Value::Null, _) => Value::Null,
            (Value::Array(a), Seg::Index(n)) => {
                let i = if *n < 0 { a.len() as i64 + n } else { *n };
                usize::try_from(i)
                    .ok()
                    .and_then(|i| a.get(i).cloned())
                    .unwrap_or(Value::Null)
            }
            (other, Seg::Key(k)) => {
                return Err(format!("Cannot index {} with \"{k}\"", tname(other)));
            }
            (other, Seg::Index(n)) => {
                return Err(format!("Cannot index {} with number ({n})", tname(other)));
            }
        };
    }
    Ok(v)
}

pub fn eval(e: &Expr, cur: &Value) -> Result<Value, String> {
    match e {
        Expr::Path(segs) => get_path(cur, segs),
        Expr::Lit(v) => Ok(v.clone()),
        Expr::Arr(items) => items
            .iter()
            .map(|it| eval(it, cur))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Expr::Neg(inner) => {
            let v = eval(inner, cur)?;
            match v.as_f64() {
                Some(n) if v.is_number() => Ok(num(-n)),
                _ => Err(format!("{} cannot be negated", show(&v))),
            }
        }
        // `and`/`or` short-circuit and yield a BOOLEAN, not their operand.
        Expr::Bin(Op::And, a, b) => {
            if !truthy(&eval(a, cur)?) {
                return Ok(Value::Bool(false));
            }
            Ok(Value::Bool(truthy(&eval(b, cur)?)))
        }
        Expr::Bin(Op::Or, a, b) => {
            if truthy(&eval(a, cur)?) {
                return Ok(Value::Bool(true));
            }
            Ok(Value::Bool(truthy(&eval(b, cur)?)))
        }
        Expr::Bin(op, a, b) => {
            let (l, r) = (eval(a, cur)?, eval(b, cur)?);
            binop(*op, &l, &r)
        }
    }
}

pub fn binop(op: Op, l: &Value, r: &Value) -> Result<Value, String> {
    use std::cmp::Ordering;
    let ord = || cmp(l, r);
    match op {
        // Equality goes through the same total order as `<`, NOT through
        // `Value`'s derived `PartialEq`: serde_json keeps a number's source
        // representation, so the parsed `1` (u64) and the computed `1.0` (f64)
        // are unequal as `Value`s while jq calls them the same number.
        Op::Eq => return Ok(Value::Bool(ord() == Ordering::Equal)),
        Op::Ne => return Ok(Value::Bool(ord() != Ordering::Equal)),
        Op::Lt => return Ok(Value::Bool(ord() == Ordering::Less)),
        Op::Le => return Ok(Value::Bool(ord() != Ordering::Greater)),
        Op::Gt => return Ok(Value::Bool(ord() == Ordering::Greater)),
        Op::Ge => return Ok(Value::Bool(ord() != Ordering::Less)),
        Op::And | Op::Or => unreachable!("short-circuited in eval"),
        _ => {}
    }
    match (op, l, r) {
        // `+` — null is the identity on BOTH sides, then per-type concatenation.
        (Op::Add, Value::Null, x) | (Op::Add, x, Value::Null) => Ok(x.clone()),
        (Op::Add, Value::Number(a), Value::Number(b)) => {
            Ok(num(a.as_f64().unwrap_or(0.0) + b.as_f64().unwrap_or(0.0)))
        }
        (Op::Add, Value::String(a), Value::String(b)) => Ok(Value::String(format!("{a}{b}"))),
        (Op::Add, Value::Array(a), Value::Array(b)) => {
            Ok(Value::Array(a.iter().chain(b).cloned().collect()))
        }
        (Op::Add, Value::Object(a), Value::Object(b)) => {
            let mut m = a.clone();
            for (k, v) in b {
                m.insert(k.clone(), v.clone());
            }
            Ok(Value::Object(m))
        }
        (Op::Add, a, b) => Err(format!("{} and {} cannot be added", show(a), show(b))),

        (Op::Sub, Value::Number(a), Value::Number(b)) => {
            Ok(num(a.as_f64().unwrap_or(0.0) - b.as_f64().unwrap_or(0.0)))
        }
        // Array difference — jq's `-` on two arrays removes every element of the
        // right from the left.
        (Op::Sub, Value::Array(a), Value::Array(b)) => Ok(Value::Array(
            a.iter()
                .filter(|x| !b.iter().any(|y| cmp(x, y) == std::cmp::Ordering::Equal))
                .cloned()
                .collect(),
        )),
        (Op::Sub, a, b) => Err(format!("{} and {} cannot be subtracted", show(a), show(b))),

        (Op::Mul, Value::Number(a), Value::Number(b)) => {
            Ok(num(a.as_f64().unwrap_or(0.0) * b.as_f64().unwrap_or(0.0)))
        }
        // String repetition, either operand order. jq 1.8 gives `""` for a count
        // that truncates to 0 and `null` for a negative one.
        (Op::Mul, Value::String(s), Value::Number(n))
        | (Op::Mul, Value::Number(n), Value::String(s)) => {
            let k = n.as_f64().unwrap_or(0.0);
            if k < 0.0 {
                Ok(Value::Null)
            } else {
                Ok(Value::String(s.repeat(k as usize)))
            }
        }
        // Object `*` is a RECURSIVE merge, unlike `+`'s one-level overwrite.
        (Op::Mul, Value::Object(a), Value::Object(b)) => Ok(Value::Object(deep_merge(a, b))),
        (Op::Mul, a, b) => Err(format!("{} and {} cannot be multiplied", show(a), show(b))),

        (Op::Div, Value::Number(a), Value::Number(b)) => {
            let (x, y) = (a.as_f64().unwrap_or(0.0), b.as_f64().unwrap_or(0.0));
            if y == 0.0 {
                return Err(format!(
                    "{} and {} cannot be divided because the divisor is zero",
                    show(l),
                    show(r)
                ));
            }
            Ok(num(x / y))
        }
        // String `/` splits — `"a,b" / ","` is jq's `split`.
        (Op::Div, Value::String(a), Value::String(b)) => Ok(Value::Array(
            split_str(a, b).into_iter().map(Value::String).collect(),
        )),
        (Op::Div, a, b) => Err(format!("{} and {} cannot be divided", show(a), show(b))),

        // jq's `%` TRUNCATES both operands to integers first (`5.5 % 3` is 2, not
        // the f64 remainder 2.5) and takes C's sign rule, which follows the
        // dividend. A divisor that truncates to zero — including `0.5` — errors.
        (Op::Mod, Value::Number(a), Value::Number(b)) => {
            let x = trunc_i64(a.as_f64().unwrap_or(0.0));
            let y = trunc_i64(b.as_f64().unwrap_or(0.0));
            if y == 0 {
                return Err(format!(
                    "{} and {} cannot be divided (remainder) because the divisor is zero",
                    show(l),
                    show(r)
                ));
            }
            Ok(num(x.wrapping_rem(y) as f64))
        }
        (Op::Mod, a, b) => Err(format!(
            "{} and {} cannot be divided (remainder)",
            show(a),
            show(b)
        )),
        _ => unreachable!("comparison ops handled above"),
    }
}

/// Truncate toward zero into an `i64`, saturating at the ends the way jq's
/// `(intmax_t)` cast does — `1e19 % 3` is `1` in jq because the cast saturates
/// to `i64::MAX` first, and Rust's `as` conversion saturates identically.
fn trunc_i64(v: f64) -> i64 {
    if v.is_nan() {
        0
    } else {
        v.trunc() as i64
    }
}

/// jq's string split: an EMPTY separator splits into characters.
fn split_str(s: &str, sep: &str) -> Vec<String> {
    if sep.is_empty() {
        return s.chars().map(|c| c.to_string()).collect();
    }
    s.split(sep).map(str::to_string).collect()
}

fn deep_merge(a: &Map<String, Value>, b: &Map<String, Value>) -> Map<String, Value> {
    let mut m = a.clone();
    for (k, v) in b {
        match (m.get(k), v) {
            (Some(Value::Object(old)), Value::Object(new)) => {
                m.insert(k.clone(), Value::Object(deep_merge(old, new)));
            }
            _ => {
                m.insert(k.clone(), v.clone());
            }
        }
    }
    m
}

/// jq's TOTAL order over values, which is what makes `<` work across types:
/// `null < false < true < numbers < strings < arrays < objects`. Arrays compare
/// elementwise; objects compare by their sorted key lists first, then by the
/// values in that order.
pub fn cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Bool(false) => 1,
            Value::Bool(true) => 2,
            Value::Number(_) => 3,
            Value::String(_) => 4,
            Value::Array(_) => 5,
            Value::Object(_) => 6,
        }
    }
    let (ra, rb) = (rank(a), rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&y.as_f64().unwrap_or(0.0))
            .unwrap_or(Ordering::Equal),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Array(x), Value::Array(y)) => {
            for (i, j) in x.iter().zip(y) {
                let o = cmp(i, j);
                if o != Ordering::Equal {
                    return o;
                }
            }
            x.len().cmp(&y.len())
        }
        (Value::Object(x), Value::Object(y)) => {
            let (kx, ky): (Vec<&String>, Vec<&String>) = (x.keys().collect(), y.keys().collect());
            let o = kx.cmp(&ky);
            if o != Ordering::Equal {
                return o;
            }
            for k in kx {
                let o = cmp(&x[k], &y[k]);
                if o != Ordering::Equal {
                    return o;
                }
            }
            Ordering::Equal
        }
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }
    fn run(src: &str, input: &str) -> Result<Value, String> {
        eval(&parse(src).unwrap(), &v(input))
    }
    /// Compare through `render`, not through `Value`'s `PartialEq`: a computed
    /// number is an f64 and a parsed one is not, so `Number(2.0) != Number(2)`
    /// as `Value`s while both print `2`.
    fn eq(got: Result<Value, String>, want: &str) {
        assert_eq!(render(&got.expect("expr must not error")), want);
    }

    #[test]
    fn comparison_yields_a_boolean_not_a_number() {
        // The f64 evaluator answered 1/0 here, so `map(. > 1)` rendered
        // `[0,1,1]` where jq renders `[false,true,true]`.
        assert_eq!(run(". > 1", "2").unwrap(), Value::Bool(true));
        assert_eq!(run(". > 1", "1").unwrap(), Value::Bool(false));
        assert_eq!(run(".a == 1", r#"{"a":1}"#).unwrap(), Value::Bool(true));
    }

    #[test]
    fn equality_is_type_strict() {
        assert_eq!(run(".a == 1", r#"{"a":"1"}"#).unwrap(), Value::Bool(false));
        assert_eq!(
            run(r#".a == "1""#, r#"{"a":1}"#).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            run(".a == true", r#"{"a":true}"#).unwrap(),
            Value::Bool(true)
        );
        // An absent key IS null, so comparing against null is how jq spells it.
        assert_eq!(run(".b == null", r#"{"a":1}"#).unwrap(), Value::Bool(true));
    }

    #[test]
    fn truthiness_is_jqs_not_c_s() {
        for (json, want) in [
            ("0", true),
            (r#""""#, true),
            ("[]", true),
            ("{}", true),
            ("null", false),
            ("false", false),
        ] {
            assert_eq!(truthy(&v(json)), want, "truthy({json})");
        }
    }

    #[test]
    fn plus_concatenates_per_type_with_null_as_identity() {
        eq(run(".a + .b", r#"{"a":"x","b":"y"}"#), "xy");
        eq(run(".a + .b", r#"{"a":[1],"b":[2]}"#), "[1,2]");
        eq(
            run(".a + .b", r#"{"a":{"x":1,"b":0},"b":{"b":9}}"#),
            r#"{"b":9,"x":1}"#,
        );
        eq(run(".a + 1", r#"{"a":null}"#), "1");
        eq(run(".a + null", r#"{"a":1}"#), "1");
    }

    #[test]
    fn mod_truncates_both_operands_like_jq() {
        // The whole point: arb's own `%` is the f64 remainder (SPEC §6), jq's is
        // not, and the jq front-end promises jq's answer.
        eq(run(". % 3", "5.5"), "2");
        eq(run(". % 3", "-5.5"), "-2");
        eq(run(". % 2", "0.5"), "0");
        eq(run(". % 4", "100.75"), "0");
        // A divisor that truncates to zero is an error, not an answer.
        assert!(run(". % 0.5", "7").is_err());
    }

    #[test]
    fn arithmetic_against_a_whole_object_refuses() {
        // SPEC §8's named contract defect: these answered `null` with exit 0.
        for op in ["+", "-", "*", "/", "%"] {
            let e = run(&format!(". {op} 3"), r#"{"a":1}"#);
            assert!(e.is_err(), ". {op} 3 must refuse, got {e:?}");
        }
    }

    #[test]
    fn indexing_a_wrong_type_refuses() {
        assert!(run(".a", "3").is_err());
        assert!(run(".a", r#""s""#).is_err());
        assert!(run(".a", "[1,2]").is_err());
        assert!(run(".[0]", r#"{"a":1}"#).is_err());
        // null indexes to null at any depth — that is jq, not a miss.
        assert_eq!(run(".a.b.c", "null").unwrap(), Value::Null);
    }

    #[test]
    fn total_order_spans_types() {
        use std::cmp::Ordering::Less;
        assert_eq!(cmp(&v("null"), &v("false")), Less);
        assert_eq!(cmp(&v("false"), &v("true")), Less);
        assert_eq!(cmp(&v("true"), &v("0")), Less);
        assert_eq!(cmp(&v("0"), &v(r#""a""#)), Less);
        assert_eq!(cmp(&v(r#""a""#), &v("[]")), Less);
        assert_eq!(cmp(&v("[]"), &v("{}")), Less);
        assert_eq!(cmp(&v("[1,2]"), &v("[1,3]")), Less);
    }

    #[test]
    fn out_of_subset_words_and_operators_refuse_to_parse() {
        for src in [
            ".foo // 0",
            "..",
            "reduce",
            "paths",
            "env",
            "not",
            "tostring",
            "$ENV",
            ".a as",
        ] {
            assert!(parse(src).is_err(), "`{src}` must not parse");
        }
    }

    #[test]
    fn keyword_operators_do_not_split_identifiers() {
        // `.android` must stay one key even though it starts with `and`.
        assert_eq!(
            parse(".android").unwrap(),
            Expr::Path(vec![Seg::Key("android".into())])
        );
        eq(run(".ordinal", r#"{"ordinal":7}"#), "7");
    }

    #[test]
    fn string_and_object_operators_follow_jq() {
        eq(run(r#". * 3"#, r#""ab""#), "ababab");
        eq(run(r#". * 0"#, r#""ab""#), "");
        eq(run(r#". * -1"#, r#""ab""#), "null");
        eq(run(r#". / ",""#, r#""a,b""#), r#"["a","b"]"#);
        eq(run(". - [2]", "[1,2,3]"), "[1,3]");
    }

    #[test]
    fn and_or_short_circuit_to_booleans() {
        eq(run(".a and .b", r#"{"a":1,"b":2}"#), "true");
        eq(run(".a or .b", r#"{"a":null,"b":false}"#), "false");
        // Short-circuit: the right operand would raise if it were evaluated.
        eq(run("false and (.a + 3)", r#"{"a":{}}"#), "false");
    }
}
