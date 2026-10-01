//! `in.json`'s reading of the line stream: one item per JSON DOCUMENT.
//!
//! jq reads its input as a sequence of JSON texts, not lines: a pretty-printed
//! object spans many lines, and `1 2 3` on one line is three inputs. arb's
//! stream is lines, and SPEC §8 keeps a line that is not JSON as a string. This
//! regroups the lines under that rule:
//!
//! * a line holding exactly one JSON value passes through UNCHANGED (the common
//!   JSON-lines case, and the fast path);
//! * a line holding several values whitespace-separated (`1 2 3`,
//!   `{"a":1} {"a":2}`, `{"a":1}{"a":2}`) becomes one item per value;
//! * a line that OPENS a container it does not close starts a document that
//!   continues on the following lines; once balanced it is one item, rendered
//!   compactly so every item stays a single line;
//! * anything else — and any assembled document that turns out not to parse,
//!   or never closes before the stream ends — passes through as the raw lines
//!   it was, which is the text reading the SPEC already gives such lines.
//!
//! Only structure is scanned here (brackets, braces, strings and their
//! escapes); whether a candidate really is JSON is decided by
//! [`crate::jqlang::parse_json`]. Structural characters are ASCII and never
//! occur inside a multi-byte UTF-8 sequence, so the scan walks bytes.

use crate::jqlang::{parse_json, render};

/// Where a structural scan stands between bytes.
#[derive(Default, Clone, Copy)]
struct Scan {
    depth: usize,
    in_str: bool,
    escaped: bool,
}

/// What one byte did to the scan.
#[derive(PartialEq)]
enum Step {
    Inside,
    /// A top-level value just ended: its outermost container closed, or a
    /// string closed at depth 0.
    Closed,
    /// A closer with no opener: not JSON structure at all.
    Bad,
}

impl Scan {
    fn step(&mut self, b: u8) -> Step {
        if self.in_str {
            match b {
                _ if self.escaped => self.escaped = false,
                b'\\' => self.escaped = true,
                b'"' => {
                    self.in_str = false;
                    if self.depth == 0 {
                        return Step::Closed;
                    }
                }
                _ => {}
            }
            return Step::Inside;
        }
        match b {
            b'"' => self.in_str = true,
            b'[' | b'{' => self.depth += 1,
            b']' | b'}' => match self.depth {
                0 => return Step::Bad,
                1 => {
                    self.depth = 0;
                    return Step::Closed;
                }
                _ => self.depth -= 1,
            },
            _ => {}
        }
        Step::Inside
    }
}

/// How a line reads when no document is open.
enum Line {
    /// At most one value (a blank line holds none): the line is the item.
    One,
    /// Two or more complete top-level values, as byte spans.
    Values(Vec<(usize, usize)>),
    /// Balanced up to a container it opens and leaves open; the scan state at
    /// the end of the line.
    Opens(Scan),
    /// Neither.
    Text,
}

/// Classify a line: split it into top-level values (containers and strings end
/// where they close, any other token at whitespace or an opener), or report
/// that it ends inside a container it opened.
fn classify(line: &str) -> Line {
    let b = line.as_bytes();
    // The second span is what allocates: a one-value line, the common case,
    // never builds the list.
    let mut first = None;
    let mut spans = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        if matches!(b[i], b'[' | b'{' | b'"') {
            let mut scan = Scan::default();
            loop {
                if i == b.len() {
                    // Ran off the end inside the value. Only a CONTAINER may
                    // continue on the next line; a JSON string never does.
                    return if scan.depth > 0 {
                        Line::Opens(scan)
                    } else {
                        Line::Text
                    };
                }
                let step = scan.step(b[i]);
                i += 1;
                match step {
                    Step::Inside => {}
                    Step::Closed => break,
                    Step::Bad => return Line::Text,
                }
            }
        } else {
            while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'[' | b'{' | b'"')
            {
                if matches!(b[i], b']' | b'}') {
                    return Line::Text;
                }
                i += 1;
            }
        }
        match first {
            None => first = Some((start, i)),
            Some(f) => {
                if spans.is_empty() {
                    spans.push(f);
                }
                spans.push((start, i));
            }
        }
    }
    if spans.is_empty() {
        Line::One
    } else {
        Line::Values(spans)
    }
}

/// The regrouping state between lines: the lines of a document still open.
#[derive(Default)]
pub struct JsonDocs {
    pending: Vec<String>,
    scan: Scan,
}

impl JsonDocs {
    /// Is `line` its own item, untouched? True for the common one-value line
    /// when no document is open, which lets a caller skip [`JsonDocs::push`]
    /// and its copy.
    pub fn passes_through(&self, line: &str) -> bool {
        self.pending.is_empty() && matches!(classify(line), Line::One)
    }

