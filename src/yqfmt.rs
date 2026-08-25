//! The encoders, decoders and clock `yq` exposes as operators.
//!
//! `to_props`/`from_props`, `to_xml`/`from_xml`, `to_csv`/`to_tsv` and their
//! readers, plus the Go reference-layout date formatting behind
//! `format_datetime`/`from_unix`/`to_unix`/`tz`/`with_dtf`. They live beside the
//! node model rather than inside the jq evaluator because none of them touch
//! evaluation — each is a pure value-to-text or text-to-value function, and
//! every shape below was measured against `yq v4.53.6` rather than transcribed
//! from its documentation.

use crate::jqlang::{render_raw, JqVal};
use std::rc::Rc;

// ─────────────────────────────────────────────────────────────────────────────
// Java properties
// ─────────────────────────────────────────────────────────────────────────────

/// `to_props`: one `dotted.path = value` line per leaf, arrays indexed by
/// position (`l.0 = 1`), in document order, with every comment the nodes carry
/// written above the entry it belongs to — which is what `yq -o=props` emits.
pub fn to_props(v: &JqVal) -> String {
    let mut out = String::new();
    if let Some(m) = v.meta() {
        push_props_comment(&mut out, &m.head);
    }
    walk_props(v, &mut String::new(), &mut out, true);
    out
}

fn push_props_comment(out: &mut String, text: &str) {
    for line in text.split('\n') {
        if text.is_empty() {
            return;
        }
        out.push('#');
        if !line.is_empty() {
            out.push(' ');
            out.push_str(line);
        }
        out.push('\n');
    }
}

/// A leaf's text, in the `.properties` escaping: a backslash, newline, tab or
/// LEADING space is escaped, a null writes nothing, and a scalar whose SOURCE
/// text the reader kept (`0x10`) is written as it was written.
fn props_leaf(v: &JqVal) -> String {
    // A node written with no text at all writes none here either; one written
    // `null` or `~` keeps that spelling, which is what `yq -o=props` emits for
    // the three of them.
    if v.meta().is_some_and(|m| m.blank) {
        return String::new();
    }
    // A kept SOURCE text is the right thing for a PLAIN scalar (`0x10` is
    // `0x10` here, not `16`) and the wrong thing for a block scalar, where it is
    // the YAML body rather than the value: a `>` folded block's value is the
    // FOLDED text, and `yq -o=props` writes that.
    let raw = v
        .meta()
        .filter(|m| !m.raw.is_empty() && !m.style.is_block_scalar());
    let text = match raw {
        Some(m) => m.raw.to_string(),
        None => match v.bare() {
            JqVal::Null => "null".to_string(),
            other => render_raw(other),
        },
    };
    let mut out = String::with_capacity(text.len());
    for (i, c) in text.chars().enumerate() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            ' ' if i == 0 => out.push_str("\\ "),
            c => out.push(c),
        }
    }
    out
}

fn walk_props(v: &JqVal, prefix: &mut String, out: &mut String, root: bool) {
    match v.bare() {
        JqVal::Obj(m) => {
            for (k, val) in m.iter() {
                let at = prefix.len();
                if !prefix.is_empty() {
                    prefix.push('.');
                }
                prefix.push_str(k);
                walk_props(val, prefix, out, false);
                prefix.truncate(at);
            }
        }
        JqVal::Arr(a) => {
            for (i, e) in a.iter().enumerate() {
                let at = prefix.len();
                if !prefix.is_empty() {
                    prefix.push('.');
                }
                prefix.push_str(&i.to_string());
                walk_props(e, prefix, out, false);
                prefix.truncate(at);
            }
        }
        _ => {
            // Only a LEAF carries comments into this format, and its key's head
            // and its own trailing comment go on ONE line, separated from the
            // previous entry by a blank. Every rule here is `yq -o=props`'s
            // (`# d head d line` above `c.d = 4`); a foot comment and a comment
            // on a non-leaf key have nowhere to go and are dropped, which is
            // what the reference does with them too.
            let mut note: Vec<&str> = Vec::new();
            if let Some(m) = v.meta() {
                match m.key.as_deref().and_then(JqVal::meta) {
                    // A mapping entry's head comment lives on its KEY node.
                    Some(k) => note.extend(k.head.split('\n').filter(|l| !l.is_empty())),
                    // A SEQUENCE item has no key, so its head is its own — except
                    // at the document root, whose head `to_props` already wrote.
                    None if !root => note.extend(m.head.split('\n').filter(|l| !l.is_empty())),
                    None => {}
                }
                note.extend(std::iter::once(&*m.line).filter(|l| !l.is_empty()));
            }
            if !note.is_empty() {
                if out.ends_with("\n")
                    && !out.trim_end().ends_with('#')
                    && out.lines().last().is_some_and(|l| !l.starts_with('#'))
                {
                    out.push('\n');
                }
                out.push_str("# ");
                out.push_str(&note.join(" "));
                out.push('\n');
            }
            out.push_str(prefix);
            out.push_str(" = ");
            out.push_str(&props_leaf(v));
            out.push('\n');
        }
    }
}

