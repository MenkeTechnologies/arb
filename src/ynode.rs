//! The YAML node model — the metadata a YAML node carries that a JSON value has
//! no place for, plus the writer that puts it back on the page.
//!
//! # Why a node model at all
//!
//! `yq`'s defining behaviour is that `yq '.' file.yaml` gives the file back
//! essentially unchanged: comments in every position, anchor names, aliases,
//! merge keys, quoting style and block scalars all survive the round trip. That
//! is not a serializer nicety — it is the reason ~60 of yq's operators exist.
//! `anchor`, `alias`, `tag`, `style`, `head_comment`/`line_comment`/`foot_comment`
//! and their assignment forms all read and write metadata that is attached to a
//! NODE, not to a value. A jq value has nowhere to put any of it.
//!
//! # Why the metadata rides ALONGSIDE the value rather than replacing it
//!
//! Three shapes were weighed:
//!
//! | shape | why not / why |
//! |---|---|
//! | a side table keyed by `Rc` pointer identity | `JqVal::Bool`/`Num`/`Null` are by-value and have no pointer, so `enabled: true # c` could not carry its comment. Fails on the commonest YAML there is. |
//! | a side table keyed by PATH | jq's model flows VALUES, not paths — `map(…)`, `add`, `to_entries` all destroy the correspondence, and the table would silently answer for the wrong node. |
//! | replace `JqVal` with a node type everywhere | rewrites the engine that currently answers jq byte-for-byte. The jq leg is at zero divergences; that is the thing not to put at risk. |
//! | **a `JqVal::Node` box, chosen** | metadata rides beside the value exactly as a number's SOURCE LITERAL already does (`JqVal::Num(f64, Option<Rc<str>>)` — same problem, same answer, already in the tree). The compiler enumerates every site that has to decide what to do with the new variant, and **JSON input never constructs one**, so the jq leg is untouched by construction rather than by inspection. |
//!
//! The last row is the load-bearing one. `crate::jqlang::parse_json` cannot emit
//! a `Node`; only `crate::yaml` can. A JSON program therefore runs over exactly
//! the values it ran over before, through exactly the same arms.
//!
//! # What is deliberately NOT modelled
//!
//! A node's `path`/`key`/`parent` are recorded as they were at READ time. yq
//! carries real parent pointers, so its `parent` survives a value being moved;
//! arb's does not, and a node that has been relocated by `map`/`pick`/`+`
//! reports where it was read from. That is stated rather than hidden — the
//! alternative is a reference cycle through `Rc` for the whole document.

use crate::jqlang::JqVal;
use std::rc::Rc;

/// How a scalar (or collection) was written in the source.
///
/// The names are `yq`'s own — `yq '.x | style'` answers with exactly these
/// strings, and `style = "…"` accepts them. A plain scalar's style is the empty
/// string, which is why `Plain` renders as `""` rather than `"plain"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Style {
    /// `key: value` — no quotes, no block indicator.
    #[default]
    Plain,
    /// `key: 'value'`
    Single,
    /// `key: "value"`
    Double,
    /// `key: |` — a literal block scalar, newlines kept.
    Literal,
    /// `key: >` — a folded block scalar, newlines folded to spaces.
    Folded,
    /// `key: {a: 1}` / `key: [1, 2]` — a collection written inline.
    Flow,
    /// A collection written across lines. Distinct from `Plain` only so a
    /// `style = "flow"` assignment on a block collection has something to
    /// switch back to.
    Block,
}

impl Style {
    /// The string `yq`'s `style` operator answers with.
    pub fn name(self) -> &'static str {
        match self {
            Style::Plain | Style::Block => "",
            Style::Single => "single",
            Style::Double => "double",
            Style::Literal => "literal",
            Style::Folded => "folded",
            Style::Flow => "flow",
        }
    }

    /// Parse a `style = "…"` assignment. `yq` accepts the names it prints plus
    /// `tagged`/`""`; an unknown name resets to plain, which is yq's behaviour
    /// rather than an error.
    pub fn parse(s: &str) -> Style {
        match s {
            "single" | "singleQuoted" | "single_quoted" => Style::Single,
            "double" | "doubleQuoted" | "double_quoted" => Style::Double,
            "literal" => Style::Literal,
            "folded" => Style::Folded,
            "flow" => Style::Flow,
            _ => Style::Plain,
        }
    }

    /// Is this a block scalar indicator (`|` / `>`)?
    pub fn is_block_scalar(self) -> bool {
        matches!(self, Style::Literal | Style::Folded)
    }
}

