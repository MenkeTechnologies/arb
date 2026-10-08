//! `jvparse` — jq's JSON text parser, ported from jq 1.8's `src/jv_parse.c`.
//!
//! [`crate::jqlang::parse_json`] reads arb's input stream with a strict, fast,
//! literal-preserving reader and falls back to this one for what the strict reader
//! refuses, since jq reads its input with `jv_parse.c` too. `fromjson` needs this
//! reader outright, because both what it accepts and how it refuses are
//! observable: `"01"`, `"1."`, `".5"`, `"+1"` and `"Infinity"` parse, `"nan1"` does not, and a refusal is a
//! fixed sentence with a line and column (`Unfinished JSON term at EOF at line
//! 1, column 4`). This is that reader, transcribed from the non-streaming path
//! of `jv_parse.c` (`scan`, `parse_token`, `found_string`, `check_literal`,
//! `jv_parser_next`, `jv_parse_sized_custom_flags`) so the decisions and the
//! messages are jq's by construction rather than by matching a list of cases.

use crate::jqlang::{num_from_literal, JqVal};
use std::rc::Rc;

/// jq's `MAX_PARSING_DEPTH`.
const MAX_PARSING_DEPTH: usize = 10000;

/// One `stack` slot of jq's parser: an open container, or an object key whose
/// value has not arrived yet (jq pushes the key string itself).
enum Frame {
    Arr(Vec<JqVal>),
    Obj(Vec<(Rc<str>, JqVal)>),
    Key(Rc<str>),
}

#[derive(PartialEq)]
enum St {
    Normal,
    String,
    StringEscape,
}

struct Parser {
    stack: Vec<Frame>,
    next: Option<JqVal>,
    token: Vec<u8>,
    st: St,
    line: usize,
    column: usize,
}

/// `scan`'s result: nothing yet, a finished value, or a refusal.
enum Step {
    More,
    Done(JqVal),
}

type P<T> = Result<T, &'static str>;

impl Parser {
    /// `value()`: a second value with no separator is refused.
    fn value(&mut self, v: JqVal) -> P<()> {
        if self.next.is_some() {
            return Err("Expected separator between values");
        }
        self.next = Some(v);
        Ok(())
    }

    /// `parse_token()`: the six structural characters.
    fn token(&mut self, ch: u8) -> P<()> {
        match ch {
            b'[' | b'{' => {
                if self.stack.len() >= MAX_PARSING_DEPTH {
                    return Err("Exceeds depth limit for parsing");
                }
                if self.next.is_some() {
                    return Err("Expected separator between values");
                }
                self.stack.push(if ch == b'[' {
                    Frame::Arr(Vec::new())
                } else {
                    Frame::Obj(Vec::new())
                });
            }
            b':' => {
                let Some(next) = self.next.take() else {
                    return Err("Expected string key before ':'");
                };
                if !matches!(self.stack.last(), Some(Frame::Obj(_))) {
                    return Err("':' not as part of an object");
                }
                let JqVal::Str(k) = next else {
                    return Err("Object keys must be strings");
                };
                self.stack.push(Frame::Key(k));
            }
            b',' => {
                let Some(next) = self.next.take() else {
                    return Err("Expected value before ','");
                };
                match self.stack.last_mut() {
                    None => return Err("',' not as part of an object or array"),
                    Some(Frame::Arr(a)) => a.push(next),
                    Some(Frame::Key(_)) => self.close_pair(next),
                    // `{"a", "b"}`
                    Some(Frame::Obj(_)) => return Err("Objects must consist of key:value pairs"),
                }
            }
            b']' => {
                let Some(Frame::Arr(a)) = self.stack.last_mut() else {
                    return Err("Unmatched ']'");
                };
                match self.next.take() {
                    Some(v) => a.push(v),
                    // `[1,2,3,]`
                    None if !a.is_empty() => return Err("Expected another array element"),
                    None => {}
                }
                let Some(Frame::Arr(a)) = self.stack.pop() else {
                    unreachable!("checked above")
                };
                self.next = Some(JqVal::arr(a));
            }
            b'}' => {
                if self.stack.is_empty() {
                    return Err("Unmatched '}'");
                }
                match self.next.take() {
                    Some(v) => {
                        if !matches!(self.stack.last(), Some(Frame::Key(_))) {
                            return Err("Objects must consist of key:value pairs");
                        }
                        self.close_pair(v);
                    }
                    None => match self.stack.last() {
                        Some(Frame::Obj(m)) if !m.is_empty() => {
                            return Err("Expected another key-value pair")
                        }
                        Some(Frame::Obj(_)) => {}
                        _ => return Err("Unmatched '}'"),
                    },
                }
                let Some(Frame::Obj(m)) = self.stack.pop() else {
                    unreachable!("a key frame always sits on an object frame")
                };
                self.next = Some(JqVal::obj(m));
            }
            _ => unreachable!("token() is only called for STRUCTURE characters"),
        }
        Ok(())
    }