/// `from_props`: read `key = value` / `key: value` lines back into a nested
/// value. Every leaf is a STRING, which is what `yq -p=props` produces — the
/// format has no types, and inventing them would make `from_props` disagree with
/// the reference on `a.b = 1`.
pub fn from_props(src: &str) -> JqVal {
    let mut root = JqVal::obj(Vec::new());
    for line in src.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let Some(at) = line.find(['=', ':']) else {
            continue;
        };
        let (k, v) = line.split_at(at);
        let key = k.trim();
        let val = v[1..].trim();
        if key.is_empty() {
            continue;
        }
        let segs: Vec<&str> = key.split('.').collect();
        root = insert_path(&root, &segs, JqVal::str(val));
    }
    root
}

/// Insert `val` at a dotted path, creating objects as it goes. A segment that is
/// all digits AND lands on a slot that is already an array indexes it; the
/// reference reads `l.0` as a list element, and that is the only way to spell
/// one in this format.
fn insert_path(cur: &JqVal, segs: &[&str], val: JqVal) -> JqVal {
    let Some((head, rest)) = segs.split_first() else {
        return val;
    };
    if rest.is_empty() {
        return put(cur, head, val);
    }
    let child = get(cur, head).unwrap_or_else(|| {
        if rest[0].chars().all(|c| c.is_ascii_digit()) {
            JqVal::arr(Vec::new())
        } else {
            JqVal::obj(Vec::new())
        }
    });
    put(cur, head, insert_path(&child, rest, val))
}

fn get(cur: &JqVal, key: &str) -> Option<JqVal> {
    match cur.bare() {
        JqVal::Obj(m) => m.iter().find(|(k, _)| &**k == key).map(|(_, v)| v.clone()),
        JqVal::Arr(a) => key.parse::<usize>().ok().and_then(|i| a.get(i).cloned()),
        _ => None,
    }
}