/// Everything a YAML node knows about itself beyond its value.
///
/// Cheap to clone: every owned field is an `Rc`, and a node that carries no
/// metadata at all is [`NodeMeta::is_bare`] and gets dropped on the way out so
/// an untouched document does not pay for boxes it does not need.
#[derive(Debug, Clone, Default)]
pub struct NodeMeta {
    /// The comment block ABOVE the node, `#` and one leading space stripped,
    /// lines joined with `\n`. Empty when there is none.
    pub head: Rc<str>,
    /// The comment after the node on its own line.
    pub line: Rc<str>,
    /// The comment block BELOW the node, at the node's own indent.
    pub foot: Rc<str>,
    /// The anchor NAME (`&x` -> `x`), empty when the node is not anchored.
    pub anchor: Rc<str>,
    /// The alias NAME (`*x` -> `x`), empty when the node is not an alias.
    /// A node with a non-empty `alias` was written as `*x` and is re-emitted
    /// that way, which is what keeps `use: *anc` from expanding on round trip.
    pub alias: Rc<str>,
    /// The node's tag, in yq's spelling: `!!str`, `!!int`, `!!map`, `!mytag`.
    /// Empty means "resolve from the value", which is what `tag` answers with
    /// for an untagged node.
    pub tag: Rc<str>,
    /// How the node was written.
    pub style: Style,
    /// 1-based line, counted from the stream's first CONTENT line — a leading
    /// comment block is not counted, which is what `yq` reports.
    pub line_no: u32,
    /// 1-based column of the node's first character.
    pub col_no: u32,
    /// The path from the document root to this node, as it was at read time.
    pub path: Rc<Vec<JqVal>>,
    /// The KEY node this node is the value of, with its own metadata — `yq`'s
    /// `key` operator answers with a node, so `.a | key | line_comment` reads
    /// the comment on the key rather than on the value.
    pub key: Option<Rc<JqVal>>,
    /// True on the node `key` returned, which is what `is_key` reports.
    pub is_key: bool,
    /// 0-based index of the document this node came from, within the stream.
    pub doc: u32,
    /// The file the document was read from; `-` for standard input, which is
    /// `yq`'s own answer for a piped document.
    pub file: Rc<str>,
    /// 0-based index of that file among the inputs.
    pub file_index: u32,
    /// The scalar's SOURCE text, kept only when rendering the VALUE would not
    /// reproduce it (`0x10` holds 16, `007` holds 7). Empty otherwise.
    pub raw: Rc<str>,
    /// The node was written with NO text at all — the `empty:` in `empty:\nnext: 1`.
    /// Its value is null, and so is a node written `null` or `~`; only this says
    /// which of the three spellings the file used.
    pub blank: bool,
    /// A mapping's entries as they were WRITTEN, with `<<` still in place.
    /// Readers see the merged mapping; the writer emits this, so a round trip
    /// does not expand a merge key into the keys it stood for.
    pub written: Option<Entries>,
}

impl NodeMeta {
    /// Is every field still at its default — nothing to record?
    ///
    /// Such metadata is dropped rather than boxed, which is what makes
    /// `. anchor = ""` on a JSON value give the value straight back instead of a
    /// box holding nothing.
    ///
    /// POSITION counts. `line_no`, `path` and `key` are as real as a comment is:
    /// `yq` answers `.a | line` and `.a | key` for a document with no comments in
    /// it at all, and a node that dropped them would answer wrongly rather than
    /// cheaply. So a node the YAML reader produced is always boxed — one `Rc`
    /// per node, which a configuration file affords and a `.` over it does not
    /// notice.
    pub fn is_bare(&self) -> bool {
        self.head.is_empty()
            && self.line.is_empty()
            && self.foot.is_empty()
            && self.anchor.is_empty()
            && self.alias.is_empty()
            && self.tag.is_empty()
            && self.raw.is_empty()
            && self.written.is_none()
            && !self.blank
            && self.line_no == 0
            && self.col_no == 0
            && self.path.is_empty()
            && self.key.is_none()
            && !self.is_key
            && self.file.is_empty()
            && matches!(self.style, Style::Plain | Style::Block)
    }
}