    /// Set the pending key on the object beneath it (`jv_object_set`: a repeated
    /// key keeps its first position and takes the last value).
    fn close_pair(&mut self, v: JqVal) {
        let Some(Frame::Key(k)) = self.stack.pop() else {
            unreachable!("caller checked for a key frame")
        };
        let Some(Frame::Obj(m)) = self.stack.last_mut() else {
            unreachable!("a key frame always sits on an object frame")
        };
        match m.iter_mut().find(|(ek, _)| *ek == k) {
            Some(slot) => slot.1 = v,
            None => m.push((k, v)),
        }
    }

    /// `found_string()`: decode the escapes in the buffered string body.
    fn found_string(&mut self) -> P<()> {
        let t = std::mem::take(&mut self.token);
        let mut out = String::with_capacity(t.len());
        let mut raw: Vec<u8> = Vec::new();
        let mut i = 0;
        let flush = |raw: &mut Vec<u8>, out: &mut String| {
            out.push_str(&String::from_utf8_lossy(raw));
            raw.clear();
        };
        while i < t.len() {
            let c = t[i];
            i += 1;
            if c != b'\\' {
                if c < 0x20 {
                    return Err("Invalid string: control characters from U+0000 through U+001F must be escaped");
                }
                raw.push(c);
                continue;
            }
            let Some(&e) = t.get(i) else {
                return Err("Expected escape character at end of string");
            };
            i += 1;
            flush(&mut raw, &mut out);
            match e {
                b'\\' | b'"' | b'/' => out.push(e as char),
                b'b' => out.push('\u{8}'),
                b'f' => out.push('\u{c}'),
                b't' => out.push('\t'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b'u' => {
                    let Some(hex) = t.get(i..i + 4) else {
                        return Err("Invalid \\uXXXX escape");
                    };
                    let mut cp = unhex4(hex).ok_or("Invalid characters in \\uXXXX escape")?;
                    i += 4;
                    if (0xD800..=0xDBFF).contains(&cp) {
                        if t.len() < i + 6 || t[i] != b'\\' || t[i + 1] != b'u' {
                            return Err("Invalid \\uXXXX\\uXXXX surrogate pair escape");
                        }
                        let lo = unhex4(&t[i + 2..i + 6]).unwrap_or(0);
                        if !(0xDC00..=0xDFFF).contains(&lo) {
                            return Err("Invalid \\uXXXX\\uXXXX surrogate pair escape");
                        }
                        i += 6;
                        cp = 0x10000 + (((cp - 0xD800) << 10) | (lo - 0xDC00));
                    }
                    // A lone low surrogate encodes to bytes `jv_string_sized`
                    // repairs to U+FFFD, as does anything past U+10FFFF.
                    out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                }
                _ => return Err("Invalid escape"),
            }
        }
        flush(&mut raw, &mut out);
        self.value(JqVal::str(out))
    }

    /// `check_literal()`: the buffered bare token is `true`/`false`/`null` or a
    /// number literal.
    fn check_literal(&mut self) -> P<()> {
        if self.token.is_empty() {
            return Ok(());
        }
        let t = std::mem::take(&mut self.token);
        let pattern: Option<(&[u8], JqVal)> = match t[0] {
            b't' => Some((b"true", JqVal::Bool(true))),
            b'f' => Some((b"false", JqVal::Bool(false))),
            b'\'' => return Err("Invalid string literal; expected \", but got '"),
            // `n` followed by `u` is `null`; anything else (`nan`) is a number.
            b'n' if t.get(1) == Some(&b'u') => Some((b"null", JqVal::Null)),
            _ => None,
        };
        let v = match pattern {
            Some((p, v)) if t == p => v,
            Some(_) => return Err("Invalid literal"),
            None => number_literal(&t).ok_or("Invalid numeric literal")?,
        };
        self.value(v)
    }

    /// `parse_check_done()`: a complete top-level value is ready.
    fn check_done(&mut self) -> Option<JqVal> {
        if self.stack.is_empty() {
            self.next.take()
        } else {
            None
        }
    }

    /// `scan()`: feed one byte.
    fn scan(&mut self, ch: u8) -> P<Step> {
        self.column += 1;
        if ch == b'\n' {
            self.line += 1;
            self.column = 0;
        }
        let mut answer = Step::More;
        if self.st == St::Normal {
            let structure = matches!(ch, b'[' | b',' | b']' | b'{' | b':' | b'}');
            let ws = matches!(ch, b' ' | b'\t' | b'\r' | b'\n');
            if structure || ws || ch == b'"' {
                self.check_literal()?;
                if let Some(v) = self.check_done() {
                    answer = Step::Done(v);
                }
            }
            if ch == b'"' {
                self.st = St::String;
            } else if structure {
                self.token(ch)?;
            } else if !ws {
                self.token.push(ch);
            }
            if let Some(v) = self.check_done() {
                answer = Step::Done(v);
            }
        } else if ch == b'"' && self.st == St::String {
            self.found_string()?;
            self.st = St::Normal;
            if let Some(v) = self.check_done() {
                answer = Step::Done(v);
            }
        } else {
            self.token.push(ch);
            self.st = if ch == b'\\' && self.st == St::String {
                St::StringEscape
            } else {
                St::String
            };
        }
        Ok(answer)
    }
}

fn unhex4(h: &[u8]) -> Option<u32> {
    let s = std::str::from_utf8(h).ok()?;
    if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(s, 16).ok()
}

/// A bare token as jq 1.8's decNumber literal reader takes it: an optional
/// sign, then digits with at most one `.` (at least one digit) and an optional
/// exponent with at least one digit — or, in any case, `nan`, `snan`, `inf` or
/// `infinity`. Measured against jq 1.8.2: `01`, `1.`, `.5`, `+1` and `-sNaN`
/// parse; `nan1`, `1e` and `0x1` do not.
fn number_literal(t: &[u8]) -> Option<JqVal> {
    let text = std::str::from_utf8(t).ok()?;
    let (neg, body) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    match body.to_ascii_lowercase().as_str() {
        "nan" | "snan" => return Some(JqVal::num(f64::NAN)),
        "inf" | "infinity" => {
            return Some(JqVal::num(if neg {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }))
        }
        _ => {}
    }
    let b = body.as_bytes();
    let mut i = 0;
    let mut digits = 0;
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
        digits += 1;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    // Rust's float parser refuses a bare `1.` and `.5` is fine; normalize the
    // one shape it rejects so the double is right.
    let for_f64 = text.strip_suffix('.').unwrap_or(text);
    let for_f64 = for_f64.replace(".e", "e").replace(".E", "E");
    let n: f64 = for_f64.parse().ok()?;
    Some(num_from_literal(n, &for_f64))
}

/// `jv_parser_next()` over a whole buffer: the next value, `Ok(None)` at a clean
/// end, or jq's message for the first refusal.
fn next_value(p: &mut Parser, b: &[u8], pos: &mut usize) -> Result<Option<JqVal>, String> {
    while *pos < b.len() {
        let ch = b[*pos];
        *pos += 1;
        match p.scan(ch) {
            Ok(Step::More) => {}
            Ok(Step::Done(v)) => return Ok(Some(v)),
            Err(msg) => {
                return Err(format!("{msg} at line {}, column {}", p.line, p.column));
            }
        }
    }
    // At EOF.
    if p.st != St::Normal {
        return Err(format!(
            "Unfinished string at EOF at line {}, column {}",
            p.line, p.column
        ));
    }
    if let Err(msg) = p.check_literal() {
        return Err(format!(
            "{msg} at EOF at line {}, column {}",
            p.line, p.column
        ));
    }
    if !p.stack.is_empty() {
        return Err(format!(
            "Unfinished JSON term at EOF at line {}, column {}",
            p.line, p.column
        ));
    }
    Ok(p.next.take())
}

/// `jv_parse_sized()`: exactly one JSON value, by jq's rules, or jq's message
/// (without the ` (while parsing '…')` suffix the caller adds).
pub fn parse(src: &str) -> Result<JqVal, String> {
    let mut b = src.as_bytes();
    // `jv_parser_set_buf` strips a UTF-8 byte-order mark; a partial one is an
    // error of its own.
    const BOM: &[u8] = b"\xEF\xBB\xBF";
    let bom = b.iter().zip(BOM).take_while(|(x, y)| x == y).count();
    if bom == BOM.len() {
        b = &b[3..];
    } else if bom > 0 {
        return Err("Malformed BOM".into());
    }
    let mut p = Parser {
        stack: Vec::new(),
        next: None,
        token: Vec::new(),
        st: St::Normal,
        line: 1,
        column: 0,
    };
    let mut pos = 0;
    match next_value(&mut p, b, &mut pos)? {
        Some(v) => match next_value(&mut p, b, &mut pos)? {
            Some(_) => Err("Unexpected extra JSON values".into()),
            None => Ok(v),
        },
        None => Err("Expected JSON value".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::jqlang::render;

    fn ok(s: &str) -> String {
        render(&parse(s).unwrap_or_else(|e| panic!("{s}: {e}")))
    }

    #[test]
    fn accepts_what_jq_accepts() {
        assert_eq!(
            ok(r#"{"b":1.50,"a":[1E5,"xé"]}"#),
            r#"{"b":1.50,"a":[1E+5,"xé"]}"#
        );
        assert_eq!(ok("01"), "1");
        assert_eq!(ok("1."), "1");
        assert_eq!(ok(".5"), "0.5");
        assert_eq!(ok("+1"), "1");
        assert_eq!(ok("1.e5"), "1E+5");
        assert_eq!(ok("-sNaN"), "null");
        assert_eq!(ok(r#"{"a":1,"a":2}"#), r#"{"a":2}"#);
    }

    #[test]
    fn refuses_with_jq_wording() {
        let err = |s: &str| parse(s).unwrap_err();
        assert_eq!(
            err("[1,2"),
            "Unfinished JSON term at EOF at line 1, column 4"
        );
        assert_eq!(err("1 2"), "Unexpected extra JSON values");
        assert_eq!(
            err("nan1"),
            "Invalid numeric literal at EOF at line 1, column 4"
        );
        assert_eq!(
            err("[1,]"),
            "Expected another array element at line 1, column 4"
        );
        assert_eq!(err("tru e"), "Invalid literal at line 1, column 4");
        assert_eq!(err(" "), "Expected JSON value");
        assert_eq!(err("{\"a\":[}"), "Unmatched '}' at line 1, column 7");
    }
}
