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
pub fn lex(src: &str) -> Result<Vec<(Tok, usize)>, crate::err::SpecError> {
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
                // atom (up to the normal word delimiters) so `xpath::translate`
                // receives an intact token. A stray space ends it, so an
                // out-of-subset spaced predicate splits and errors downstream.
                let start = i;
                while i < n && !matches!(cs[i], ' ' | '\t' | '\r' | '\n' | ';' | '{' | '"') {
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