fn put(cur: &JqVal, key: &str, val: JqVal) -> JqVal {
    match cur.bare() {
        JqVal::Arr(a) => {
            let Ok(i) = key.parse::<usize>() else {
                return cur.clone();
            };
            let mut items = a.as_ref().clone();
            while items.len() <= i {
                items.push(JqVal::Null);
            }
            items[i] = val;
            JqVal::arr(items)
        }
        JqVal::Obj(m) => {
            let mut pairs = m.as_ref().clone();
            match pairs.iter_mut().find(|(k, _)| &**k == key) {
                Some(slot) => slot.1 = val,
                None => pairs.push((Rc::from(key), val)),
            }
            JqVal::obj(pairs)
        }
        _ => JqVal::obj(vec![(Rc::from(key), val)]),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// XML
// ─────────────────────────────────────────────────────────────────────────────

/// `to_xml`: one element per key, an array key repeated once per element, and a
/// leaf written as text. Attribute keys (`+@name`) and the content key
/// (`+content`) are the two conventions `yq` uses in both directions.
pub fn to_xml(v: &JqVal, indent: usize) -> String {
    let mut out = String::new();
    write_xml(v, &mut out, 0, indent);
    out
}

fn write_xml(v: &JqVal, out: &mut String, depth: usize, indent: usize) {
    let JqVal::Obj(m) = v.bare() else {
        out.push_str(&render_raw(v));
        return;
    };
    for (k, val) in m.iter() {
        if k.starts_with("+@") || &**k == "+content" {
            continue;
        }
        let items: Vec<&JqVal> = match val.bare() {
            JqVal::Arr(a) => a.iter().collect(),
            _ => vec![val],
        };
        for item in items {
            for _ in 0..depth * indent {
                out.push(' ');
            }
            out.push('<');
            out.push_str(k);
            if let JqVal::Obj(im) = item.bare() {
                for (ak, av) in im.iter() {
                    if let Some(name) = ak.strip_prefix("+@") {
                        out.push_str(&format!(" {name}=\"{}\"", xml_escape(&render_raw(av))));
                    }
                }
            }
            out.push('>');
            match item.bare() {
                JqVal::Obj(im) if im.iter().any(|(ik, _)| !ik.starts_with("+@")) => {
                    match im.iter().find(|(ik, _)| &**ik == "+content") {
                        Some((_, c)) => out.push_str(&xml_escape(&render_raw(c))),
                        None => {
                            out.push('\n');
                            write_xml(item, out, depth + 1, indent);
                            for _ in 0..depth * indent {
                                out.push(' ');
                            }
                        }
                    }
                }
                other => out.push_str(&xml_escape(&render_raw(other))),
            }
            out.push_str(&format!("</{k}>\n"));
        }
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `from_xml`: a small well-formed-XML reader in `yq`'s shape — elements become
/// keys, repeated siblings become an array, attributes become `+@name` and mixed
/// text becomes `+content`. Every leaf is a STRING, as `yq -p=xml` produces.
pub fn from_xml(src: &str) -> JqVal {
    let mut p = XmlParser {
        b: src.as_bytes(),
        i: 0,
    };
    let mut pairs: Vec<(Rc<str>, JqVal)> = Vec::new();
    while let Some((name, val)) = p.element() {
        add_child(&mut pairs, &name, val);
    }
    JqVal::obj(pairs)
}

/// Append `val` under `name`, promoting to an array when the name repeats —
/// which is how XML's repeated siblings reach a value model that has no
/// duplicate keys.
fn add_child(pairs: &mut Vec<(Rc<str>, JqVal)>, name: &str, val: JqVal) {
    match pairs.iter_mut().find(|(k, _)| &**k == name) {
        Some(slot) => {
            let items = match slot.1.bare() {
                JqVal::Arr(a) => {
                    let mut v = a.as_ref().clone();
                    v.push(val);
                    v
                }
                other => vec![other.clone(), val],
            };
            slot.1 = JqVal::arr(items);
        }
        None => pairs.push((Rc::from(name), val)),
    }
}

struct XmlParser<'a> {
    b: &'a [u8],
    i: usize,
}

impl XmlParser<'_> {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    /// Parse the next element, or `None` at a closing tag / end of input.
    fn element(&mut self) -> Option<(String, JqVal)> {
        self.skip_ws();
        if self.i >= self.b.len() || self.b[self.i] != b'<' {
            return None;
        }
        // A declaration, comment or closing tag is not an element start.
        if matches!(self.b.get(self.i + 1), Some(b'/') | Some(b'?') | Some(b'!')) {
            return None;
        }
        self.i += 1;
        let name = self.take_name();
        let mut attrs: Vec<(Rc<str>, JqVal)> = Vec::new();
        loop {
            self.skip_ws();
            match self.b.get(self.i) {
                Some(b'>') => {
                    self.i += 1;
                    break;
                }
                Some(b'/') => {
                    self.i += 2; // `/>`
                    return Some((name, JqVal::obj(attrs)));
                }
                None => return Some((name, JqVal::obj(attrs))),
                _ => {
                    let an = self.take_name();
                    if an.is_empty() {
                        self.i += 1;
                        continue;
                    }
                    self.skip_ws();
                    let mut av = String::new();
                    if self.b.get(self.i) == Some(&b'=') {
                        self.i += 1;
                        self.skip_ws();
                        av = self.take_quoted();
                    }
                    attrs.push((Rc::from(format!("+@{an}").as_str()), JqVal::str(av)));
                }
            }
        }
        let mut kids: Vec<(Rc<str>, JqVal)> = Vec::new();
        let mut text = String::new();
        loop {
            let before = self.i;
            while self.i < self.b.len() && self.b[self.i] != b'<' {
                self.i += 1;
            }
            text.push_str(&xml_unescape(&String::from_utf8_lossy(
                &self.b[before..self.i],
            )));
            if self.b.get(self.i + 1) == Some(&b'/') || self.i >= self.b.len() {
                // Consume `</name>`.
                while self.i < self.b.len() && self.b[self.i] != b'>' {
                    self.i += 1;
                }
                self.i += 1;
                break;
            }
            match self.element() {
                Some((kn, kv)) => add_child(&mut kids, &kn, kv),
                None => break,
            }
        }
        let text = text.trim().to_string();
        if kids.is_empty() && attrs.is_empty() {
            return Some((name, JqVal::str(text)));
        }
        let mut pairs = kids;
        if !text.is_empty() {
            pairs.insert(0, (Rc::from("+content"), JqVal::str(text)));
        }
        pairs.extend(attrs);
        Some((name, JqVal::obj(pairs)))
    }

    fn take_name(&mut self) -> String {
        let start = self.i;
        while self.i < self.b.len()
            && !self.b[self.i].is_ascii_whitespace()
            && !matches!(self.b[self.i], b'>' | b'/' | b'=')
        {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.b[start..self.i]).into_owned()
    }

    fn take_quoted(&mut self) -> String {
        let q = match self.b.get(self.i) {
            Some(&c @ (b'"' | b'\'')) => {
                self.i += 1;
                c
            }
            _ => return String::new(),
        };
        let start = self.i;
        while self.i < self.b.len() && self.b[self.i] != q {
            self.i += 1;
        }
        let s = String::from_utf8_lossy(&self.b[start..self.i]).into_owned();
        self.i += 1;
        xml_unescape(&s)
    }
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

// ─────────────────────────────────────────────────────────────────────────────
// CSV / TSV
// ─────────────────────────────────────────────────────────────────────────────

/// `to_csv` / `to_tsv`. An array of OBJECTS gets a header row from the first
/// object's keys; an array of arrays is written row for row.
pub fn to_delim(v: &JqVal, sep: char) -> String {
    let JqVal::Arr(rows) = v.bare() else {
        return String::new();
    };
    let mut out = String::new();
    let header: Vec<Rc<str>> = match rows.first().map(JqVal::bare) {
        Some(JqVal::Obj(m)) => m.iter().map(|(k, _)| k.clone()).collect(),
        _ => Vec::new(),
    };
    if !header.is_empty() {
        push_row(&mut out, header.iter().map(|k| k.to_string()), sep);
    }
    for row in rows.iter() {
        match row.bare() {
            JqVal::Obj(_) => push_row(
                &mut out,
                header
                    .iter()
                    .map(|k| row.bare().obj_lookup(k).map(render_raw).unwrap_or_default()),
                sep,
            ),
            JqVal::Arr(cells) => push_row(&mut out, cells.iter().map(render_raw), sep),
            other => push_row(&mut out, std::iter::once(render_raw(other)), sep),
        }
    }
    out
}

fn push_row(out: &mut String, cells: impl Iterator<Item = String>, sep: char) {
    let parts: Vec<String> = cells.map(|c| quote_cell(&c, sep)).collect();
    out.push_str(&parts.join(&sep.to_string()));
    out.push('\n');
}

fn quote_cell(c: &str, sep: char) -> String {
    if c.contains(sep) || c.contains('"') || c.contains('\n') {
        format!("\"{}\"", c.replace('"', "\"\""))
    } else {
        c.to_string()
    }
}

/// `from_csv` / `from_tsv`: the first row is the header, every later row becomes
/// an object. Cells that parse as numbers become numbers, which is what
/// `yq -p=csv` does.
pub fn from_delim(src: &str, sep: char) -> JqVal {
    let mut rows = parse_delim(src, sep);
    if rows.is_empty() {
        return JqVal::arr(Vec::new());
    }
    let header = rows.remove(0);
    let out: Vec<JqVal> = rows
        .into_iter()
        .map(|r| {
            let pairs: Vec<(Rc<str>, JqVal)> = header
                .iter()
                .enumerate()
                .map(|(i, h)| {
                    let cell = r.get(i).cloned().unwrap_or_default();
                    (Rc::from(h.as_str()), cell_value(&cell))
                })
                .collect();
            JqVal::obj(pairs)
        })
        .collect();
    JqVal::arr(out)
}

fn cell_value(c: &str) -> JqVal {
    match c.parse::<f64>() {
        Ok(n) if !c.is_empty() => crate::jqlang::num_from_literal(n, c),
        _ => JqVal::str(c),
    }
}

/// RFC-4180 splitting: `"` opens a quoted cell, `""` is a literal quote inside
/// one, and a separator or newline inside quotes is data.
fn parse_delim(src: &str, sep: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut cs = src.chars().peekable();
    while let Some(c) = cs.next() {
        if quoted {
            if c == '"' {
                if cs.peek() == Some(&'"') {
                    cs.next();
                    cell.push('"');
                } else {
                    quoted = false;
                }
            } else {
                cell.push(c);
            }
            continue;
        }
        match c {
            '"' if cell.is_empty() => quoted = true,
            c if c == sep => row.push(std::mem::take(&mut cell)),
            '\n' => {
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
            }
            '\r' => {}
            c => cell.push(c),
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}

// ─────────────────────────────────────────────────────────────────────────────
// Dates — Go's reference-layout formatting
// ─────────────────────────────────────────────────────────────────────────────

/// Format `secs` (a Unix timestamp) with a Go reference layout such as
/// `2006-01-02T15:04:05Z07:00`.
///
/// Go names each field by its value in the reference instant
/// `Mon Jan 2 15:04:05 MST 2006`, so the layout IS an example rather than a set
/// of `%` codes. The tokens below are the ones `yq`'s own default layouts use,
/// longest first so `2006` is not read as `20` then `06`.
pub fn format_go(secs: f64, layout: &str, utc: bool) -> String {
    let tm = broken_down(secs, utc);
    let mut out = String::new();
    let b = layout.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let rest = &layout[i..];
        let mut matched = false;
        for (tok, val) in go_tokens(&tm, utc) {
            if rest.starts_with(tok) {
                out.push_str(&val);
                i += tok.len();
                matched = true;
                break;
            }
        }
        if !matched {
            let c = rest.chars().next().unwrap_or(' ');
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// The Go layout tokens, longest first. Order is the contract here: `2006` must
/// be tried before `2`, and `15` before `1`.
fn go_tokens(tm: &libc::tm, utc: bool) -> Vec<(&'static str, String)> {
    let hour12 = match tm.tm_hour % 12 {
        0 => 12,
        h => h,
    };
    let zone = if utc {
        "Z".to_string()
    } else {
        let off = tm.tm_gmtoff;
        let sign = if off < 0 { '-' } else { '+' };
        format!(
            "{sign}{:02}:{:02}",
            off.abs() / 3600,
            (off.abs() % 3600) / 60
        )
    };
    vec![
        ("2006", format!("{:04}", tm.tm_year + 1900)),
        ("January", MONTHS[tm.tm_mon as usize % 12].to_string()),
        ("Monday", DAYS[tm.tm_wday as usize % 7].to_string()),
        ("Z07:00", zone.clone()),
        ("-07:00", zone),
        ("01", format!("{:02}", tm.tm_mon + 1)),
        ("02", format!("{:02}", tm.tm_mday)),
        ("03", format!("{hour12:02}")),
        ("04", format!("{:02}", tm.tm_min)),
        ("05", format!("{:02}", tm.tm_sec)),
        ("15", format!("{:02}", tm.tm_hour)),
        ("Jan", MONTHS[tm.tm_mon as usize % 12][..3].to_string()),
        ("Mon", DAYS[tm.tm_wday as usize % 7][..3].to_string()),
        ("PM", if tm.tm_hour < 12 { "AM" } else { "PM" }.to_string()),
        ("pm", if tm.tm_hour < 12 { "am" } else { "pm" }.to_string()),
        ("06", format!("{:02}", (tm.tm_year + 1900) % 100)),
        ("1", format!("{}", tm.tm_mon + 1)),
        ("2", format!("{}", tm.tm_mday)),
        ("3", format!("{hour12}")),
        ("4", format!("{}", tm.tm_min)),
        ("5", format!("{}", tm.tm_sec)),
    ]
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn broken_down(secs: f64, utc: bool) -> libc::tm {
    let t = secs.trunc() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        if utc {
            libc::gmtime_r(&t, &mut tm);
        } else {
            libc::localtime_r(&t, &mut tm);
        }
    }
    tm
}

/// Read an RFC-3339 timestamp (or a bare `YYYY-MM-DD`) back to a Unix second
/// count. `to_unix`'s input, and the only shape `yq` accepts there.
pub fn parse_rfc3339(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let num = |a: usize, n: usize| -> Option<i64> { s.get(a..a + n)?.parse::<i64>().ok() };
    if b.len() < 10 {
        return None;
    }
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (mut h, mut mi, mut sec) = (0, 0, 0);
    if b.len() >= 19 && matches!(b[10], b'T' | b't' | b' ') {
        h = num(11, 2)?;
        mi = num(14, 2)?;
        sec = num(17, 2)?;
    }
    // Days since the epoch by Howard Hinnant's civil-from-days, which is exact
    // for the whole proleptic Gregorian range and needs no table.
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut total = days * 86_400 + h * 3600 + mi * 60 + sec;
    // A trailing offset shifts the instant; `Z` (or nothing) is UTC.
    if let Some(at) = s.rfind(['+', '-']).filter(|&i| i > 10) {
        let sign: i64 = if b[at] == b'-' { 1 } else { -1 };
        let oh = num(at + 1, 2).unwrap_or(0);
        let om = num(at + 4, 2).unwrap_or(0);
        total += sign * (oh * 3600 + om * 60);
    }
    Some(total as f64)
}

// ─────────────────────────────────────────────────────────────────────────────
// Reshaping
// ─────────────────────────────────────────────────────────────────────────────

/// `pivot`: transpose an array of objects into an object of arrays, so
/// `[{a:1,b:2},{a:3,b:4}]` becomes `{a:[1,3], b:[2,4]}`. Keys appear in first-seen
/// order and a row missing a key contributes `null` at its position.
pub fn pivot(v: &JqVal) -> JqVal {
    let JqVal::Arr(rows) = v.bare() else {
        return v.clone();
    };
    let mut keys: Vec<Rc<str>> = Vec::new();
    for r in rows.iter() {
        if let JqVal::Obj(m) = r.bare() {
            for (k, _) in m.iter() {
                if !keys.iter().any(|e| e == k) {
                    keys.push(k.clone());
                }
            }
        }
    }
    let pairs = keys
        .into_iter()
        .map(|k| {
            let col: Vec<JqVal> = rows
                .iter()
                .map(|r| r.bare().obj_lookup(&k).cloned().unwrap_or(JqVal::Null))
                .collect();
            (k, JqVal::arr(col))
        })
        .collect();
    JqVal::obj(pairs)
}

/// `shuffle`: a Fisher-Yates shuffle over an xorshift64* stream seeded from the
/// clock. `yq` shuffles too; neither is reproducible, and neither claims to be.
pub fn shuffle(v: &JqVal) -> JqVal {
    let JqVal::Arr(a) = v.bare() else {
        return v.clone();
    };
    let mut items = a.as_ref().clone();
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x2545_F491_4F6C_DD1D, |d| d.as_nanos() as u64)
        | 1;
    for i in (1..items.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        items.swap(i, (state % (i as u64 + 1)) as usize);
    }
    JqVal::arr(items)
}

/// `sort_keys`: sort a mapping's keys, recursively where `deep`. The VALUES keep
/// their node metadata, so sorting a commented document keeps every comment on
/// the entry it was written against.
pub fn sort_keys(v: &JqVal, deep: bool) -> JqVal {
    let rewrap = |built: JqVal| match v.meta() {
        Some(m) => JqVal::wrap(built, m.clone()),
        None => built,
    };
    match v.bare() {
        JqVal::Obj(m) => {
            let mut pairs = m.as_ref().clone();
            pairs.sort_by(|(a, _), (b, _)| a.cmp(b));
            if deep {
                for slot in pairs.iter_mut() {
                    slot.1 = sort_keys(&slot.1, true);
                }
            }
            rewrap(JqVal::obj(pairs))
        }
        JqVal::Arr(a) if deep => rewrap(JqVal::arr(
            a.iter().map(|e| sort_keys(e, true)).collect::<Vec<_>>(),
        )),
        _ => v.clone(),
    }
}

/// `envsubst`: expand `$VAR` and `${VAR}` from the process environment. An
/// undefined name expands to the empty string, which is what `yq` does.
pub fn envsubst(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut cs = s.chars().peekable();
    while let Some(c) = cs.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = cs.peek() == Some(&'{');
        if braced {
            cs.next();
        }
        let mut name = String::new();
        while let Some(&n) = cs.peek() {
            if n.is_alphanumeric() || n == '_' {
                name.push(n);
                cs.next();
            } else {
                break;
            }
        }
        if braced && cs.peek() == Some(&'}') {
            cs.next();
        }
        if name.is_empty() {
            out.push('$');
            continue;
        }
        out.push_str(&std::env::var(&name).unwrap_or_default());
    }
    out
}
