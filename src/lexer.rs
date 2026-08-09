//! Tcl-flavored lexer.
//!
//! Words are whitespace-separated. `{ ... }` is a verbatim (brace-quoted) block
//! whose inner text is re-lexed by the parser when it is known to be a command
//! body. `"..."` is a literal string (no interpolation in M1). `#` begins a
//! comment to end-of-line, but only where a command is expected (start of line,
//! after `;`). `;` and newline terminate commands.

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// A bare word: paths (`.x`), flags (`-opt`), values, `<-`, verbs.
    Word(String),
    /// A `"..."` literal.
    Str(String),
    /// Raw inner text of a `{ ... }` block, parsed recursively by the parser.
    Block(String),
    /// Command terminator: `;` or newline.
    Sep,
}

/// Tokenize a spec source string. Each token carries its start char-offset (used
/// by the LSP to anchor a diagnostic); errors carry the offending span.
///
/// `jq_ok` says whether a jq literal may start a command here. It is true only
/// inside a `{ … }` body, where a leading `.` is unambiguous. At top level a
/// command legitimately starts with a widget path — `.x <- in` (bind shorthand)
/// and `.g configure -max 200` — so reading `.`-first commands as jq literals
/// there would swallow those whole.
pub fn lex(src: &str, jq_ok: bool) -> Result<Vec<(Tok, usize)>, crate::err::SpecError> {
    use crate::err::SpecError;
    let cs: Vec<char> = src.chars().collect();
    let n = cs.len();
    let mut i = 0;
    let mut toks = Vec::new();
    // `#` is a comment only where a command is expected.
    let mut at_cmd_start = true;

    while i < n {
        let c = cs[i];
        match c {
            ' ' | '\t' | '\r' => i += 1,
            '\n' | ';' => {
                toks.push((Tok::Sep, i));
                i += 1;
                at_cmd_start = true;
            }
            '#' if at_cmd_start => {
                while i < n && cs[i] != '\n' {
                    i += 1;
                }
            }
            '"' => {
                let q = i;
                i += 1;
                let mut s = String::new();
                while i < n && cs[i] != '"' {
                    if cs[i] == '\\' && i + 1 < n {
                        i += 1;
                        s.push(match cs[i] {
                            'n' => '\n',
                            't' => '\t',
                            o => o,
                        });
                    } else {
                        s.push(cs[i]);
                    }
                    i += 1;
                }
                if i >= n {
                    return Err(SpecError {
                        msg: "unterminated string".into(),
                        span: Some((q, n)),
                    });
                }
                i += 1; // closing quote
                toks.push((Tok::Str(s), q));
                at_cmd_start = false;
            }
            '{' => {
                let open = i;
                let mut depth = 1;
                i += 1;
                let start = i;
                while i < n {
                    match cs[i] {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        // Skip a "..." string so a `{`/`}` inside it does not
                        // miscount block depth (honoring the `\"` escape). An
                        // unterminated string runs to EOF -> unterminated block.
                        '"' => {
                            i += 1;
                            while i < n && cs[i] != '"' {
                                if cs[i] == '\\' && i + 1 < n {
                                    i += 1;
                                }
                                i += 1;
                            }
                        }
                        // Skip a /regex/ literal so a `{`/`}` in the pattern does
                        // not miscount depth — but only when it actually closes
                        // before end-of-line. A non-closing `/` (division, a path)
                        // is treated as an ordinary char, matching the main lexer.
                        '/' => {
                            let mut j = i + 1;
                            let mut closed = false;
                            while j < n {
                                match cs[j] {
                                    '\\' if j + 1 < n => j += 2,
                                    '/' => {
                                        closed = true;
                                        break;
                                    }
                                    '\n' => break,
                                    _ => j += 1,
                                }
                            }
                            if closed {
                                i = j; // land on the closing `/`; the `i += 1` below steps past it
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                if depth != 0 {
                    return Err(SpecError {
                        msg: "unterminated block".into(),
                        span: Some((open, n)),
                    });
                }
                let inner: String = cs[start..i].iter().collect();
                i += 1; // closing brace
                toks.push((Tok::Block(inner), open));
                at_cmd_start = false;
            }
            '/' if at_cmd_start => {
                // An xpath location path at command position (`/a/b`, `//tag`,
                // `//a/@href`). Regex literals only ever appear as ARGS, so a
                // leading `/` here is never a regex — lex the whole path as one
                // atom so `xpath::translate` receives an intact token.
                //
                // A `[…]` PREDICATE is part of that atom, quotes and spaces
                // included. XPath accepts either quote for an attribute value, so
                // `//div[@class="card"]` is as legal as `[@class='card']` — but a
                // bare stop at `"` split it into `//div[@class=`, a `Str` arg whose
                // quotes the parser drops, and `]`, which rejoined as
                // `[@class= card ]` and was rejected as "must be quoted". That is
                // the same lossy verb+args reconstruction the jq branch below
                // documents, so it gets the same fix: track quote state and `[`
                // depth, and only treat a delimiter as one at depth 0.
                let start = i;
                let mut depth = 0i32;
                let mut in_str: Option<char> = None;
                while i < n {
                    let c = cs[i];
                    match in_str {
                        Some(q) if c == q => in_str = None,
                        Some(_) => {}
                        None => match c {
                            '\'' | '"' if depth > 0 => in_str = Some(c),
                            '[' => depth += 1,
                            ']' => depth -= 1,
                            ' ' | '\t' | '\r' | '\n' | ';' | '{' | '"' if depth <= 0 => break,
                            _ => {}
                        },
                    }
                    i += 1;
                }
                toks.push((Tok::Word(cs[start..i].iter().collect()), start));
                at_cmd_start = false;
            }
            '/' => {
                // A regex literal `/.../` — reads to the closing unescaped `/`,
                // spanning quotes and spaces (unlike a bare word), so patterns
                // like `/" (4|5)\d\d /` lex as a single token. `\/` is an escaped
                // slash inside the pattern, not the terminator. If no closing `/`
                // appears before the line ends, it falls back to a bare word.
                let start = i;
                let mut j = i + 1;
                let mut closed = false;
                while j < n {
                    match cs[j] {
                        '\\' if j + 1 < n => j += 2,
                        '/' => {
                            j += 1;
                            closed = true;
                            break;
                        }
                        '\n' => break,
                        _ => j += 1,
                    }
                }
                if closed {
                    let w: String = cs[start..j].iter().collect();
                    toks.push((Tok::Word(w), start));
                    i = j;
                } else {
                    while i < n && !matches!(cs[i], ' ' | '\t' | '\r' | '\n' | ';' | '{' | '"') {
                        i += 1;
                    }
                    let w: String = cs[start..i].iter().collect();
                    toks.push((Tok::Word(w), start));
                }
                at_cmd_start = false;
            }
            _ if jq_ok && at_cmd_start && jq_literal_at(&cs, i) => {
                // A jq literal occupies a whole body command (`.a | select(.k ==
                // "v")`). Lex it as ONE verbatim atom so `jq::translate` receives
                // the exact source text. Splitting it into verb + args and
                // space-joining them back (what the parser would otherwise hand
                // downstream) is lossy twice over: `Arg::Str` drops its quotes, so
                // `select(.k == "v")` reconstructs as `select(.k == v )` and
                // silently compares against a bareword; and the rejoin injects
                // spaces, so `.["k"]` becomes `.[ "k" ]` and no longer parses as a
                // path at all. Both are corruptions of a documented construct.
                //
                // The scan tracks `"` strings and `(`/`[` depth so a `;` inside
                // either does not end the command, and stops at a brace so a
                // block still lexes as a block.
                let start = i;
                let mut depth = 0i32;
                let mut in_str = false;
                while i < n {
                    match cs[i] {
                        '"' => in_str = !in_str,
                        '(' | '[' if !in_str => depth += 1,
                        ')' | ']' if !in_str => depth -= 1,
                        ';' | '\n' | '{' | '}' if !in_str && depth == 0 => break,
                        _ => {}
                    }
                    i += 1;
                }
                let w: String = cs[start..i].iter().collect();
                toks.push((Tok::Word(w.trim_end().to_string()), start));
                at_cmd_start = false;
            }
            _ => {
                let start = i;
                while i < n && !matches!(cs[i], ' ' | '\t' | '\r' | '\n' | ';' | '{' | '"') {
                    i += 1;
                }
                let w: String = cs[start..i].iter().collect();
                toks.push((Tok::Word(w), start));
                at_cmd_start = false;
            }
        }
    }
    Ok(toks)
}

/// Does a jq literal begin at `cs[i]`, which is at command position inside a
/// body block? A leading `.` is unambiguous there — a body holds query verbs, and
/// the widget-path commands that also start with `.` are top-level only, which is
/// why the caller gates this on `jq_ok`. `select(`, `map(` and `has(` are the jq
/// call forms the spec documents; the CALL spelling is what distinguishes them,
/// so arb's native space-separated `map EXPR` and `has KEY` still lex as verbs.
fn jq_literal_at(cs: &[char], i: usize) -> bool {
    if cs[i] == '.' {
        return true;
    }
    let mut j = i;
    while j < cs.len() && (cs[j].is_ascii_alphanumeric() || cs[j] == '_') {
        j += 1;
    }
    if cs.get(j) != Some(&'(') {
        return false;
    }
    let name: String = cs[i..j].iter().collect();
    matches!(name.as_str(), "select" | "map" | "has")
}
