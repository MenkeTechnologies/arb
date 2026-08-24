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
            // A jq literal occupies a whole body command (`.a | select(.k ==
            // "v")`, `reduce .[] as $x (0; . + $x)`, `{k: .v}`). Lex it as ONE
            // verbatim atom so the jq front-end receives the exact source text.
            // Splitting it into verb + args and space-joining them back (what
            // the parser would otherwise hand downstream) is lossy three times
            // over: `Arg::Str` drops its quotes, so `select(.k == "v")`
            // reconstructs as `select(.k == v )` and silently compares against a
            // bareword; the rejoin injects spaces, so `.["k"]` becomes `.[ "k" ]`
            // and no longer parses as a path; and a `;` — jq's ARGUMENT
            // separator — is an arb command terminator, so `setpath(["a"];9)`
            // would be cut in half.
            //
            // The scan tracks `"` strings (honouring `\"`) and `(`/`[`/`{` depth
            // so a `;` or a brace inside any of them does not end the command. A
            // `}` at depth 0 is the ENCLOSING arb block closing, and ends it.
            //
            // This arm sits ahead of the `"` and `{` arms deliberately: a jq
            // program may BEGIN with either (`"\(.a)"`, `{k: .v}`), and those
            // arms would otherwise take the opener first.
            _ if jq_ok && at_cmd_start && jq_literal_at(&cs, i) => {
                let start = i;
                let mut depth = 0i32;
                let mut in_str = false;
                // `def NAME: BODY;` ends its definition with a `;` at depth 0 —
                // the same character arb uses to end a command. A jq program that
                // OPENS with `def` therefore runs to end of line (or to the
                // block's `}`), which is the only reading under which
                // `def f: . * 2; map(f)` is one program rather than two commands.
                let def_led = cs[i..].starts_with(&['d', 'e', 'f'])
                    && cs.get(i + 3).is_some_and(|c| matches!(c, ' ' | '\t'));
                while i < n {
                    match cs[i] {
                        '\\' if in_str => i += 1,
                        '"' => in_str = !in_str,
                        '(' | '[' | '{' if !in_str => depth += 1,
                        ')' | ']' if !in_str => depth -= 1,
                        '}' if !in_str => {
                            if depth == 0 {
                                break;
                            }
                            depth -= 1;
                        }
                        ';' if !in_str && depth == 0 && !def_led => break,
                        '\n' if !in_str && depth == 0 => break,
                        _ => {}
                    }
                    i += 1;
                }
                let w: String = cs[start..i].iter().collect();
                toks.push((Tok::Word(w.trim_end().to_string()), start));
                at_cmd_start = false;
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

/// jq's KEYWORDS. None of them is an arb verb, and each one starts a jq
/// expression that runs over several whitespace-separated tokens, so the whole
/// command has to be taken verbatim for the jq front-end to see it at all.
const JQ_KEYWORDS: &[&str] = &["reduce", "foreach", "if", "try", "label", "def"];

/// jq builtins in their CALL spelling (`name(`). The call form is what
/// distinguishes them from arb's own verbs, which are space-separated
/// (`split DELIM`, `join SEP`, `del KEY`, `first`) — SPEC §8's context rule.
/// Every one of these was an `unknown verb` error before the jq engine existed,
/// so claiming them takes nothing away.
///
/// `where(` is deliberately ABSENT: it is arb's own predicate verb.
const JQ_CALLS: &[&str] = &[
    "select", "map", "map_values", "has", "with_entries", "group_by", "unique_by", "sort_by",
    "min_by", "max_by", "any", "all", "range", "limit", "first", "last", "nth", "until",
    "while", "repeat", "recurse", "paths", "del", "path", "getpath", "setpath", "delpaths",
    "splits", "split", "join", "ltrimstr", "rtrimstr", "startswith", "endswith", "test",
    "match", "capture", "scan", "sub", "gsub", "error", "add", "flatten", "contains",
    "inside", "index", "rindex", "indices", "walk", "combinations", "tostream", "fromstream",
    "truncate_stream", "IN", "INDEX", "isempty", "pick", "todate", "strftime", "strptime",
    "ascii", "implode", "debug", "halt_error", "pow", "atan2", "ltrim", "isvalid", "env",
    "not", "abs", "toarray", "tojson", "getpath", "objects", "arrays", "values",
];

/// jq builtins in their BARE spelling that arb has no verb for. A bare word is
/// arb's native verb wherever the two share a spelling (SPEC §8's context rule),
/// so this list is exactly the jq names arb does NOT define — every one of them
/// was an `unknown verb` error before the jq engine existed.
///
/// Being here matters only for a MULTI-token command: it makes the whole line one
/// verbatim atom, so `env | has("PATH")` keeps its quotes. A one-word command
/// reaches the jq engine either way.
const JQ_BARE: &[&str] = &[
    "env", "tostring", "tonumber", "tojson", "fromjson", "type", "keys_unsorted",
    "to_entries", "from_entries", "values", "paths", "leaf_paths", "any", "all", "unique",
    "not", "empty", "error", "input", "inputs", "ascii_downcase", "ascii_upcase", "explode",
    "implode", "transpose", "combinations", "tostream", "todate", "fromdate", "todateiso8601",
    "fromdateiso8601", "now", "infinite", "nan", "isnan", "isinfinite", "isnormal", "halt",
    "halt_error", "arrays", "objects", "booleans", "numbers", "strings", "nulls", "iterables",
    "scalars", "recurse", "toarray", "utf8bytelength", "input_line_number", "builtins",
    "stderr", "debug", "mktime", "gmtime", "localtime", "reverse", "date", "finites",
    "normals", "ltrim", "rtrim", "sqrt", "log", "log2", "log10", "exp", "exp2", "exp10",
    "cbrt", "trunc", "nearbyint", "fabs", "significand", "logb", "sin", "cos", "tan",
    "asin", "acos", "atan", "sinh", "cosh", "tanh",
];

/// jq's `@format` strings. A leading `@` is otherwise an XPATH attribute step,
/// so only these names are claimed for jq — `@href` still selects an attribute.
const JQ_FORMATS: &[&str] = &[
    "base64", "base64d", "csv", "tsv", "json", "text", "html", "uri", "sh",
];

/// Does a jq literal begin at `cs[i]`, which is at command position inside a
/// body block?
///
/// A leading `.` is unambiguous there — a body holds query verbs, and the
/// widget-path commands that also start with `.` are top-level only, which is
/// why the caller gates this on `jq_ok`. The bracket/brace/paren/`$`/`"`
/// openers are jq's array, object, grouping, variable and string forms; none of
/// them can begin an arb command (a leading `{` is `command cannot start with a
/// block`, the rest are `unknown verb`), so none is taken from arb.
///
/// A bare alphanumeric word is still arb's NATIVE verb, per SPEC §8 — only the
/// jq keywords, the `name(` call spelling and the `@format` names are claimed.
fn jq_literal_at(cs: &[char], i: usize) -> bool {
    if matches!(cs[i], '.' | '[' | '(' | '$' | '"') {
        return true;
    }
    // A `{` is jq's object construction ONLY when what follows looks like an
    // object KEY. Without that test a nested arb block (`bind C-a { { … } }`)
    // would be swallowed whole, and the parser's block-depth guard — the thing
    // that stops a pathological spec from overflowing the stack — would never
    // see a block at all.
    if cs[i] == '{' {
        return jq_object_at(cs, i);
    }
    // A jq program may open with a NUMBER literal (`0 | todate`). No arb verb
    // starts with a digit, so nothing is taken from arb here either.
    if cs[i].is_ascii_digit() {
        return true;
    }
    let mut j = i;
    if cs[i] == '@' {
        j += 1;
        let start = j;
        while j < cs.len() && (cs[j].is_ascii_alphanumeric() || cs[j] == '_') {
            j += 1;
        }
        let name: String = cs[start..j].iter().collect();
        return JQ_FORMATS.contains(&name.as_str());
    }
    while j < cs.len() && (cs[j].is_ascii_alphanumeric() || cs[j] == '_') {
        j += 1;
    }
    let name: String = cs[i..j].iter().collect();
    if JQ_KEYWORDS.contains(&name.as_str()) {
        // `if`/`try`/... only start a jq expression when a token follows them on
        // the same line; a bare word that happens to spell one is left alone.
        return cs.get(j).is_some_and(|c| matches!(c, ' ' | '\t' | '(' | '.' | '$' | '"'));
    }
    if JQ_BARE.contains(&name.as_str()) {
        // Only when the word ENDS here: `envelope` is not `env`.
        return !cs.get(j).is_some_and(|c| c.is_alphanumeric() || *c == '_');
    }
    cs.get(j) == Some(&'(') && JQ_CALLS.contains(&name.as_str())
}

/// Does `cs[i] == '{'` open a jq OBJECT construction rather than an arb block?
///
/// The test is the first token inside: an object is `{}`, or `{key…` where the
/// key is an identifier, a `"string"`, a `$var`, a `(expr)` or an `@format`. An
/// arb block opens with a command, and the only commands that could be confused
/// with a key are followed by neither `:` nor `,` nor `}`.
fn jq_object_at(cs: &[char], i: usize) -> bool {
    let mut j = i + 1;
    while j < cs.len() && matches!(cs[j], ' ' | '\t' | '\r' | '\n') {
        j += 1;
    }
    match cs.get(j) {
        None => false,
        Some('}') => true,
        Some('"' | '$' | '(' | '@') => true,
        Some(c) if c.is_alphabetic() || *c == '_' => {
            while j < cs.len() && (cs[j].is_alphanumeric() || cs[j] == '_') {
                j += 1;
            }
            while j < cs.len() && matches!(cs[j], ' ' | '\t') {
                j += 1;
            }
            matches!(cs.get(j), Some(':' | ',' | '}'))
        }
        _ => false,
    }
}
