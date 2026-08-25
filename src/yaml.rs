//! YAML -> the jq value model, composed from the parser's EVENT stream, with
//! every node's metadata kept beside its value.
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
//! of it is here.
//!
//! # Node metadata
//!
//! `yq`'s surface is ~60 operators wide because a YAML node carries more than a
//! value: comments above it, beside it and below it; an anchor name; whether it
//! is an alias; a tag; the style it was written in. [`crate::ynode`] holds that
//! metadata and explains why it rides ALONGSIDE the value rather than replacing
//! it; this module is what fills it in.
//!
//! Two things the parser does not hand over have to be recovered from the source
//! text, and both are recovered from the SPANS it does hand over rather than
//! guessed at:
//!
//! * **Comments.** saphyr's scanner skips them; they are never tokens. Every
//!   scalar's span is known, though, so a `#` that falls inside one is content
//!   (`pw: "a#b"`, a `|` block containing a comment line) and a `#` outside every
//!   scalar span starts a comment. That is exact, not heuristic.
//! * **Anchor NAMES.** The events carry an anchor *id*, not the name `&x`. The
//!   name is read backwards from the node's span start, over the whitespace and
//!   an optional `!tag` that may sit between them — and only for a node the
//!   parser already said is anchored, so there is nothing to false-positive on.
//!
//! # Comment attachment, as measured against `yq v4.53.6`
//!
//! | source shape | where the comment lands |
//! |---|---|
//! | `a: 1 # x` | LINE comment of the value node |
//! | `# x` directly above `a: 1` | HEAD comment of the KEY node `a` |
//! | `# x` directly above `- item` | HEAD comment of the item node |
//! | `# x` then a BLANK line then the next entry | FOOT comment of the PREVIOUS entry |
//! | the first comment block of a document | HEAD comment of the document's root node |
//! | a trailing comment block at EOF | FOOT comment of the last entry at that INDENT |
//!
//! Each row was measured (`yq '.b | key | head_comment'` and friends) rather
//! than taken from yq's documentation, and `scripts/jq_parity.sh`'s
//! `yq_roundtrip_probe` is what checks the whole set byte-for-byte.

use crate::jqlang::JqVal;
use crate::ynode::{NodeMeta, Style};
use saphyr_parser::{Event, Parser, ScalarStyle, Span};
use std::collections::HashMap;
use std::rc::Rc;

/// Parse a YAML stream into one `JqVal` per document, reading from standard
/// input as far as the node metadata is concerned (`filename` answers `-`,
/// which is `yq`'s own answer for a piped document).
pub fn documents(src: &str) -> Vec<JqVal> {
    documents_from(src, "-", 0)
}

/// Parse a YAML stream that came from `file`, the `idx`th input.
///
/// A document that composes to `null` is dropped, which is what makes a trailing
/// `---` or a comment-only tail contribute nothing rather than an empty line.
/// A parse error ENDS the stream: the documents already composed stand, and the
/// malformed remainder is not guessed at.
pub fn documents_from(src: &str, file: &str, idx: u32) -> Vec<JqVal> {
    let mut evs = Vec::new();
    for item in Parser::new_from_str(src).keep_tags(true) {
        match item {
            Ok(pair) => evs.push(pair),
            Err(_) => break,
        }
    }
    let comments = CommentMap::build(src, &evs);
    Composer {
        evs: &evs,
        i: 0,
        src,
        comments,
        anchors: HashMap::new(),
        doc: 0,
        file: Rc::from(file),
        file_index: idx,
        line_base: 0,
    }
    .run()
}

// ─────────────────────────────────────────────────────────────────────────────
// Comments
// ─────────────────────────────────────────────────────────────────────────────

/// One source line's comment, if it has one.
#[derive(Clone, Default)]
struct LineInfo {
    /// The comment text with `#` and one following space removed. `None` when
    /// the line has no comment.
    text: Option<String>,
    /// True when nothing but whitespace precedes the `#` — an "own line"
    /// comment, which is the only kind that can be a head or a foot.
    own: bool,
    /// Column (0-based) of the `#`, which is the indent a foot block is claimed
    /// at.
    col: usize,
    /// True when the line holds nothing but whitespace.
    blank: bool,
    /// Set once a node has taken this comment, so no second node takes it too.
    claimed: bool,
}

