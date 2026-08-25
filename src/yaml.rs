//! YAML -> the jq value model, composed from the parser's EVENT stream.
//!
//! **Why this is not `serde_yaml`.** serde's data model has no place to put a
//! number's source text: a plain scalar reaches the `Visitor` as an `f64` with
//! the text already discarded, and there is no escape hatch (serde_yaml has the
//! scalar's `repr` internally and surfaces it only for strings). So `ratio: 1.50`
//! could only ever print `1.5`, where `yq -o=json` prints `1.50` — while arb's
//! JSON reader kept the literal and printed `1.50`. Same engine, same value, two
//! answers depending on which file it was read from. That asymmetry was the bug.
//!
//! saphyr-parser hands over each scalar as its RAW TEXT, so this composes the
//! document from the event stream and builds numbers through the same
//! `num_from_literal` the JSON reader uses. One number representation
//! (`JqVal::Num(f64, Option<literal>)`), one renderer, both formats.
//!
//! Composing from events also means owning what serde_yaml did for free —
//! anchors, aliases, `<<` merge keys, multi-document streams, `!!tag`s — so all
//! of it is here, and `scripts/jq_parity.sh`'s yq leg is what checks it.

use crate::jqlang::JqVal;
use saphyr_parser::{Event, Parser, ScalarStyle};
use std::collections::HashMap;
use std::rc::Rc;

/// Parse a YAML stream into one `JqVal` per document.
///
/// A document that composes to `null` is dropped, which is what makes a trailing
/// `---` or a comment-only tail contribute nothing rather than an empty line.
/// A parse error ENDS the stream: the documents already composed stand, and the
/// malformed remainder is not guessed at.
pub fn documents(src: &str) -> Vec<JqVal> {
    let mut evs = Vec::new();
    for item in Parser::new_from_str(src).keep_tags(true) {
        match item {
            Ok((ev, _span)) => evs.push(ev),
            Err(_) => break,
        }
    }
    Composer {
        evs: &evs,
        i: 0,
        anchors: HashMap::new(),
    }
    .run()
}

struct Composer<'a, 'input> {
    evs: &'a [Event<'input>],
    i: usize,
    /// Anchor id -> the value it names. YAML scopes anchors to a DOCUMENT, so
    /// this is cleared at each document start; an alias to an anchor from an
    /// earlier document is not defined and resolves to null.
    anchors: HashMap<usize, JqVal>,
}

impl Composer<'_, '_> {
    fn run(mut self) -> Vec<JqVal> {
        let mut docs = Vec::new();
        while let Some(ev) = self.evs.get(self.i) {
            match ev {
                Event::StreamEnd => break,
                Event::DocumentStart(_) => {
                    self.i += 1;
                    self.anchors.clear();
                    let v = self.node();
                    if !matches!(v, JqVal::Null) {
                        docs.push(v);
                    }
                }
                // StreamStart, DocumentEnd and the internal `Nothing` carry no
                // value; anything else at this depth is a stray closing event.
                _ => self.i += 1,
            }
        }
        docs
    }

    /// Compose the node starting at the cursor, leaving the cursor just past it.
    fn node(&mut self) -> JqVal {
        let Some(ev) = self.evs.get(self.i) else {
            return JqVal::Null;
        };
        self.i += 1;
        match ev {
            Event::Scalar(text, style, aid, tag) => {
                let v = scalar(text, *style, tag.as_ref().map(|t| t.as_ref()));
                self.anchor(*aid, &v);
                v
            }
            Event::Alias(aid) => self.anchors.get(aid).cloned().unwrap_or(JqVal::Null),
            Event::SequenceStart(aid, _) => {
                let mut items = Vec::new();
                while !matches!(self.evs.get(self.i), Some(Event::SequenceEnd) | None) {
                    items.push(self.node());
                }
                self.i += 1; // SequenceEnd
                let v = JqVal::arr(items);
                self.anchor(*aid, &v);
                v
            }
            Event::MappingStart(aid, _) => {
                let v = self.mapping();
                self.anchor(*aid, &v);
                v
            }
            // A DocumentEnd where a node was expected is an EMPTY document. The
            // cursor has already stepped past it, so back up: `run` needs to see
            // it to stay in step with the stream.
            Event::DocumentEnd => {
                self.i -= 1;
                JqVal::Null
            }
            _ => JqVal::Null,
        }
    }