/// A mapping's entry list. Named because it appears in both `NodeMeta` and the
/// composer's return, and spelling it out at both sites reads worse than this.
pub type Entries = Rc<Vec<(Rc<str>, JqVal)>>;

/// A value plus the metadata YAML recorded about it.
#[derive(Debug, Clone)]
pub struct YNode {
    pub meta: NodeMeta,
    /// The value itself. Never another `JqVal::Node` — [`crate::jqlang::JqVal::wrap`]
    /// collapses a re-wrap so `bare()` is one hop, not a chain.
    pub val: JqVal,
}

/// What the writer would put on the page for a bare value, with no style and no
/// metadata. Used by the reader to decide whether a scalar's source text needs
/// keeping.
pub fn quote_scalar_value(v: &JqVal) -> String {
    match v.bare() {
        JqVal::Null => "null".to_string(),
        JqVal::Bool(b) => b.to_string(),
        JqVal::Num(n, lit) => match lit {
            Some(t) => t.to_string(),
            None => crate::jqlang::fmt_num(*n),
        },
        JqVal::Str(s) => quote_scalar(s, Style::Plain),
        other => crate::jqlang::render(other),
    }
}

/// The tag a value resolves to when the node carries none, in `yq`'s spelling.
pub fn implicit_tag(v: &JqVal) -> &'static str {
    match v.bare() {
        JqVal::Null => "!!null",
        JqVal::Bool(_) => "!!bool",
        JqVal::Num(n, lit) => {
            let is_int = match lit {
                Some(l) => !l.contains(['.', 'e', 'E']),
                None => n.fract() == 0.0 && n.is_finite(),
            };
            if is_int {
                "!!int"
            } else {
                "!!float"
            }
        }
        JqVal::Str(_) => "!!str",
        JqVal::Arr(_) => "!!seq",
        JqVal::Obj(_) => "!!map",
        JqVal::Node(_) => "!!str",
    }
}