/// Every source line's comment, plus the byte ranges that are scalar CONTENT and
/// therefore cannot start one.
struct CommentMap {
    /// Indexed by 1-based line number; slot 0 is a placeholder.
    lines: Vec<LineInfo>,
}

impl CommentMap {
    fn build(src: &str, evs: &[(Event<'_>, Span)]) -> CommentMap {
        // Byte ranges that are scalar content. A `#` inside one is data — a
        // `"pass#word"`, or a comment-looking line inside a `|` block.
        let mut masked: Vec<(usize, usize)> = evs
            .iter()
            .filter(|(e, _)| matches!(e, Event::Scalar(..)))
            .map(|(_, sp)| (sp.start.index(), sp.end.index()))
            .filter(|(a, b)| b > a)
            .collect();
        masked.sort_unstable();

        let mut lines = vec![LineInfo::default()];
        let mut at = 0usize;
        for raw in src.split('\n') {
            let mut info = LineInfo {
                blank: raw.trim().is_empty(),
                ..LineInfo::default()
            };
            if let Some((col, text)) = find_comment(raw, at, &masked) {
                info.own = raw[..col].trim().is_empty();
                info.col = raw[..col].chars().count();
                info.text = Some(text);
            }
            lines.push(info);
            at += raw.len() + 1;
        }
        CommentMap { lines }
    }

    fn get(&self, line: usize) -> Option<&LineInfo> {
        self.lines.get(line)
    }

    /// The head-comment block that sits DIRECTLY above `line`: the maximal run
    /// of unclaimed own-line comments ending at `line - 1`, with no blank line
    /// between the block and `line`.
    fn take_head(&mut self, line: usize) -> String {
        let mut top = line;
        while top > 1 {
            match self.lines.get(top - 1) {
                Some(l) if l.own && l.text.is_some() && !l.claimed => top -= 1,
                _ => break,
            }
        }
        self.take_block(top, line)
    }

    /// The foot-comment block that sits DIRECTLY below `line`, at column `col`.
    ///
    /// A block only counts as a foot when what follows it is a blank line or the
    /// end of the stream — otherwise it belongs to the node below it, as a head.
    /// The column test is what makes a trailing block at the outer indent belong
    /// to the outer entry rather than to the innermost node that happens to end
    /// just above it, which is where `yq` puts it.
    fn take_foot(&mut self, line: usize, col: usize) -> String {
        let mut end = line + 1;
        while let Some(l) = self.lines.get(end) {
            if l.own && l.text.is_some() && !l.claimed && l.col == col {
                end += 1;
            } else {
                break;
            }
        }
        if end == line + 1 {
            return String::new();
        }
        let terminated = match self.lines.get(end) {
            None => true,
            Some(l) => l.blank,
        };
        if !terminated {
            return String::new();
        }
        self.take_block(line + 1, end)
    }

    /// Claim `[from, to)` and return the comment text, newline-joined.
    fn take_block(&mut self, from: usize, to: usize) -> String {
        let mut parts = Vec::new();
        for l in from..to {
            if let Some(info) = self.lines.get_mut(l) {
                if let Some(t) = info.text.clone() {
                    info.claimed = true;
                    parts.push(t);
                }
            }
        }
        parts.join("\n")
    }

    /// The trailing comment on `line`, if any and unclaimed.
    fn take_line(&mut self, line: usize) -> String {
        match self.lines.get_mut(line) {
            Some(info) if !info.own && !info.claimed => match info.text.clone() {
                Some(t) => {
                    info.claimed = true;
                    t
                }
                None => String::new(),
            },
            _ => String::new(),
        }
    }
}

/// The `#` that starts a comment on `raw`, whose first byte is at `base` in the
/// source, given the sorted scalar-content ranges. Returns the byte column of
/// the `#` within `raw` and the comment text.
///
/// A `#` only opens a comment when it is at the start of the line's content or
/// preceded by a space or tab — `a#b` is a plain scalar, not `a` plus a comment,
/// and that is YAML's rule, not a shortcut.
fn find_comment(raw: &str, base: usize, masked: &[(usize, usize)]) -> Option<(usize, String)> {
    let bytes = raw.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b != b'#' {
            continue;
        }
        if i > 0 && !matches!(bytes[i - 1], b' ' | b'\t') {
            continue;
        }
        let abs = base + i;
        if masked.iter().any(|&(s, e)| abs >= s && abs < e) {
            continue;
        }
        let text = raw[i + 1..].strip_prefix(' ').unwrap_or(&raw[i + 1..]);
        return Some((i, text.trim_end_matches('\r').to_string()));
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// Composition
// ─────────────────────────────────────────────────────────────────────────────

struct Composer<'a, 'input> {
    evs: &'a [(Event<'input>, Span)],
    i: usize,
    src: &'a str,
    comments: CommentMap,
    /// Anchor id -> the value it names. YAML scopes anchors to a DOCUMENT, so
    /// this is cleared at each document start; an alias to an anchor from an
    /// earlier document is not defined and resolves to null.
    anchors: HashMap<usize, JqVal>,
    doc: u32,
    file: Rc<str>,
    file_index: u32,
    /// Physical line of the stream's first CONTENT line, minus one. `yq` counts
    /// lines from there rather than from the top of the file, so a document
    /// behind a leading comment block reports its first entry as line 1.
    line_base: u32,
}

impl Composer<'_, '_> {
    fn run(mut self) -> Vec<JqVal> {
        let mut docs = Vec::new();
        // Rebase the line numbering on the first event that has a position.
        if let Some((_, sp)) = self
            .evs
            .iter()
            .find(|(e, _)| !matches!(e, Event::StreamStart))
        {
            self.line_base = sp.start.line().saturating_sub(1) as u32;
        }
        while let Some((ev, _)) = self.evs.get(self.i) {
            match ev {
                Event::StreamEnd => break,
                Event::DocumentStart(_) => {
                    self.i += 1;
                    self.anchors.clear();
                    let v = self.node(&[], None);
                    if !matches!(v.bare(), JqVal::Null) {
                        docs.push(self.finish_doc(v));
                    }
                    self.doc += 1;
                }
                // StreamStart, DocumentEnd and the internal `Nothing` carry no
                // value; anything else at this depth is a stray closing event.
                _ => self.i += 1,
            }
        }
        docs
    }