    /// Compose a mapping, resolving `<<` merge keys as it goes.
    ///
    /// Two rules, both measured against `yq`:
    ///
    /// * **Precedence.** An EXPLICIT key wins over any merged one, and among
    ///   several sources (`<<: [*a, *b]`) an earlier source wins. That is YAML's
    ///   spec rule; it is also `yq --yaml-fix-merge-anchor-to-spec`, which yq
    ///   warns will become its default. It cannot fall out of the loop order,
    ///   because an explicit key may follow the `<<`, so merges are held until
    ///   every explicit key of this mapping is in.
    /// * **Position.** A merged key lands where the `<<` WAS, not at the end.
    ///   `{<<: *a, extra: 1}` is `{"k":"v","extra":1}` and `{extra: 1, <<: *a}`
    ///   is `{"extra":1,"k":"v"}` — appending put `extra` first in both, so the
    ///   identity filter `.` diverged on any document using a merge key.
    fn mapping(&mut self) -> JqVal {
        let mut pairs: Vec<(Rc<str>, JqVal)> = Vec::new();
        // (index in `pairs` where the `<<` stood, the merge source).
        let mut merges: Vec<(usize, JqVal)> = Vec::new();
        while !matches!(self.evs.get(self.i), Some(Event::MappingEnd) | None) {
            let k = self.node();
            let v = self.node();
            let key: Rc<str> = match &k {
                JqVal::Str(s) => s.clone(),
                // YAML allows a non-string key (`1: x`); a JSON object requires a
                // string, and its scalar text is what `yq -o=json` emits.
                other => Rc::from(crate::jqlang::render_raw(other).as_str()),
            };
            if &*key == "<<" {
                merges.push((pairs.len(), v));
                continue;
            }
            // A repeated key keeps its FIRST position and takes the last value,
            // which is how a mapping-shaped model with insertion order behaves.
            match pairs.iter_mut().find(|(k2, _)| *k2 == key) {
                Some(slot) => slot.1 = v,
                None => pairs.push((key, v)),
            }
        }
        self.i += 1; // MappingEnd
                     // Each splice shifts every later `<<` position right by what it inserted.
        let mut shift = 0;
        for (at, src) in merges {
            let sources = match src {
                JqVal::Arr(a) => a.to_vec(),
                other => vec![other],
            };
            let mut insert: Vec<(Rc<str>, JqVal)> = Vec::new();
            for s in sources {
                if let JqVal::Obj(m) = s {
                    for (k, v) in m.iter() {
                        let taken = pairs.iter().chain(insert.iter()).any(|(k2, _)| k2 == k);
                        if !taken {
                            insert.push((k.clone(), v.clone()));
                        }
                    }
                }
            }
            let n = insert.len();
            pairs.splice(at + shift..at + shift, insert);
            shift += n;
        }
        JqVal::obj(pairs)
    }

    /// Record `v` under anchor id `aid`. Id 0 means the node carried no anchor.
    fn anchor(&mut self, aid: usize, v: &JqVal) {
        if aid > 0 {
            self.anchors.insert(aid, v.clone());
        }
    }
}

/// Resolve one scalar to a value.
///
/// A NON-plain scalar (quoted, `|` literal, `>` folded) is a string by
/// construction — that is what quoting a YAML scalar means — so only a plain one
/// is type-resolved. An explicit `!!tag` overrides both.
fn scalar(text: &str, style: ScalarStyle, tag: Option<&saphyr_parser::Tag>) -> JqVal {
    if let Some(t) = tag {
        if t.handle == "tag:yaml.org,2002:" {
            match t.suffix.as_ref() {
                "str" => return JqVal::str(text),
                "null" => return JqVal::Null,
                "bool" => return parse_bool(text).map_or(JqVal::Null, JqVal::Bool),
                "int" | "float" => {
                    return parse_int(text)
                        .or_else(|| parse_float(text))
                        .unwrap_or(JqVal::Null)
                }
                // Any other `!!` tag has no JSON form; its PAYLOAD is the value,
                // resolved as if it were untagged.
                _ => {}
            }
        }
    }
    if style != ScalarStyle::Plain {
        return JqVal::str(text);
    }
    plain(text)
}

/// Resolve a PLAIN scalar by its text, in YAML's own precedence: null, then
/// bool, then int, then float, then string.
fn plain(text: &str) -> JqVal {
    if text.is_empty() || matches!(text, "null" | "Null" | "NULL" | "~") {
        return JqVal::Null;
    }
    if let Some(b) = parse_bool(text) {
        return JqVal::Bool(b);
    }
    parse_int(text)
        .or_else(|| parse_float(text))
        .unwrap_or_else(|| JqVal::str(text))
}