/// `yq`'s `kind`: the four shapes a YAML node can have.
pub fn kind_of(v: &JqVal) -> &'static str {
    if let JqVal::Node(n) = v {
        if !n.meta.alias.is_empty() {
            return "alias";
        }
    }
    match v.bare() {
        JqVal::Arr(_) => "seq",
        JqVal::Obj(_) => "map",
        _ => "scalar",
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The YAML writer
//
// This is the half that makes the metadata worth carrying: `yq '.' file.yaml`
// gives the file back essentially unchanged, and so must arb's `out.yaml`.
// Every rule below was measured against `yq v4.53.6` and is checked
// byte-for-byte by `yq_roundtrip_probe` in `scripts/jq_parity.sh`.
// ─────────────────────────────────────────────────────────────────────────────

/// Rendering options. `indent` is `yq`'s `-I`.
#[derive(Debug, Clone, Copy)]
pub struct Emit {
    pub indent: usize,
}

impl Default for Emit {
    fn default() -> Self {
        Emit { indent: 2 }
    }
}

/// Render a stream of documents. Two or more are separated by `---`, which is
/// what makes a multi-document file round-trip and what `splitDoc` relies on.
pub fn emit_docs(docs: &[JqVal], o: Emit) -> String {
    let mut out = String::new();
    for (i, d) in docs.iter().enumerate() {
        if i > 0 {
            out.push_str("---\n");
        }
        out.push_str(&emit_doc(d, o));
    }
    out
}

/// Render one document, newline-terminated.
pub fn emit_doc(v: &JqVal, o: Emit) -> String {
    let mut out = String::new();
    let m = v.meta();
    if let Some(m) = m {
        push_comment(&mut out, &m.head, 0);
    }
    match v.bare() {
        JqVal::Obj(map) if !map.is_empty() && !is_flow(v) && !is_alias(v) => {
            push_block_map(&mut out, entries(v, map), 0, o)
        }
        JqVal::Arr(a) if !a.is_empty() && !is_flow(v) && !is_alias(v) => {
            push_block_seq(&mut out, a, 0, o)
        }
        _ => {
            out.push_str(&inline(v, o));
            out.push('\n');
        }
    }
    if let Some(m) = m {
        push_comment(&mut out, &m.foot, 0);
    }
    out
}

fn is_flow(v: &JqVal) -> bool {
    v.meta().is_some_and(|m| m.style == Style::Flow)
}

fn is_alias(v: &JqVal) -> bool {
    v.meta().is_some_and(|m| !m.alias.is_empty())
}

/// Emit a comment block, one `# `-prefixed line per stored line.
fn push_comment(out: &mut String, text: &str, ind: usize) {
    if text.is_empty() {
        return;
    }
    for line in text.split('\n') {
        for _ in 0..ind {
            out.push(' ');
        }
        out.push('#');
        if !line.is_empty() {
            out.push(' ');
            out.push_str(line);
        }
        out.push('\n');
    }
}

/// The prefix a node writes before its own content: `&anchor` and `!tag`, in
/// the order `yq` emits them.
fn decorations(v: &JqVal) -> String {
    let Some(m) = v.meta() else {
        return String::new();
    };
    let mut s = String::new();
    if !m.anchor.is_empty() {
        s.push('&');
        s.push_str(&m.anchor);
        s.push(' ');
    }
    if !m.tag.is_empty() {
        s.push_str(&m.tag);
        s.push(' ');
    }
    s
}

/// The entries the writer walks: the mapping as it was WRITTEN when a `<<` merge
/// key was in it, and the value's own entries otherwise.
fn entries<'a>(v: &'a JqVal, map: &'a [(Rc<str>, JqVal)]) -> &'a [(Rc<str>, JqVal)] {
    match v.meta().and_then(|m| m.written.as_ref()) {
        Some(w) => w.as_slice(),
        None => map,
    }
}

/// Emit a block mapping at `ind`. Each entry is `head comment`, the `key: value`
/// line, then the key's foot comment — and a foot comment that is not the last
/// thing in the mapping is followed by the BLANK line that is what made it a
/// foot rather than the next entry's head.
fn push_block_map(out: &mut String, map: &[(Rc<str>, JqVal)], ind: usize, o: Emit) {
    for (i, (k, v)) in map.iter().enumerate() {
        let key_node = v.meta().and_then(|m| m.key.clone());
        let km = key_node.as_deref().and_then(JqVal::meta);
        let head: Rc<str> = km.map_or(Rc::from(""), |m| m.head.clone());
        let foot: Rc<str> = km.map_or(Rc::from(""), |m| m.foot.clone());
        push_comment(out, &head, ind);
        pad(out, ind);
        out.push_str(&scalar_key(k, key_node.as_deref()));
        out.push(':');
        push_value(out, v, ind, o);
        push_comment(out, &foot, ind);
        if !foot.is_empty() && i + 1 < map.len() {
            out.push('\n');
        }
    }
}

fn push_block_seq(out: &mut String, items: &[JqVal], ind: usize, o: Emit) {
    for item in items {
        if let Some(m) = item.meta() {
            push_comment(out, &m.head, ind);
        }
        pad(out, ind);
        out.push('-');
        match item.bare() {
            JqVal::Obj(map) if !map.is_empty() && !is_flow(item) && !is_alias(item) => {
                out.push(' ');
                out.push_str(&decorations(item));
                let mut body = String::new();
                push_block_map(&mut body, entries(item, map), ind + o.indent, o);
                // The mapping's FIRST line shares the `- ` the caller just wrote,
                // so its indent is dropped; every later line keeps the indent
                // that lines it up under the first key.
                out.push_str(body.trim_start_matches(' '));
            }
            JqVal::Arr(a) if !a.is_empty() && !is_flow(item) && !is_alias(item) => {
                out.push('\n');
                push_block_seq(out, a, ind + o.indent, o);
            }
            _ => {
                out.push(' ');
                out.push_str(&inline(item, o));
                push_line_comment(out, item);
                out.push('\n');
            }
        }
        if let Some(m) = item.meta() {
            push_comment(out, &m.foot, ind);
        }
    }
}

/// Write the value half of `key: …`, choosing between an inline scalar, a block
/// scalar and a nested collection.
fn push_value(out: &mut String, v: &JqVal, ind: usize, o: Emit) {
    let style = v.meta().map_or(Style::Plain, |m| m.style);
    match v.bare() {
        JqVal::Obj(map) if !map.is_empty() && !is_flow(v) && !is_alias(v) => {
            let deco = decorations(v);
            if !deco.is_empty() {
                out.push(' ');
                out.push_str(deco.trim_end());
            }
            push_line_comment(out, v);
            out.push('\n');
            push_block_map(out, entries(v, map), ind + o.indent, o);
        }
        JqVal::Arr(a) if !a.is_empty() && !is_flow(v) && !is_alias(v) => {
            let deco = decorations(v);
            if !deco.is_empty() {
                out.push(' ');
                out.push_str(deco.trim_end());
            }
            push_line_comment(out, v);
            out.push('\n');
            push_block_seq(out, a, ind + o.indent, o);
        }
        JqVal::Str(s) if style.is_block_scalar() && !is_alias(v) => {
            out.push(' ');
            out.push_str(&decorations(v));
            // A block scalar's trailing comment belongs on the INDICATOR line,
            // which is the line the reader took it from.
            let note = v.meta().map_or(Rc::from(""), |m| m.line.clone());
            push_block_scalar(out, s, style, &note, ind + o.indent);
        }
        _ => {
            let text = inline(v, o);
            if !text.is_empty() {
                out.push(' ');
                out.push_str(&text);
            }
            push_line_comment(out, v);
            out.push('\n');
        }
    }
}

fn push_line_comment(out: &mut String, v: &JqVal) {
    if let Some(m) = v.meta() {
        if !m.line.is_empty() {
            out.push_str(" # ");
            out.push_str(&m.line);
        }
    }
}

/// `|` / `>` with the body indented, plus the chomping indicator the text needs:
/// `-` when it does not end in a newline, `+` when it ends in more than one.
fn push_block_scalar(out: &mut String, s: &str, style: Style, note: &str, ind: usize) {
    out.push(if style == Style::Literal { '|' } else { '>' });
    let trailing = s.len() - s.trim_end_matches('\n').len();
    match trailing {
        0 => out.push('-'),
        1 => {}
        _ => out.push('+'),
    }
    if !note.is_empty() {
        out.push_str(" # ");
        out.push_str(note);
    }
    out.push('\n');
    let body = if trailing > 1 {
        &s[..s.len() - (trailing - 1)]
    } else {
        s
    };
    for line in body.trim_end_matches('\n').split('\n') {
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        pad(out, ind);
        out.push_str(line);
        out.push('\n');
    }
    for _ in 1..trailing {
        out.push('\n');
    }
}

fn pad(out: &mut String, n: usize) {
    for _ in 0..n {
        out.push(' ');
    }
}

/// A mapping KEY, quoted only when it has to be.
fn scalar_key(k: &str, node: Option<&JqVal>) -> String {
    let style = node.and_then(JqVal::meta).map_or(Style::Plain, |m| m.style);
    let deco = node.map(decorations).unwrap_or_default();
    format!("{deco}{}", quote_scalar(k, style))
}

/// Render a node on ONE line: a scalar, an alias, or a flow collection.
fn inline(v: &JqVal, o: Emit) -> String {
    // `o` rides along unused by the scalar arms; a flow collection needs it for
    // its children, which is why it is threaded rather than dropped.
    let _ = o;
    if let Some(m) = v.meta() {
        if !m.alias.is_empty() {
            return format!("*{}", m.alias);
        }
    }
    let deco = decorations(v);
    let style = v.meta().map_or(Style::Plain, |m| m.style);
    // A scalar whose source text the reader kept (`0x10`, `007`) is written back
    // as it was written.
    if let Some(m) = v.meta() {
        if !m.raw.is_empty() {
            return format!("{deco}{}", m.raw);
        }
    }
    let body = match v.bare() {
        // `key:` with nothing after it round-trips as nothing; a node written
        // `null` or `~` keeps the spelling it was written with (`raw` above).
        JqVal::Null if v.meta().is_some_and(|m| m.blank) && deco.is_empty() => {
            return String::new()
        }
        JqVal::Null => "null".to_string(),
        JqVal::Bool(b) => b.to_string(),
        JqVal::Num(n, lit) => match lit {
            Some(t) => t.to_string(),
            None => crate::jqlang::fmt_num(*n),
        },
        // `!!str 123` needs no quotes — the tag is what makes it a string, and
        // quoting it as well is not what the file said.
        JqVal::Str(s) if !deco.is_empty() && style == Style::Plain => s.to_string(),
        JqVal::Str(s) => quote_scalar(s, style),
        JqVal::Arr(a) => {
            let parts: Vec<String> = a.iter().map(|e| inline(e, o)).collect();
            format!("[{}]", parts.join(", "))
        }
        JqVal::Obj(m) => {
            if m.is_empty() {
                "{}".to_string()
            } else {
                let parts: Vec<String> = m
                    .iter()
                    .map(|(k, val)| {
                        let kn = val.meta().and_then(|mm| mm.key.clone());
                        format!("{}: {}", scalar_key(k, kn.as_deref()), inline(val, o))
                    })
                    .collect();
                format!("{{{}}}", parts.join(", "))
            }
        }
        JqVal::Node(_) => unreachable!("bare() never returns a Node"),
    };
    format!("{deco}{body}")
}

/// Quote a string exactly as far as YAML requires, in `yq`'s own two-tier rule
/// (both halves measured, `yq -n '["null","a: b"]'`):
///
/// * a text that would RE-READ as another type (`null`, `1.5`, `true`) takes
///   DOUBLE quotes, because the quoting is about the type, and
/// * a text that would break the SYNTAX (`a: b`, `#x`, `*x`, a leading `- `)
///   takes SINGLE quotes, because the quoting is about the characters.
///
/// Everything else is written plain, which is why `yes`, `no` and `a b` come
/// back unquoted where a naive "quote if in doubt" writer would not round-trip.
pub fn quote_scalar(s: &str, style: Style) -> String {
    match style {
        Style::Single => format!("'{}'", s.replace('\'', "''")),
        Style::Double => double_quote(s),
        _ => {
            if s.is_empty() || reparses_as_non_string(s) {
                return double_quote(s);
            }
            if needs_single_quote(s) {
                return format!("'{}'", s.replace('\'', "''"));
            }
            s.to_string()
        }
    }
}

/// Would this text, written plain, come back as something other than a string?
fn reparses_as_non_string(s: &str) -> bool {
    if matches!(s, "null" | "Null" | "NULL" | "~") {
        return true;
    }
    if matches!(s, "true" | "True" | "TRUE" | "false" | "False" | "FALSE") {
        return true;
    }
    if matches!(s, ".inf" | "-.inf" | ".nan" | ".Inf" | ".NaN") {
        return true;
    }
    let body = s.strip_prefix(['-', '+']).unwrap_or(s);
    !body.is_empty()
        && body
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-' | 'x' | 'o'))
        && body.chars().any(|c| c.is_ascii_digit())
}

/// Would this text, written plain, break the surrounding syntax?
fn needs_single_quote(s: &str) -> bool {
    let first = s.chars().next().unwrap_or(' ');
    if matches!(
        first,
        '#' | '&'
            | '*'
            | '!'
            | '|'
            | '>'
            | '%'
            | '@'
            | '`'
            | '{'
            | '}'
            | '['
            | ']'
            | ','
            | '\''
            | '"'
            | ' '
    ) {
        return true;
    }
    if (first == '-' || first == '?' || first == ':') && s.len() > 1 && s.as_bytes()[1] == b' ' {
        return true;
    }
    if s == "-" || s == "?" || s == ":" {
        return true;
    }
    if s.ends_with(' ') || s.contains('\n') || s.contains('\t') {
        return true;
    }
    // A `: ` or a ` #` inside the text would be read as a mapping separator or a
    // comment where the value is written.
    s.contains(": ") || s.contains(" #") || s.ends_with(':')
}

fn double_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