    /// Give the document's root node the leading comment block, which is where
    /// `yq` puts a file's opening comment (`head_comment` at the root answers
    /// with it, and the first key's does not).
    fn finish_doc(&mut self, v: JqVal) -> JqVal {
        let first = v.meta().map_or(1, |m| m.line_no) as usize + self.line_base as usize;
        let head = self.comments.take_head(first);
        if head.is_empty() {
            return v;
        }
        v.with_meta(|m| m.head = Rc::from(head.as_str()))
    }

    /// The 1-based line `yq` reports for a span, rebased past the leading
    /// comment block.
    fn line_of(&self, sp: &Span) -> u32 {
        (sp.start.line() as u32).saturating_sub(self.line_base).max(1)
    }

    /// Compose the node starting at the cursor, leaving the cursor just past it.
    ///
    /// `path` is where the node sits in its document and `key` is the key node
    /// it is the value of, both recorded so `path`/`key`/`parent` can answer
    /// later. See [`crate::ynode`] for what that recording does NOT promise.
    fn node(&mut self, path: &[JqVal], key: Option<Rc<JqVal>>) -> JqVal {
        let Some((ev, sp)) = self.evs.get(self.i) else {
            return JqVal::Null;
        };
        let (ev, sp) = (ev.clone(), *sp);
        self.i += 1;
        let mut meta = NodeMeta {
            line_no: self.line_of(&sp),
            col_no: sp.start.col() as u32 + 1,
            path: Rc::new(path.to_vec()),
            key,
            doc: self.doc,
            file: self.file.clone(),
            file_index: self.file_index,
            ..NodeMeta::default()
        };
        match ev {
            Event::Scalar(text, style, aid, tag) => {
                let tref = tag.as_ref().map(|t| t.as_ref());
                let v = scalar(&text, style, tref);
                meta.style = scalar_style(style);
                meta.tag = tag_name(tref);
                meta.anchor = Rc::from(self.anchor_name(sp.start.index()).as_str());
                self.anchor(aid, &v);
                // A block scalar spans several lines; its trailing comment, if
                // any, sits on the line the indicator is on, which is the line
                // BEFORE the span starts.
                let line = if style == ScalarStyle::Literal || style == ScalarStyle::Folded {
                    (sp.start.line() as usize).saturating_sub(1)
                } else {
                    sp.start.line() as usize
                };
                meta.line = Rc::from(self.comments.take_line(line).as_str());
                JqVal::wrap(v, meta)
            }
            Event::Alias(aid) => {
                let v = self.anchors.get(&aid).cloned().unwrap_or(JqVal::Null);
                // An alias keeps ITS OWN box, not the anchor's: `use: *anc` must
                // re-emit as `*anc`, and must not inherit the anchored node's
                // `&anc` and re-declare it.
                meta.alias = Rc::from(
                    self.src
                        .get(sp.start.index()..sp.end.index())
                        .unwrap_or("")
                        .trim_start_matches('*'),
                );
                meta.line = Rc::from(self.comments.take_line(sp.start.line() as usize).as_str());
                JqVal::wrap(v.bare().clone(), meta)
            }
            Event::SequenceStart(aid, tag) => {
                let flow = self.src.as_bytes().get(sp.start.index()) == Some(&b'[');
                meta.style = if flow { Style::Flow } else { Style::Block };
                meta.tag = tag_name(tag.as_ref().map(|t| t.as_ref()));
                meta.anchor = Rc::from(self.anchor_name(sp.start.index()).as_str());
                let mut items = Vec::new();
                while !matches!(
                    self.evs.get(self.i).map(|(e, _)| e),
                    Some(Event::SequenceEnd) | None
                ) {
                    let mut p = path.to_vec();
                    p.push(JqVal::num(items.len() as f64));
                    let start = self.evs[self.i].1.start.line() as usize;
                    let item = self.node(&p, None);
                    let head = self.comments.take_head(start);
                    items.push(if head.is_empty() {
                        item
                    } else {
                        item.with_meta(|m| m.head = Rc::from(head.as_str()))
                    });
                }
                self.i += 1; // SequenceEnd
                self.claim_foot(&mut items, sp.start.col());
                let v = JqVal::arr(items);
                self.anchor(aid, &v);
                JqVal::wrap(v, meta)
            }
            Event::MappingStart(aid, tag) => {
                let flow = self.src.as_bytes().get(sp.start.index()) == Some(&b'{');
                meta.style = if flow { Style::Flow } else { Style::Block };
                meta.tag = tag_name(tag.as_ref().map(|t| t.as_ref()));
                meta.anchor = Rc::from(self.anchor_name(sp.start.index()).as_str());
                let v = self.mapping(path, sp.start.col());
                self.anchor(aid, &v);
                JqVal::wrap(v, meta)
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

    /// Hand the trailing comment block at `col` to the LAST entry of the
    /// collection that just closed.
    ///
    /// The column test is the whole point: a block at column 0 under a nested
    /// sequence belongs to the outer mapping's last key, which is where `yq`
    /// puts it, and matching on indent is what lets the inner frame decline it.
    fn claim_foot(&mut self, items: &mut [JqVal], col: usize) {
        let Some(last) = items.last_mut() else { return };
        let end = self.evs[self.i.saturating_sub(1)].1.start.line() as usize;
        let foot = self.comments.take_foot(end.saturating_sub(1), col);
        if !foot.is_empty() {
            *last = last.with_meta(|m| m.foot = Rc::from(foot.as_str()));
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
    fn mapping(&mut self, path: &[JqVal], col: usize) -> JqVal {
        let mut pairs: Vec<(Rc<str>, JqVal)> = Vec::new();
        // (index in `pairs` where the `<<` stood, the merge source).
        let mut merges: Vec<(usize, JqVal)> = Vec::new();
        // The key nodes, parallel to `pairs`, so a foot comment can be attached
        // to the LAST key after the mapping closes — which is where yq puts a
        // trailing block.
        let mut keys: Vec<JqVal> = Vec::new();
        while !matches!(
            self.evs.get(self.i).map(|(e, _)| e),
            Some(Event::MappingEnd) | None
        ) {
            let kline = self.evs[self.i].1.start.line() as usize;
            let k = self.node(&[], None);
            // The head comment above an entry belongs to its KEY node, which is
            // what `.b | key | head_comment` reads.
            let head = self.comments.take_head(kline);
            let k = if head.is_empty() {
                k
            } else {
                k.with_meta(|m| {
                    m.head = Rc::from(head.as_str());
                    m.is_key = true;
                })
            };
            let k = if k.meta().is_some_and(|m| m.is_key) {
                k
            } else {
                k.with_meta(|m| m.is_key = true)
            };
            let key: Rc<str> = match k.bare() {
                JqVal::Str(s) => s.clone(),
                // YAML allows a non-string key (`1: x`); a JSON object requires a
                // string, and its scalar text is what `yq -o=json` emits.
                other => Rc::from(crate::jqlang::render_raw(other).as_str()),
            };
            let mut p = path.to_vec();
            p.push(JqVal::Str(key.clone()));
            let v = self.node(&p, Some(Rc::new(k.clone())));
            if &*key == "<<" {
                merges.push((pairs.len(), v));
                continue;
            }
            // A repeated key keeps its FIRST position and takes the last value,
            // which is how a mapping-shaped model with insertion order behaves.
            match pairs.iter().position(|(k2, _)| *k2 == key) {
                Some(at) => {
                    pairs[at].1 = v;
                    keys[at] = k;
                }
                None => {
                    pairs.push((key, v));
                    keys.push(k);
                }
            }
        }
        self.i += 1; // MappingEnd
        self.claim_foot(&mut keys, col);
        // Re-attach the (possibly foot-annotated) key nodes to their values.
        for (slot, k) in pairs.iter_mut().zip(keys.iter()) {
            slot.1 = slot.1.with_meta(|m| m.key = Some(Rc::new(k.clone())));
        }
        // Each splice shifts every later `<<` position right by what it inserted.
        let mut shift = 0;
        for (at, src) in merges {
            let sources = match src.bare() {
                JqVal::Arr(a) => a.to_vec(),
                other => vec![other.clone()],
            };
            let mut insert: Vec<(Rc<str>, JqVal)> = Vec::new();
            for s in sources {
                if let JqVal::Obj(m) = s.bare() {
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

    /// The anchor NAME written just before the node that starts at `at`.
    ///
    /// The events carry an id, not a name, so the name is read backwards from
    /// the node: over whitespace, over an optional `!tag` (which may be written
    /// on either side of the anchor), and then over the `&name` itself. Called
    /// only for a node the parser already reported as anchored.
    fn anchor_name(&self, at: usize) -> String {
        let b = self.src.as_bytes();
        let mut i = at;
        for _ in 0..2 {
            while i > 0 && matches!(b[i - 1], b' ' | b'\t' | b'\n' | b'\r') {
                i -= 1;
            }
            let end = i;
            while i > 0 && !matches!(b[i - 1], b' ' | b'\t' | b'\n' | b'\r' | b':' | b'-') {
                i -= 1;
            }
            match b.get(i) {
                Some(b'&') => {
                    return String::from_utf8_lossy(&b[i + 1..end]).into_owned();
                }
                // A tag may sit between the anchor and the node; step over it and
                // look once more.
                Some(b'!') => continue,
                _ => return String::new(),
            }
        }
        String::new()
    }
}

/// `yq`'s spelling of a node's explicit tag: `!!str` for the core schema,
/// `!mytag` for a local one. Empty when the node carries none.
fn tag_name(tag: Option<&saphyr_parser::Tag>) -> Rc<str> {
    match tag {
        None => Rc::from(""),
        Some(t) if t.handle == "tag:yaml.org,2002:" => Rc::from(format!("!!{}", t.suffix).as_str()),
        Some(t) if t.handle == "!" => Rc::from(format!("!{}", t.suffix).as_str()),
        Some(t) => Rc::from(format!("{}{}", t.handle, t.suffix).as_str()),
    }
}

fn scalar_style(style: ScalarStyle) -> Style {
    match style {
        ScalarStyle::Plain => Style::Plain,
        ScalarStyle::SingleQuoted => Style::Single,
        ScalarStyle::DoubleQuoted => Style::Double,
        ScalarStyle::Literal => Style::Literal,
        ScalarStyle::Folded => Style::Folded,
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