fn parse_bool(text: &str) -> Option<bool> {
    match text {
        "true" | "True" | "TRUE" => Some(true),
        "false" | "False" | "FALSE" => Some(false),
        _ => None,
    }
}

/// An integer scalar. Decimal, `0x` hex, `0o` octal, an optional sign, and YAML
/// 1.1's `_` digit separators, which `yq` accepts (`1_000` is `1000` there).
///
/// The VALUE is what an integer renders as, never the source text: `yq -o=json`
/// prints `007` as `7` and `0xFF` as `255`. That is the opposite of the float
/// rule below, and it is measured, not assumed — both spellings are probed in
/// `scripts/jq_parity.sh`.
fn parse_int(text: &str) -> Option<JqVal> {
    let (neg, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if body.starts_with(['+', '-']) {
        return None;
    }
    let (radix, digits) = match body {
        b if b.len() > 2 && (b.starts_with("0x") || b.starts_with("0X")) => (16, &b[2..]),
        b if b.len() > 2 && (b.starts_with("0o") || b.starts_with("0O")) => (8, &b[2..]),
        b => (10, b),
    };
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    if clean.is_empty() || digits.starts_with('_') || !clean.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    // Past i64 an integer is read as a double, the way the JSON reader does it,
    // so a 20-digit literal keeps its text and round-trips instead of failing.
    // `yq` refuses that input outright ("value out of range"); SPEC §8 states the
    // tolerance, which is the same one arb already takes against jq above 2^53.
    let n = match i64::from_str_radix(&clean, radix) {
        Ok(v) => v as f64,
        Err(_) if radix == 10 => clean.parse::<f64>().ok()?,
        Err(_) => u64::from_str_radix(&clean, radix).ok()? as f64,
    };
    let n = if neg { -n } else { n };
    // Only a plain decimal integer can carry a meaningful literal — `0xFF`
    // renders as `255`, so its source text must NOT be kept.
    if radix == 10 && clean.len() == digits.len() {
        // A leading `+` is not part of the literal (`yq` prints `+7` as `7`); a
        // leading `-` is.
        return Some(crate::jqlang::num_from_literal(
            n,
            text.trim_start_matches('+'),
        ));
    }
    Some(JqVal::num(n))
}

/// A float scalar, keeping its SOURCE LITERAL — this is the whole point of the
/// module. `ratio: 1.50` holds the value `1.5` and the literal `1.50`, exactly
/// as the JSON reader holds a JSON `1.50`, so both print `1.50`.
///
/// `.inf`/`.nan` have no JSON spelling; they take arb's jq value model, where an
/// infinity clamps to ±DBL_MAX and a NaN renders as `null` (`fmt_num`, measured
/// against jq 1.8.2). `yq` refuses them instead.
fn parse_float(text: &str) -> Option<JqVal> {
    let body = text.strip_prefix('+').unwrap_or(text);
    if body.starts_with(['+', '-']) && !body.starts_with('-') {
        return None;
    }
    match body {
        ".inf" | ".Inf" | ".INF" => return Some(JqVal::num(f64::INFINITY)),
        "-.inf" | "-.Inf" | "-.INF" => return Some(JqVal::num(f64::NEG_INFINITY)),
        ".nan" | ".NaN" | ".NAN" => return Some(JqVal::num(f64::NAN)),
        _ => {}
    }
    // `_` separators are legal in a YAML 1.1 float too, and stripping them means
    // the text is no longer the literal, so those fall back to the value.
    let clean: String = body.chars().filter(|c| *c != '_').collect();
    // Rust's parser accepts `inf`/`nan`/`infinity` as spellings YAML does not.
    if clean
        .chars()
        .any(|c| !matches!(c, '0'..='9' | '.' | 'e' | 'E' | '+' | '-'))
    {
        return None;
    }
    let n = clean.parse::<f64>().ok()?;
    if clean.len() != body.len() {
        return Some(JqVal::num(n));
    }
    // A trailing `.` is a legal YAML float with an empty fraction, and it is the
    // one spelling whose literal is not writable as-is: neither JSON nor
    // decNumber has a bare `5.`, so the literal would be dropped and the value
    // print as `5`. Its zero is explicit instead, which is `yq`'s answer too
    // (`5.` -> `5.0`).
    let lit = if body.ends_with('.') {
        format!("{body}0")
    } else {
        body.to_string()
    };
    Some(crate::jqlang::num_from_literal(n, &lit))
}