    /// Read one input line, appending every item it completes to `out`.
    pub fn push(&mut self, line: &str, out: &mut Vec<String>) {
        if !self.pending.is_empty() {
            self.continue_doc(line, out);
            return;
        }
        match classify(line) {
            Line::One => out.push(line.to_string()),
            // Several values: one item each, but only when every one of them
            // is JSON — `200 OK` is a text line, not the number 200.
            Line::Values(spans) => {
                let parts: Vec<&str> = spans.iter().map(|&(a, z)| &line[a..z]).collect();
                if parts.iter().all(|p| parse_json(p).is_ok()) {
                    out.extend(parts.into_iter().map(str::to_string));
                } else {
                    out.push(line.to_string());
                }
            }
            // A document starts here only if nothing before the opener was
            // text: `1 [2,` is not one.
            Line::Opens(scan) if starts_with_opener(line) => {
                self.scan = scan;
                self.pending.push(line.to_string());
            }
            Line::Opens(_) | Line::Text => out.push(line.to_string()),
        }
    }

    /// The stream ended: a document still open never closed, so its lines are
    /// text after all.
    pub fn finish(&mut self, out: &mut Vec<String>) {
        out.append(&mut self.pending);
        self.scan = Scan::default();
    }

    fn continue_doc(&mut self, line: &str, out: &mut Vec<String>) {
        let mut end = None;
        for (i, &b) in line.as_bytes().iter().enumerate() {
            match self.scan.step(b) {
                Step::Inside => {}
                Step::Closed => {
                    end = Some(i + 1);
                    break;
                }
                Step::Bad => {
                    let lines = std::mem::take(&mut self.pending);
                    self.scan = Scan::default();
                    self.replay(lines, line, out);
                    return;
                }
            }
        }
        let Some(end) = end else {
            self.pending.push(line.to_string());
            return;
        };
        let mut text = self.pending.join("\n");
        text.push('\n');
        text.push_str(&line[..end]);
        let lines = std::mem::take(&mut self.pending);
        self.scan = Scan::default();
        match parse_json(&text) {
            Ok(v) => {
                out.push(render(&v));
                // Whatever follows the closer on this line is read afresh.
                let rest = line[end..].trim_start();
                if !rest.is_empty() {
                    self.push(rest, out);
                }
            }
            Err(_) => self.replay(lines, line, out),
        }
    }

    /// Give back lines that did not make a document: the first one is text,
    /// and the rest are read again from the top, since a document may start
    /// on any of them.
    fn replay(&mut self, lines: Vec<String>, last: &str, out: &mut Vec<String>) {
        let mut it = lines.into_iter();
        if let Some(first) = it.next() {
            out.push(first);
        }
        for l in it {
            self.push(&l, out);
        }
        self.push(last, out);
    }
}

/// Does the line's first non-blank byte open a container?
fn starts_with_opener(line: &str) -> bool {
    matches!(line.trim_start().as_bytes().first(), Some(b'[' | b'{'))
}

/// Regroup a whole buffer of lines (the batch path).
pub fn documents(lines: &[String]) -> Vec<String> {
    let mut docs = JsonDocs::default();
    let mut out = Vec::with_capacity(lines.len());
    for l in lines {
        docs.push(l, &mut out);
    }
    docs.finish(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docs(lines: &[&str]) -> Vec<String> {
        documents(&lines.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn one_value_lines_pass_through_unchanged() {
        assert_eq!(
            docs(&[r#"{"a": 1.50}"#, "abc", "", r#""x y""#]),
            [r#"{"a": 1.50}"#, "abc", "", r#""x y""#]
        );
    }

    #[test]
    fn several_values_on_a_line_split() {
        assert_eq!(docs(&["1 2 3"]), ["1", "2", "3"]);
        assert_eq!(docs(&[r#"{"a":1}{"a":2}"#]), [r#"{"a":1}"#, r#"{"a":2}"#]);
        assert_eq!(docs(&[r#""a" [1] "b""#]), [r#""a""#, "[1]", r#""b""#]);
        // A text line keeps its text reading.
        assert_eq!(docs(&["200 OK"]), ["200 OK"]);
        assert_eq!(docs(&["[INFO] started"]), ["[INFO] started"]);
    }

    #[test]
    fn a_pretty_document_is_one_compact_item() {
        assert_eq!(
            docs(&["{", r#"  "a": [1,"#, "  2.50],", r#"  "s": "}{""#, "}", "7"]),
            [r#"{"a":[1,2.50],"s":"}{"}"#, "7"]
        );
        assert_eq!(docs(&["[1,", "2] [3,", "4]"]), ["[1,2]", "[3,4]"]);
    }

    #[test]
    fn what_never_makes_a_document_stays_text() {
        assert_eq!(docs(&["{", "not json", "}"]), ["{", "not json", "}"]);
        assert_eq!(docs(&["[INFO", "x"]), ["[INFO", "x"]);
        assert_eq!(docs(&["a]", "1 [2,", "3]"]), ["a]", "1 [2,", "3]"]);
        assert_eq!(docs(&[r#""open"#, "x"]), [r#""open"#, "x"]);
    }
}
