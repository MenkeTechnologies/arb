//! Language Server (`arb --lsp`): a std-only, no-async LSP over stdio JSON-RPC
//! for `.arb` specs. It parses + builds each open document and republishes the
//! parser/interpreter error as a diagnostic, plus `documentSymbol`, `hover`,
//! `completion`, `signatureHelp`, `definition`, `references`, `documentHighlight`,
//! `rename`, `foldingRange`, `formatting`, and `semanticTokens/full`. The
//! transport shape is the standard `Content-Length` framing; the language logic
//! (`diagnose`/`handle` + the pure helpers) is unit-testable.
//!
//! Diagnostics anchor to the error's real source span (the offending verb token
//! or lexer position). Position columns are LSP UTF-16 code units (see
//! `col_utf16`/`line_len_utf16`); spans are per-command (the verb), not
//! per-argument.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

/// A single diagnostic: a message and the (line, col..col) range it covers.
#[derive(Debug, Clone, PartialEq)]
pub struct Diag {
    pub message: String,
    pub line: u32,
    pub start_col: u32,
    pub end_col: u32,
}

/// Parse + build `src`; map the first error (if any) to a diagnostic, anchored
/// to its source span when the error carries one (else the first line). Empty
/// when the spec is valid.
pub fn diagnose(src: &str) -> Vec<Diag> {
    let err = match crate::parser::parse(src) {
        Ok(cmds) => match crate::spec::build(&cmds) {
            Ok(_) => return Vec::new(),
            Err(e) => e,
        },
        Err(e) => e,
    };
    let diag = match err.span {
        Some((start, end)) => {
            let (line, start_col) = offset_to_line_col(src, start);
            // Clamp the end to the same line so the range stays on one line.
            let (eline, ecol) = offset_to_line_col(src, end);
            let end_col = if eline == line { ecol } else { start_col + 1 };
            Diag {
                message: err.msg,
                line,
                start_col,
                end_col: end_col.max(start_col + 1),
            }
        }
        None => {
            let ec = src
                .lines()
                .next()
                .map(|l| l.chars().count() as u32)
                .unwrap_or(0);
            Diag {
                message: err.msg,
                line: 0,
                start_col: 0,
                end_col: ec.max(1),
            }
        }
    };
    vec![diag]
}

/// Map a char offset to (0-based line, 0-based column). Columns are LSP UTF-16
/// code units (the protocol's required unit), so astral chars (emoji, non-BMP
/// CJK) in a preceding string literal don't shift every downstream column.
fn offset_to_line_col(src: &str, off: usize) -> (u32, u32) {
    let (mut line, mut col) = (0u32, 0u32);
    for (i, ch) in src.chars().enumerate() {
        if i >= off {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += ch.len_utf16() as u32;
        }
    }
    (line, col)
}

/// UTF-16 length of a whole line (replaces `.chars().count()` at range ends so
/// symbol/diagnostic end columns stay LSP-conformant). BMP chars = 1 unit,
/// astral = 2.
fn line_len_utf16(line: &str) -> u32 {
    line.chars().map(|c| c.len_utf16() as u32).sum()
}

/// Convert an LSP UTF-16 code-unit column into a char index in `chars`. LSP
/// `position.character` counts UTF-16 units, so a non-BMP char (2 units but one
/// `char`) shifts every later token; indexing the char vector by the raw column
/// lands on the wrong token. Walk until the accumulated UTF-16 length reaches col.
fn char_idx_from_utf16(chars: &[char], col: u32) -> usize {
    let mut units = 0u32;
    for (i, c) in chars.iter().enumerate() {
        if units >= col {
            return i;
        }
        units += c.len_utf16() as u32;
    }
    chars.len()
}

/// The server's open-document store, keyed by URI.
#[derive(Default)]
pub struct Server {
    docs: HashMap<String, String>,
}

/// Handle one incoming message, returning the replies + notifications to send
/// (pure: no IO, so this is integration-testable). Requests echo their `id`.
pub fn handle(server: &mut Server, msg: &Value) -> Vec<Value> {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let id = msg.get("id").cloned();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let reply = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result });

    match method {
        "initialize" => vec![reply(json!({
            "capabilities": {
                "textDocumentSync": 1,          // Full sync
                "documentSymbolProvider": true,
                "hoverProvider": true,
                "completionProvider": { "resolveProvider": false, "triggerCharacters": [".", "-"] },
                "foldingRangeProvider": true,
                "documentHighlightProvider": true,
                "referencesProvider": true,
                "definitionProvider": true,
                "signatureHelpProvider": { "triggerCharacters": [" "] },
                "renameProvider": { "prepareProvider": true },
                "documentFormattingProvider": true,
                "semanticTokensProvider": {
                    "legend": { "tokenTypes": TOKEN_LEGEND, "tokenModifiers": [] },
                    "full": true,
                },
            },
            "serverInfo": { "name": "arb", "version": env!("CARGO_PKG_VERSION") },
        }))],
        "initialized" | "exit" => vec![],
        "shutdown" => vec![reply(Value::Null)],
        "textDocument/didOpen" => {
            let uri = params["textDocument"]["uri"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let text = params["textDocument"]["text"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let notif = publish_diagnostics(&uri, &text);
            server.docs.insert(uri, text);
            vec![notif]
        }
        "textDocument/didChange" => {
            let uri = params["textDocument"]["uri"]
                .as_str()
                .unwrap_or("")
                .to_string();
            // Full sync: the last content change carries the whole document.
            let text = params["contentChanges"]
                .as_array()
                .and_then(|a| a.last())
                .and_then(|c| c["text"].as_str())
                .unwrap_or("")
                .to_string();
            let notif = publish_diagnostics(&uri, &text);
            server.docs.insert(uri, text);
            vec![notif]
        }
        "textDocument/didClose" => {
            let uri = params["textDocument"]["uri"]
                .as_str()
                .unwrap_or("")
                .to_string();
            server.docs.remove(&uri);
            // Clear squiggles by publishing an empty set.
            vec![json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": { "uri": uri, "diagnostics": [] },
            })]
        }
        "textDocument/documentSymbol" => {
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
            let src = server.docs.get(uri).cloned().unwrap_or_default();
            vec![reply(json!(document_symbols(&src)))]
        }
        "textDocument/hover" => {
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
            let src = server.docs.get(uri).cloned().unwrap_or_default();
            let line = params["position"]["line"].as_u64().unwrap_or(0) as u32;
            let ch = params["position"]["character"].as_u64().unwrap_or(0) as u32;
            match hover(&src, line, ch) {
                Some(md) => vec![reply(
                    json!({ "contents": { "kind": "markdown", "value": md } }),
                )],
                None => vec![reply(Value::Null)],
            }
        }
        "textDocument/completion" => {
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
            let src = server.docs.get(uri).cloned().unwrap_or_default();
            let line = params["position"]["line"].as_u64().unwrap_or(0) as u32;
            let ch = params["position"]["character"].as_u64().unwrap_or(0) as u32;
            vec![reply(json!(completions(&src, line, ch)))]
        }
        "textDocument/foldingRange" => {
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
            let src = server.docs.get(uri).cloned().unwrap_or_default();
            vec![reply(json!(folding_ranges(&src)))]
        }
        "textDocument/documentHighlight" => {
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
            let src = server.docs.get(uri).cloned().unwrap_or_default();
            let line = params["position"]["line"].as_u64().unwrap_or(0) as u32;
            let ch = params["position"]["character"].as_u64().unwrap_or(0) as u32;
            let hls: Vec<Value> = match word_at(&src, line, ch) {
                Some(w) => word_occurrences(&src, &w)
                    .into_iter()
                    .map(|r| json!({ "range": r, "kind": 1 }))
                    .collect(),
                None => Vec::new(),
            };
            vec![reply(json!(hls))]
        }
        "textDocument/references" => {
            let uri = params["textDocument"]["uri"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let src = server.docs.get(&uri).cloned().unwrap_or_default();
            let line = params["position"]["line"].as_u64().unwrap_or(0) as u32;
            let ch = params["position"]["character"].as_u64().unwrap_or(0) as u32;
            let locs: Vec<Value> = match word_at(&src, line, ch) {
                Some(w) => word_occurrences(&src, &w)
                    .into_iter()
                    .map(|r| json!({ "uri": uri, "range": r }))
                    .collect(),
                None => Vec::new(),
            };
            vec![reply(json!(locs))]
        }
        "textDocument/definition" => {
            let uri = params["textDocument"]["uri"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let src = server.docs.get(&uri).cloned().unwrap_or_default();
            let line = params["position"]["line"].as_u64().unwrap_or(0) as u32;
            let ch = params["position"]["character"].as_u64().unwrap_or(0) as u32;
            match word_at(&src, line, ch).and_then(|w| widget_decl_location(&src, &uri, &w)) {
                Some(loc) => vec![reply(loc)],
                None => vec![reply(Value::Null)],
            }
        }
        "textDocument/signatureHelp" => {
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
            let src = server.docs.get(uri).cloned().unwrap_or_default();
            let line = params["position"]["line"].as_u64().unwrap_or(0) as u32;
            match signature_help(&src, line) {
                Some(v) => vec![reply(v)],
                None => vec![reply(Value::Null)],
            }
        }
        "textDocument/prepareRename" => {
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
            let src = server.docs.get(uri).cloned().unwrap_or_default();
            let line = params["position"]["line"].as_u64().unwrap_or(0) as u32;
            let ch = params["position"]["character"].as_u64().unwrap_or(0) as u32;
            // Only widget `.path` names are safely renameable (verbs are keywords).
            match word_at(&src, line, ch).filter(|w| w.starts_with('.')) {
                Some(w) => match word_occurrences(&src, &w)
                    .into_iter()
                    .find(|r| r["start"]["line"].as_u64() == Some(line as u64))
                {
                    Some(r) => vec![reply(r)],
                    None => vec![reply(Value::Null)],
                },
                None => vec![reply(Value::Null)],
            }
        }
        "textDocument/rename" => {
            let uri = params["textDocument"]["uri"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let src = server.docs.get(&uri).cloned().unwrap_or_default();
            let line = params["position"]["line"].as_u64().unwrap_or(0) as u32;
            let ch = params["position"]["character"].as_u64().unwrap_or(0) as u32;
            let new_name = params["newName"].as_str().unwrap_or("").to_string();
            let edits: Vec<Value> = match word_at(&src, line, ch).filter(|w| w.starts_with('.')) {
                Some(w) => word_occurrences(&src, &w)
                    .into_iter()
                    .map(|r| json!({ "range": r, "newText": new_name }))
                    .collect(),
                None => Vec::new(),
            };
            if edits.is_empty() {
                vec![reply(Value::Null)]
            } else {
                vec![reply(json!({ "changes": { uri: edits } }))]
            }
        }
        "textDocument/formatting" => {
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
            let src = server.docs.get(uri).cloned().unwrap_or_default();
            let end_line = src.lines().count() as u32;
            vec![reply(json!([{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": end_line, "character": 0 },
                },
                "newText": format_arb(&src),
            }]))]
        }
        "textDocument/semanticTokens/full" => {
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
            let src = server.docs.get(uri).cloned().unwrap_or_default();
            vec![reply(json!({ "data": semantic_tokens(&src) }))]
        }
        // Unknown request (has an id) -> MethodNotFound; notification -> ignore.
        _ => {
            if id.is_some() {
                vec![json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {method}") },
                })]
            } else {
                vec![]
            }
        }
    }
}

/// A `textDocument/publishDiagnostics` notification for `src`.
fn publish_diagnostics(uri: &str, src: &str) -> Value {
    let diags: Vec<Value> = diagnose(src).iter().map(diag_to_json).collect();
    json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": { "uri": uri, "diagnostics": diags },
    })
}

fn diag_to_json(d: &Diag) -> Value {
    json!({
        "range": {
            "start": { "line": d.line, "character": d.start_col },
            "end": { "line": d.line, "character": d.end_col },
        },
        "severity": 1, // Error
        "source": "arb",
        "message": d.message,
    })
}

/// Line-scan for widget declarations (`<verb> .path …`) -> LSP document symbols.
fn document_symbols(src: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let mut it = line.split_whitespace();
        let (Some(verb), Some(path)) = (it.next(), it.next()) else {
            continue;
        };
        if crate::spec::WidgetKind::from(verb).is_some() && path.starts_with('.') {
            let len = line_len_utf16(line);
            out.push(json!({
                "name": path,
                "detail": verb,
                "kind": 8, // SymbolKind.Field
                "range": symbol_range(i as u32, len),
                "selectionRange": symbol_range(i as u32, len),
            }));
        }
    }
    out
}

fn symbol_range(line: u32, len: u32) -> Value {
    json!({
        "start": { "line": line, "character": 0 },
        "end": { "line": line, "character": len },
    })
}

/// Help text for the verb under the cursor, as markdown, or `None`.
fn hover(src: &str, line: u32, ch: u32) -> Option<String> {
    let l = src.lines().nth(line as usize)?;
    let chars: Vec<char> = l.chars().collect();
    let ci = char_idx_from_utf16(&chars, ch);
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut start = ci;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = ci;
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    let word: String = chars[start..end].iter().collect();
    verb_help(&word).map(|h| format!("**`{word}`** — {h}"))
}

/// The arb language corpus: (name, chapter, one-line doc). Single source of
/// truth for the `hover` path here and the offline `gen-docs` reference
/// generator, so the served reference never drifts from what the runtime and
/// editor tooling actually know. Every entry mirrors a real construct:
/// widgets come from `spec::WidgetKind::from`, query verbs from the
/// `build_query` verb table, directives/actions from the spec parser, and
/// input modes from the pipeline `in` markers.
const CORPUS: &[(&str, &str, &str, &str)] = &[
    // ── Input ──
    ("in", "Input", "Begin the pipeline, reading the raw stdin stream line by line.", "source .log { in }"),
    ("in.json", "Input", "Begin the pipeline reading stdin (JSON-lines intent; a marker, same as in).", "source .s { in.json; field status; tally }"),
    ("in.html", "Input", "Begin the pipeline reading stdin as HTML (marker, same as in; use with sel/find).", "source .s { in.html; find a; attr href }"),
    ("in.xml", "Input", "Begin the pipeline reading stdin as XML (marker, same as in).", "source .s { in.xml; find item; text }"),
    ("in.logfmt", "Input", "Begin the pipeline reading stdin as logfmt (marker, same as in).", "source .s { in.logfmt; field level; tally }"),
    ("in.csv", "Input", "Begin the pipeline treating stdin as CSV: the header row keys each data row into a JSON object (field NAME works).", "source .rows { in.csv; field name; tally }"),
    ("in.tsv", "Input", "Begin the pipeline treating stdin as TSV: a tab-separated header keys each data row into a JSON object (field NAME works).", "source .rows { in.tsv; field name; tally }"),
    ("in.yaml", "Input", "Begin the pipeline parsing stdin as YAML (--- multi-doc), emitting each document as a JSON line.", "source .s { in.yaml; field status; tally }"),
    ("in.yml", "Input", "Alias of in.yaml: parse stdin as YAML into JSON lines.", "source .s { in.yml; field status; tally }"),
    ("in.toml", "Input", "Begin the pipeline parsing stdin as one TOML document, emitted as a JSON object line.", "source .cfg { in.toml; field version }"),
    // ── Query ──
    ("find", "Query", "Parse stream as HTML, emit the outer HTML of each element matching the selector.", "in.html; find a; attr href"),
    ("attr", "Query", "From element fragments, emit each element's named attribute; drop those lacking it.", "in.html; find a; attr href"),
    ("text", "Query", "From element fragments, emit each element's inner text; non-elements pass through.", "in.html; find h2; text"),
    ("match", "Query", "Keep lines matching the regex.", "in; match /ERROR/"),
    ("grep", "Query", "Keep lines matching the regex.", "in; grep /ERROR/"),
    ("reject", "Query", "Drop lines matching the regex.", "in; reject /^#/"),
    ("grepv", "Query", "Drop lines matching the regex.", "in; grepv /^#/"),
    ("field", "Query", "Replace each line with a selected field (whitespace column or JSON key path).", "in.json; field status; tally"),
    ("fields", "Query", "Project multiple 1-based whitespace columns, space-joined, in the given order.", "in; fields 1 3"),
    ("each", "Query", "Flatten JSON-array lines into one line per element; non-array lines pass through.", "in.json; field users; each; field name"),
    ("count", "Query", "Reduce to the current line count (scalar).", "in; count"),
    ("rate", "Query", "Reduce to lines-per-second over the elapsed window (scalar).", "in; rate"),
    ("tally", "Query", "Group identical lines and count them, sorted by count desc then key asc.", "in; field status; tally"),
    ("sum", "Query", "Sum of lines parsed as numbers; non-numeric lines are ignored (scalar).", "in; field 3; sum"),
    ("min", "Query", "Minimum of numeric lines, or 0 if none (scalar).", "in; field 2; min"),
    ("max", "Query", "Maximum of numeric lines, or 0 if none (scalar).", "in; field 2; max"),
    ("avg", "Query", "Mean of numeric lines, or 0 if none (scalar).", "in; field 2; avg"),
    ("keys", "Query", "Flatten a JSON object's keys into one line each; non-object lines pass through.", "in.json; keys; tally"),
    ("vals", "Query", "Flatten a JSON object's values into one line each; non-object lines pass through.", "in.json; vals"),
    ("pick", "Query", "Project a JSON object to the named keys, keeping order; missing keys dropped.", "in.json; pick ts msg"),
    ("sort", "Query", "Sort the lines; -n sorts numerically, -r reverses.", "in; tally; sort -n -r"),
    ("uniq", "Query", "Drop duplicate lines globally, keeping the first occurrence.", "in; sort; uniq"),
    ("rev", "Query", "Reverse the order of the lines.", "in; rev"),
    ("first", "Query", "Keep only the first line.", "in; first"),
    ("last", "Query", "Keep only the last line.", "in; last"),
    ("upper", "Query", "Uppercase each line.", "in; upper"),
    ("lower", "Query", "Lowercase each line.", "in; lower"),
    ("trim", "Query", "Strip leading and trailing whitespace from each line.", "in; trim"),
    ("replace", "Query", "Regex replace-all per line (replace /RE/ TO); TO may use $1 captures.", "in; replace /foo/ bar"),
    ("join", "Query", "Collapse all lines into one, joined by a separator (default a space).", "in; field 1; join ,"),
    ("nth", "Query", "Keep only the Nth line (1-based).", "in; nth 3"),
    ("take", "Query", "Keep the first N lines.", "in; tally; take 10"),
    ("drop", "Query", "Drop the first N lines.", "in; drop 1"),
    ("calc", "Query", "Reduce to a scalar from an arithmetic expression over the line count (x).", "in; count; calc x * 100"),
    ("where", "Query", "Keep lines whose numeric value / field predicate holds (fusevm or Rust eval).", "in.json; where lat < 5"),
    ("map", "Query", "Replace each line with an expression's value (field-aware; x = line-as-number).", "in; map x * 2"),
    ("via", "Query", "Fan the stream across a supervised pool of actor NAME in parallel (via NAME * N; reply is the output line).", "in; via sq * 8"),
    ("sort_by", "Query", "Stable-sort JSON records by FIELD (numeric if all parse, else lexicographic).", "in.json; sort_by ts"),
    ("unique_by", "Query", "Keep the first record per distinct FIELD value, preserving input order.", "in.json; unique_by id"),
    ("count_by", "Query", "Group JSON records by FIELD and count; value->count sorted by count desc.", "in.json; count_by level"),
    ("group_by", "Query", "Group lines by FIELD into one JSON-array line per value, keys ascending.", "in.json; group_by level"),
    ("min_by", "Query", "Emit the single record whose numeric FIELD is smallest.", "in.json; min_by lat"),
    ("max_by", "Query", "Emit the single record whose numeric FIELD is largest.", "in.json; max_by lat"),
    ("has", "Query", "Keep only JSON-object lines that contain KEY; all other lines are dropped.", "in.json; has error"),
    ("entries", "Query", "Expand each JSON object into one key/value object line per key (jq to_entries).", "in.json; entries"),
    ("flatten", "Query", "Emit each element of JSON-array lines, one level deeper than each.", "in.json; flatten"),
    ("add", "Query", "Reduce a JSON array line to one value (sum numbers, else concat their strings).", "in.json; add"),
    ("over", "Query", "Keep lines parsing as a number strictly greater than N; drop non-numeric.", "in; field 2; over 100"),
    ("under", "Query", "Keep numeric lines strictly less than N; drop non-numeric lines.", "in; field 2; under 10"),
    ("between", "Query", "Keep numeric lines x where lo <= x <= hi inclusive; drop non-numeric lines.", "in; between 1 10"),
    ("enumerate", "Query", "Prefix each line with its 1-based index and a tab.", "in; enumerate"),
    ("words", "Query", "Split each line on whitespace and emit one word per line.", "in; words; tally"),
    ("dedup", "Query", "Collapse runs of adjacent identical lines to one (classic uniq).", "in; dedup"),
    ("tailn", "Query", "Keep only the last N lines.", "in; tailn 5"),
    ("pad", "Query", "Right-pad each line with spaces to a minimum visible width N (no truncation).", "in; pad 10"),
    ("lpad", "Query", "Left-pad each line with spaces to a minimum width N.", "in; lpad 8"),
    ("grepf", "Query", "Keep lines whose FIELD (JSON key or 1-based column) matches the regex.", "in.json; grepf status /5\\d\\d/"),
    ("apply", "Query", "Placeholder replaced at render by the pipeline typed into input .name; else no-op.", "source .out { in; apply .q }"),
    ("basename", "Query", "Path basename: the part after the last / (the whole line if none).", "in; basename"),
    ("dirname", "Query", "Path dirname: the part before the last / (. if none).", "in; dirname"),
    ("commafy", "Query", "Group a numeric line's integer part with thousands separators; else pass through.", "in; sum; commafy"),
    ("bytes", "Query", "Humanize a byte count (1024-based): 1536 -> 1.5 KB; non-numeric passes through.", "in; field 2; bytes"),
    ("duration", "Query", "Humanize seconds: 3661 -> 1h 1m; non-numeric lines pass through.", "in; field 2; duration"),
    ("flip", "Query", "Reverse the Unicode characters of each line.", "in; flip"),
    ("b64", "Query", "Base64-encode each line.", "in; b64"),
    ("b64d", "Query", "Base64-decode each line; invalid or non-UTF8 lines pass through unchanged.", "in; b64d"),
    ("hex", "Query", "Lowercase hex-encode each line, two hex digits per UTF-8 byte.", "in; hex"),
    ("unhex", "Query", "Decode a hex string to UTF-8; on any error the line passes through unchanged.", "in; unhex"),
    ("urlenc", "Query", "Percent-encode each line, escaping every non-alphanumeric byte (RFC 3986).", "in; urlenc"),
    ("urldec", "Query", "Percent-decode each line to UTF-8; invalid UTF-8 passes through unchanged.", "in; urldec"),
    ("extract", "Query", "Emit the first regex match per line (group 1 if any); drop lines with no match.", "in; extract /(\\d+)/"),
    ("split", "Query", "Explode each line by the literal DELIM into multiple lines (one part per line).", "in; split ,"),
    ("substr", "Query", "Character substring [A,B) 0-based, clamped to the line length.", "in; substr 0 8"),
    ("chars", "Query", "Explode each line into one output line per Unicode character.", "in; chars"),
    ("title", "Query", "Title-case each line: capitalize each word's first letter, lowercase the rest.", "in; title"),
    ("repeat", "Query", "Replace each line with its content repeated N times, concatenated.", "in; repeat 3"),
    ("set", "Query", "Set key K to string value V in each JSON object line; non-objects pass through.", "in.json; set env prod"),
    ("del", "Query", "Remove key K from each JSON object line; non-object lines pass through.", "in.json; del debug"),
    ("rename", "Query", "Rename JSON object key OLD to NEW, keeping the value; no-op if OLD absent.", "in.json; rename ts time"),
    ("default", "Query", "Set string key K to V only when K is absent from the JSON object.", "in.json; default env dev"),
    ("merge", "Query", "Reduce all JSON object lines into one object (later keys win); emits one line.", "in.json; merge"),
    ("floor", "Query", "Floor each numeric line to the nearest lower integer; non-numeric passes through.", "in; floor"),
    ("ceil", "Query", "Round each numeric line up to the nearest integer; non-numeric passes through.", "in; ceil"),
    ("round", "Query", "Round each numeric line to the nearest integer.", "in; round"),
    ("abs", "Query", "Absolute value of each numeric line.", "in; delta; abs"),
    ("clamp", "Query", "Clamp each numeric line into the inclusive range [LO, HI]; non-numeric untouched.", "in; clamp 0 100"),
    ("len", "Query", "Replace each line with its character count.", "in; len"),
    ("length", "Query", "jq length: array element count / object key count / string char count / |number| / null 0; non-JSON falls back to char count.", "in.json; length"),
    ("wc", "Query", "Replace each line with its word count.", "in; wc"),
    ("index", "Query", "Keep only the Nth line (1-based); out-of-range yields no lines.", "in; index 3"),
    ("cut", "Query", "Split each line by DELIM and keep the Nth (1-based) field.", "in; cut , 2"),
    ("contains", "Query", "Keep lines containing a literal substring.", "in; contains error"),
    ("starts", "Query", "Keep lines starting with a literal prefix.", "in; starts GET"),
    ("ends", "Query", "Keep lines ending with a literal suffix.", "in; ends .json"),
    ("nonempty", "Query", "Drop empty or whitespace-only lines.", "in; nonempty"),
    ("numeric", "Query", "Keep only lines that parse as a number.", "in; numeric; avg"),
    ("distinct", "Query", "Reduce to the count of distinct lines (scalar).", "in; distinct"),
    ("sample", "Query", "Keep every Nth line (1-based).", "in; sample 10"),
    ("range", "Query", "Max minus min of numeric lines (scalar).", "in; field 2; range"),
    ("bins", "Query", "Bucket numeric lines into N equal-width ranges -> (range, count) pairs.", "in; bins 10"),
    ("cumsum", "Query", "Running cumulative total of the numeric series.", "in; cumsum"),
    ("delta", "Query", "Consecutive differences of the numeric series (n values -> n-1 deltas).", "in; delta"),
    ("ewma", "Query", "Exponentially-weighted moving average, smoothing factor alpha in (0,1]; s0=x0.", "in; ewma 0.3"),
    ("sma", "Query", "Simple moving average over a trailing window of N (length-preserving).", "in; sma 5"),
    ("median", "Query", "Median of numeric lines (scalar).", "in; field 2; median"),
    ("percentile", "Query", "Nth percentile (0-100) of numeric values, linear interpolation between ranks.", "in; percentile 95"),
    ("p50", "Query", "50th percentile (median) of the numeric values (scalar).", "in; field 2; p50"),
    ("p90", "Query", "90th percentile of the numeric values (scalar).", "in; field 2; p90"),
    ("p95", "Query", "95th percentile of the numeric values (scalar).", "in; field 2; p95"),
    ("p99", "Query", "99th percentile of the numeric values, for latency tails (scalar).", "in; field 2; p99"),
    ("stddev", "Query", "Population standard deviation of numeric lines (scalar).", "in; field 2; stddev"),
    ("product", "Query", "Product of numeric lines (scalar).", "in; product"),
    ("append", "Query", "Suffix every line with a literal string.", "in; append !"),
    ("prepend", "Query", "Prefix every line with a literal string.", "in; prepend >>"),
    ("slice", "Query", "Keep lines from index A to B inclusive (1-based).", "in; slice 1 10"),
    ("sel", "Query", "Parse stream as HTML, emit each CSS-selector match's text (or -attr value).", "in.html; sel { div.card h2 }"),
    // ── Widget ──
    ("text", "Widget", "Shows the last stream line, or a scalar/top-pair result, in a bordered pane.", "text .t  ·  source .t { in; last }"),
    ("tail", "Widget", "Scrolling list of the newest lines that fit (tail -f); -limit/-lines caps rows.", "tail .log  ·  source .log { in }"),
    ("list", "Widget", "Scrollable list of stream lines in a bordered pane; -limit caps rows shown.", "list .rows  ·  source .rows { in }"),
    ("gauge", "Widget", "Renders a scalar as a progress bar filled to value/max (-max, default 100).", "gauge .cpu -max 100  ·  source .cpu { in; field 3; sum }"),
    ("bars", "Widget", "Horizontal bar chart of tally/count pairs; -top caps the number of bars (def 20).", "bars .top  ·  source .top { in; field 1; tally }"),
    ("histo", "Widget", "Bar chart of value-distribution pairs; same rendering as bars, -top caps bars.", "histo .h  ·  source .h { in; field 2; tally }"),
    ("spark", "Widget", "Compact braille sparkline of the numeric series, newest points that fit width.", "spark .s  ·  source .s { in; field 2 }"),
    ("chart", "Widget", "Braille line chart of the numeric series with auto-scaled x/y axes.", "chart .c  ·  source .c { in; field 2 }"),
    ("linegauge", "Widget", "Thin one-line progress bar (value/max) for tight cells — a compact gauge.", "linegauge .load -max 8  ·  source .load { in; count }"),
    ("scatter", "Widget", "Braille scatter plot of the numeric series (higher resolution than spark).", "scatter .lat  ·  source .lat { in; field 2 }"),
    ("sparkline", "Widget", "Block-bar sparkline (ratatui Sparkline) of the numeric series; fixed-height bars.", "sparkline .rps  ·  source .rps { in; rate }"),
    ("map", "Widget", "Braille world map plotting `lon lat` points from the stream (-res high|low).", "map .geo  ·  source .geo { in }"),
    ("calendar", "Widget", "Month calendar; days appearing as YYYY-MM-DD in the stream are highlighted.", "calendar .cal  ·  source .cal { in }"),
    ("table", "Widget", "Columnar table of records; -cols sets headers, newest rows that fit are shown.", "table .rows -cols \"a,b,c\"  ·  source .rows { in }"),
    ("tabs", "Widget", "Tab bar from -tabs {a b}; first tab selected (labelled selector, no per-tab body).", "tabs .t -tabs {a b c}"),
    ("block", "Widget", "Bordered container box rendering its bound stream content as a list.", "block .b -title Logs  ·  source .b { in }"),
    ("frame", "Widget", "Framed container rendering its bound stream content as a list.", "frame .f  ·  source .f { in }"),
    ("input", "Widget", "Editable text field; its value is spliced into pipelines via apply .name.", "input .q  ·  out { apply .q }"),
    ("select", "Widget", "Interactive fuzzy-select over the stream; type to filter, Tab marks, Enter emits.", "select .k -prompt pick  ·  source .k { in }"),
    ("sel", "Widget", "In-dashboard selection list over its own source; the highlighted row is published as .<path>.sel.", "sel .ps  ·  source .ps { in }  ·  out { where match(.ps.sel) }"),
    ("filter", "Widget", "Labelled filter control; its value drives where match(.name) / apply.", "filter .q  ·  out { where match(.q) }"),
    ("facet", "Widget", "Multi-select of values (-opts or distinct -field); drives where <field> in .name.", "facet .lv -field level  ·  out { where level in .lv }"),
    ("slider", "Widget", "Numeric control (-min/-max/-step) driving where <field> < .name; arrows adjust.", "slider .th -field lat -max 5  ·  out { where lat < .th }"),
    ("check", "Widget", "Boolean toggle (-label); its value is the string \"1\" or \"0\".", "check .on -label live"),
    // ── Directive ──
    ("import", "Directive", "Inline a module (stdlib preset or user .arb file) into the spec.", "import http"),
    ("import as", "Directive", "Build a module into namespace ALIAS, prefixing its names, then merge it in.", "import gauges as g"),
    ("source", "Directive", "Attach a query pipeline (body must start with in) as a widget's data source.", "source .status { in.json; field status; tally }"),
    ("search", "Directive", "Attach a select widget's fuzzy-match key pipeline; the row still shows source.", "search .k { in; field 2 }"),
    ("bind", "Directive", "Bind a control key to an action (set|quit|beep|alert|flash|exec, or a { } block).", "bind C-q quit"),
    ("expect", "Directive", "Fire an action when a stream line matches /regex/ (event-driven reaction).", "expect /5\\d\\d/ { flash .log red }"),
    ("timeout", "Directive", "Fire an action when the stream is idle for a duration (Ns/Nms/Nm).", "timeout 5s alert \"stream idle\""),
    ("out", "Directive", "Apply a query pipeline to the stream and write it to stdout (modify a pipe).", "out { where match(.q) }"),
    ("grid", "Directive", "Place a widget in the layout grid (-row/-col, -span/-rowspan/-colspan).", "grid .cpu -row 0 -col 0 -span 2"),
    ("rows", "Directive", "Size the grid's row tracks: N fixed cells, N% percentage, or N*/* proportional weight.", "rows \"1 2 1\""),
    ("cols", "Directive", "Size the grid's column tracks: N fixed cells, N% percentage, or N*/* weight.", "cols \"20 * 2*\""),
    ("gap", "Directive", "Blank cells between grid rows/columns (default 0).", "gap 1"),
    ("layout", "Directive", "Auto-tile direction when no widget has a grid cell (horizontal | vertical).", "layout horizontal"),
    ("configure", "Directive", "Merge new -opts into an already-declared widget (build-time; later keys win).", ".cpu configure -max 200"),
    ("actor", "Directive", "Declare a message-handling actor with a single scalar state and one handler per message.", "actor sq(state) { on job(x) { reply x * x } }"),
    ("on", "Directive", "A message handler inside an actor body: on MSG(params) { stmts; reply EXPR }.", "on job(x) { reply x * 2 }"),
    ("reply", "Directive", "Inside a handler, send an expression's value back to an ask/via caller.", "reply state + x"),
    ("spawn", "Directive", "Bind a session actor: spawn NAME = ACTOR(init); tell/ask drive it (distinct from the spawn CMD source).", "spawn w = worker(0)"),
    ("pool", "Directive", "Bind a supervised session pool: pool NAME = ACTOR * N (a dead worker is respawned).", "pool p = worker * 8"),
    ("supervise", "Directive", "Set a ref's crash policy: supervise NAME { on crash { restart | stop } } (default restart).", "supervise p { on crash { restart } }"),
    // ── Action ──
    ("quit", "Action", "Quit the TUI.", "bind C-q quit"),
    ("beep", "Action", "Ring the terminal bell (0x07 after the next draw).", "expect /panic/ beep"),
    ("alert", "Action", "Flash a message in the status bar for a few seconds.", "timeout 5s alert \"stream idle\""),
    ("exec", "Action", "Run a shell command, fire-and-forget; never waits or blocks the loop.", "expect /down/ exec \"notify-send arb\""),
    ("flash", "Action", "Tint a widget's border/accent for a few seconds (-color, default yellow).", "expect /5\\d\\d/ flash .log red"),
    ("set", "Action", "Set an input .name widget's value; with out { apply .name } reshapes the live pipe.", "bind C-a set .q ERROR"),
    ("tell", "Action", "Post a message to a session actor/pool, fire-and-forget: tell REF MSG(args).", "bind C-t tell w job(5)"),
    ("ask", "Action", "Ask a session actor/pool and store the reply into control .CTRL: ask .CTRL REF MSG(args).", "bind C-a ask .out p job(.th)"),
];

/// The language corpus, exposed for the offline docs generator (`gen-docs`).
pub fn corpus() -> &'static [(&'static str, &'static str, &'static str, &'static str)] {
    CORPUS
}

/// One-line help for a widget / query verb / directive, resolved from the
/// shared `CORPUS` (first entry whose name matches). Names carrying more than
/// one role (e.g. `text` as both widget and query verb) hover as the first
/// listed sense.
fn verb_help(word: &str) -> Option<&'static str> {
    CORPUS
        .iter()
        .find(|(n, _, _, _)| *n == word)
        .map(|(_, _, d, _)| *d)
}

// ─────────────────────────── Grammar data (Area A) ───────────────────────────

/// A verb/directive/action argument signature: `label` is the one-line usage,
/// `params` the ordered parameter names (drives `signatureHelp`).
pub struct Sig {
    pub verb: &'static str,
    pub label: &'static str,
    pub params: &'static [&'static str],
}

/// Argument signatures for each pipeline query verb, extracted from the arg
/// contract each `pipeline_from_body` match arm parses (`src/spec.rs`). Keeps
/// `signatureHelp` faithful to what the parser actually consumes.
pub const SIGS: &[Sig] = &[
    Sig {
        verb: "replace",
        label: "replace /RE/ TO",
        params: &["/RE/", "TO"],
    },
    Sig {
        verb: "slice",
        label: "slice A B",
        params: &["A", "B"],
    },
    Sig {
        verb: "substr",
        label: "substr A B",
        params: &["A", "B"],
    },
    Sig {
        verb: "clamp",
        label: "clamp LO HI",
        params: &["LO", "HI"],
    },
    Sig {
        verb: "between",
        label: "between A B",
        params: &["A", "B"],
    },
    Sig {
        verb: "cut",
        label: "cut DELIM N",
        params: &["DELIM", "N"],
    },
    Sig {
        verb: "grepf",
        label: "grepf FIELD /RE/",
        params: &["FIELD", "/RE/"],
    },
    Sig {
        verb: "set",
        label: "set KEY VAL",
        params: &["KEY", "VAL"],
    },
    Sig {
        verb: "rename",
        label: "rename OLD NEW",
        params: &["OLD", "NEW"],
    },
    Sig {
        verb: "default",
        label: "default KEY VAL",
        params: &["KEY", "VAL"],
    },
    Sig {
        verb: "sel",
        label: "sel CSS [-attr A]",
        params: &["CSS", "-attr"],
    },
    Sig {
        verb: "field",
        label: "field COL | KEY...",
        params: &["COL|KEY"],
    },
    Sig {
        verb: "fields",
        label: "fields COL...",
        params: &["COL..."],
    },
    Sig {
        verb: "pick",
        label: "pick KEY...",
        params: &["KEY..."],
    },
    Sig {
        verb: "sort",
        label: "sort [-n] [-r]",
        params: &["-n", "-r"],
    },
    Sig {
        verb: "take",
        label: "take N",
        params: &["N"],
    },
    Sig {
        verb: "drop",
        label: "drop N",
        params: &["N"],
    },
    Sig {
        verb: "nth",
        label: "nth N",
        params: &["N"],
    },
    Sig {
        verb: "tailn",
        label: "tailn N",
        params: &["N"],
    },
    Sig {
        verb: "pad",
        label: "pad N",
        params: &["N"],
    },
    Sig {
        verb: "lpad",
        label: "lpad N",
        params: &["N"],
    },
    Sig {
        verb: "sma",
        label: "sma N",
        params: &["N"],
    },
    Sig {
        verb: "repeat",
        label: "repeat N",
        params: &["N"],
    },
    Sig {
        verb: "sample",
        label: "sample N",
        params: &["N"],
    },
    Sig {
        verb: "bins",
        label: "bins N",
        params: &["N"],
    },
    Sig {
        verb: "index",
        label: "index N",
        params: &["N"],
    },
    Sig {
        verb: "over",
        label: "over N",
        params: &["N"],
    },
    Sig {
        verb: "under",
        label: "under N",
        params: &["N"],
    },
    Sig {
        verb: "percentile",
        label: "percentile P",
        params: &["P"],
    },
    Sig {
        verb: "ewma",
        label: "ewma ALPHA",
        params: &["ALPHA"],
    },
    Sig {
        verb: "split",
        label: "split DELIM",
        params: &["DELIM"],
    },
    Sig {
        verb: "join",
        label: "join [SEP]",
        params: &["SEP"],
    },
    Sig {
        verb: "contains",
        label: "contains SUBSTR",
        params: &["SUBSTR"],
    },
    Sig {
        verb: "starts",
        label: "starts PREFIX",
        params: &["PREFIX"],
    },
    Sig {
        verb: "ends",
        label: "ends SUFFIX",
        params: &["SUFFIX"],
    },
    Sig {
        verb: "prepend",
        label: "prepend STR",
        params: &["STR"],
    },
    Sig {
        verb: "append",
        label: "append STR",
        params: &["STR"],
    },
    Sig {
        verb: "match",
        label: "match /RE/",
        params: &["/RE/"],
    },
    Sig {
        verb: "reject",
        label: "reject /RE/",
        params: &["/RE/"],
    },
    Sig {
        verb: "extract",
        label: "extract /RE/",
        params: &["/RE/"],
    },
    Sig {
        verb: "where",
        label: "where EXPR",
        params: &["EXPR"],
    },
    Sig {
        verb: "map",
        label: "map EXPR",
        params: &["EXPR"],
    },
    Sig {
        verb: "calc",
        label: "calc EXPR",
        params: &["EXPR"],
    },
    Sig {
        verb: "has",
        label: "has KEY",
        params: &["KEY"],
    },
    Sig {
        verb: "del",
        label: "del KEY",
        params: &["KEY"],
    },
    Sig {
        verb: "sort_by",
        label: "sort_by FIELD",
        params: &["FIELD"],
    },
    Sig {
        verb: "unique_by",
        label: "unique_by FIELD",
        params: &["FIELD"],
    },
    Sig {
        verb: "count_by",
        label: "count_by FIELD",
        params: &["FIELD"],
    },
    Sig {
        verb: "group_by",
        label: "group_by FIELD",
        params: &["FIELD"],
    },
    Sig {
        verb: "min_by",
        label: "min_by FIELD",
        params: &["FIELD"],
    },
    Sig {
        verb: "max_by",
        label: "max_by FIELD",
        params: &["FIELD"],
    },
];

/// Signature for a query verb, or `None`.
pub fn sig_of(verb: &str) -> Option<&'static Sig> {
    SIGS.iter().find(|s| s.verb == verb)
}

/// Top-level directive signatures (`bind`/`expect`/`timeout`/…), ported from the
/// directive dispatch in `src/spec.rs`.
pub const DIRECTIVE_SIGS: &[Sig] = &[
    Sig {
        verb: "bind",
        label: "bind KEY ACTION",
        params: &["KEY", "ACTION"],
    },
    Sig {
        verb: "expect",
        label: "expect /RE/ ACTION",
        params: &["/RE/", "ACTION"],
    },
    Sig {
        verb: "timeout",
        label: "timeout DUR ACTION",
        params: &["DUR", "ACTION"],
    },
    Sig {
        verb: "grid",
        label: "grid .widget -row R -col C [-span N]",
        params: &[".widget", "-row", "-col"],
    },
    Sig {
        verb: "source",
        label: "source .widget { in ... }",
        params: &[".widget", "{body}"],
    },
    Sig {
        verb: "search",
        label: "search .widget { in ... }",
        params: &[".widget", "{body}"],
    },
    Sig {
        verb: "out",
        label: "out { ... }",
        params: &["{body}"],
    },
    Sig {
        verb: "import",
        label: "import NAME [as ALIAS]",
        params: &["NAME", "as", "ALIAS"],
    },
    Sig {
        verb: "configure",
        label: "configure .widget -k v",
        params: &[".widget", "-k v"],
    },
];

/// Action signatures shared by `bind`/`expect`/`timeout`, ported from
/// `parse_action` in `src/spec.rs`.
pub const ACTION_SIGS: &[Sig] = &[
    Sig {
        verb: "set",
        label: "set .NAME VALUE",
        params: &[".NAME", "VALUE"],
    },
    Sig {
        verb: "quit",
        label: "quit",
        params: &[],
    },
    Sig {
        verb: "beep",
        label: "beep",
        params: &[],
    },
    Sig {
        verb: "alert",
        label: "alert MSG",
        params: &["MSG"],
    },
    Sig {
        verb: "flash",
        label: "flash .WIDGET [COLOR]",
        params: &[".WIDGET", "COLOR"],
    },
    Sig {
        verb: "exec",
        label: "exec CMD",
        params: &["CMD"],
    },
];

/// Generic `-flags` accepted by any widget (`parse_opts` consumes them).
const GENERIC_FLAGS: &[&str] = &["-color", "-label", "-title"];

/// The `-flags` a given widget verb accepts, ported from the widget option
/// parsing in `src/spec.rs`/`src/query.rs`. Empty for widgets that take none.
fn flags_for(kind: &str) -> &'static [&'static str] {
    match kind {
        "gauge" => &["-max"],
        "tail" | "list" => &["-limit", "-lines"],
        "table" => &["-cols"],
        "select" => &["-prompt", "-header"],
        "slider" => &["-min", "-max", "-step"],
        "facet" => &["-opts", "-field"],
        "check" => &["-label"],
        "bars" | "histo" => &["-top"],
        "tabs" => &["-tabs"],
        "sel" => &["-attr"],
        _ => &[],
    }
}

/// Semantic-token classes and their LSP legend indices.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tok {
    Verb,
    WidgetPath,
    ControlRef,
    Regex,
    Flag,
    Number,
    Str,
    Brace,
}

/// The LSP semantic-token legend (`tokenTypes`); each `Tok` maps to an index.
pub const TOKEN_LEGEND: &[&str] = &[
    "keyword", "property", "variable", "regexp", "operator", "number", "string",
];

impl Tok {
    /// Index into `TOKEN_LEGEND`, or `None` for punctuation that isn't colored.
    fn legend_index(self) -> Option<u32> {
        Some(match self {
            Tok::Verb => 0,
            Tok::WidgetPath => 1,
            Tok::ControlRef => 2,
            Tok::Regex => 3,
            Tok::Flag => 4,
            Tok::Number => 5,
            Tok::Str => 6,
            Tok::Brace => return None,
        })
    }
}

/// Classify a command-level token. `first_word` selects the verb-head rule
/// (recognized verb/widget → `Verb`, else `Str`); other positions disambiguate
/// `.path` vs `.5`, `-flag` vs `-3`, `/re/`, and `"str"`.
fn classify(tok: &str, first_word: bool) -> Tok {
    if tok == "{" || tok == "}" {
        return Tok::Brace;
    }
    if first_word {
        if crate::spec::WidgetKind::from(tok).is_some() || verb_help(tok).is_some() {
            return Tok::Verb;
        }
        // A jq/xpath literal at command position (the front-ends): color it as the
        // query construct it is, not as a bare string.
        if tok.starts_with("select(") || tok.starts_with("map(") {
            return Tok::Verb;
        }
        if (tok.starts_with('.') && tok.parse::<f64>().is_err())
            || tok.starts_with('/')
            || tok.starts_with('@')
        {
            return Tok::WidgetPath;
        }
        return Tok::Str;
    }
    if tok.len() >= 2 && tok.starts_with('/') && tok.ends_with('/') {
        return Tok::Regex;
    }
    if tok.starts_with('"') {
        return Tok::Str;
    }
    if tok.parse::<f64>().is_ok() {
        return Tok::Number;
    }
    if tok.starts_with('.') {
        return Tok::WidgetPath;
    }
    if tok.starts_with('-') {
        return Tok::Flag;
    }
    Tok::Str
}

/// Keywords/builtins of the `where`/`map`/`calc` expression sub-language.
const EXPR_KEYWORDS: &[&str] = &["and", "or", "not", "in", "match", "x"];
const EXPR_CMP_OPS: &[&str] = &["==", "!=", "<", "<=", ">", ">="];
const EXPR_ARITH_OPS: &[&str] = &["+", "-", "*", "/", "%"];

/// Classify a token inside a `where`/`map`/`calc` expression argument, so the
/// sub-language highlights distinctly from pipeline verbs.
fn classify_expr(tok: &str) -> Tok {
    if EXPR_KEYWORDS.contains(&tok) {
        return Tok::Verb;
    }
    if EXPR_CMP_OPS.contains(&tok) || EXPR_ARITH_OPS.contains(&tok) {
        return Tok::Flag;
    }
    if tok.parse::<f64>().is_ok() {
        return Tok::Number;
    }
    if tok.starts_with('.') {
        return Tok::ControlRef;
    }
    Tok::Str
}

// ──────────────────────────── LSP features (Area B) ──────────────────────────

/// LSP `CompletionItemKind` for a CORPUS chapter.
fn kind_for(chapter: &str) -> u8 {
    match chapter {
        "Widget" => 7,     // Class
        "Directive" => 14, // Keyword
        "Action" => 23,    // Event
        _ => 3,            // Function (Query/Input)
    }
}

/// Completion items for the cursor: declared `.path` names in dot-context,
/// widget `-flags` in dash-context, else the whole CORPUS verb set.
fn completions(src: &str, line: u32, ch: u32) -> Vec<Value> {
    let tok = token_before(src, line, ch);
    if tok.starts_with('.') {
        return document_symbols(src)
            .iter()
            .filter_map(|s| s["name"].as_str().map(String::from))
            .map(|p| json!({ "label": p, "kind": 5 })) // Field
            .collect();
    }
    if tok.starts_with('-') {
        let verb = src
            .lines()
            .nth(line as usize)
            .and_then(|l| l.split_whitespace().next())
            .unwrap_or("");
        return flags_for(verb)
            .iter()
            .chain(GENERIC_FLAGS.iter())
            .map(|f| json!({ "label": f, "kind": 14 })) // Keyword
            .collect();
    }
    CORPUS
        .iter()
        .map(|(name, chap, doc, example)| {
            json!({
                "label": name,
                "kind": kind_for(chap),
                "detail": chap,
                "documentation": {
                    "kind": "markdown",
                    "value": format!("{doc}\n\n```arb\n{example}\n```"),
                },
            })
        })
        .collect()
}

/// The partial token ending at `(line, ch)`, including a leading `.`/`-` so the
/// completion handler can branch on context.
fn token_before(src: &str, line: u32, ch: u32) -> String {
    let Some(l) = src.lines().nth(line as usize) else {
        return String::new();
    };
    let chars: Vec<char> = l.chars().collect();
    let end = char_idx_from_utf16(&chars, ch);
    let is_tok = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-';
    let mut i = end;
    while i > 0 && is_tok(chars[i - 1]) {
        i -= 1;
    }
    chars[i..end].iter().collect()
}

/// Pair `{`..`}` across lines (ignoring braces inside `"…"` strings) into
/// `FoldingRange`s. Ported from awkrs' string-aware brace matcher.
fn folding_ranges(src: &str) -> Vec<Value> {
    let mut stack: Vec<u32> = Vec::new();
    let mut out = Vec::new();
    for (li, line) in src.lines().enumerate() {
        let (mut in_str, mut esc) = (false, false);
        for c in line.chars() {
            if in_str {
                if esc {
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else if c == '"' {
                    in_str = false;
                }
            } else if c == '"' {
                in_str = true;
            } else if c == '{' {
                stack.push(li as u32);
            } else if c == '}' {
                if let Some(open) = stack.pop() {
                    if li as u32 > open {
                        out.push(json!({ "startLine": open, "endLine": li as u32 }));
                    }
                }
            }
        }
    }
    out
}

/// A `.path` or bare identifier at `(line, ch)`; `.` counts as a word char so a
/// widget path is one unit. Shared by highlight/references/definition/rename.
fn word_at(src: &str, line: u32, ch: u32) -> Option<String> {
    let l = src.lines().nth(line as usize)?;
    let cs: Vec<char> = l.chars().collect();
    let i = char_idx_from_utf16(&cs, ch);
    let is_w = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '.';
    let mut s = i;
    while s > 0 && is_w(cs[s - 1]) {
        s -= 1;
    }
    let mut e = i;
    while e < cs.len() && is_w(cs[e]) {
        e += 1;
    }
    if s == e {
        None
    } else {
        Some(cs[s..e].iter().collect())
    }
}

/// Whole-word occurrences of `word`, as LSP ranges (UTF-16 columns), one line at
/// a time. Ported from awkrs' `word_occurrences`.
fn word_occurrences(src: &str, word: &str) -> Vec<Value> {
    let w: Vec<char> = word.chars().collect();
    let wl = w.len();
    let is_w = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '.';
    let mut out = Vec::new();
    if wl == 0 {
        return out;
    }
    for (li, line) in src.lines().enumerate() {
        let cs: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i + wl <= cs.len() {
            if cs[i..i + wl] == w[..]
                && (i == 0 || !is_w(cs[i - 1]))
                && (i + wl == cs.len() || !is_w(cs[i + wl]))
            {
                let start = col_utf16(line, i);
                let end = col_utf16(line, i + wl);
                out.push(json!({
                    "start": { "line": li as u32, "character": start },
                    "end": { "line": li as u32, "character": end },
                }));
                i += wl;
            } else {
                i += 1;
            }
        }
    }
    out
}

/// UTF-16 column of char-offset `char_col` within `line` (LSP position unit).
fn col_utf16(line: &str, char_col: usize) -> u32 {
    line.chars()
        .take(char_col)
        .map(|c| c.len_utf16() as u32)
        .sum()
}

/// The `<widget-verb> .path` declaration line for `path`, as a `Location`. Uses
/// the same predicate `document_symbols` uses to recognize a widget decl.
fn widget_decl_location(src: &str, uri: &str, path: &str) -> Option<Value> {
    if !path.starts_with('.') {
        return None;
    }
    for (i, line) in src.lines().enumerate() {
        let mut it = line.split_whitespace();
        if let (Some(verb), Some(p)) = (it.next(), it.next()) {
            if p == path && crate::spec::WidgetKind::from(verb).is_some() {
                let len = line_len_utf16(line);
                return Some(json!({ "uri": uri, "range": symbol_range(i as u32, len) }));
            }
        }
    }
    None
}

/// `SignatureHelp` for the verb/directive/action heading the current line.
fn signature_help(src: &str, line: u32) -> Option<Value> {
    let l = src.lines().nth(line as usize)?;
    let verb = l.split_whitespace().next()?;
    let sig = sig_of(verb)
        .or_else(|| DIRECTIVE_SIGS.iter().find(|s| s.verb == verb))
        .or_else(|| ACTION_SIGS.iter().find(|s| s.verb == verb))?;
    let doc = verb_help(verb).unwrap_or("");
    Some(json!({
        "signatures": [{
            "label": sig.label,
            "documentation": { "kind": "markdown", "value": doc },
        }],
        "activeSignature": 0,
        "activeParameter": 0,
    }))
}

/// Net unquoted brace balance of one line (`+1` per `{`, `-1` per `}`).
fn net_brace_delta(line: &str) -> i32 {
    let (mut in_str, mut esc, mut d) = (false, false, 0i32);
    for c in line.chars() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
        } else if c == '{' {
            d += 1;
        } else if c == '}' {
            d -= 1;
        }
    }
    d
}

/// Re-indent each line to two spaces per unquoted brace-depth (only leading
/// whitespace is rewritten). One full-document `TextEdit`.
fn format_arb(src: &str) -> String {
    let mut depth = 0i32;
    let mut out = String::new();
    for line in src.lines() {
        let t = line.trim();
        let starts_close = t.starts_with('}');
        let indent = (depth - starts_close as i32).max(0) as usize;
        out.push_str(&"  ".repeat(indent));
        out.push_str(t);
        out.push('\n');
        depth += net_brace_delta(t);
        if depth < 0 {
            depth = 0;
        }
    }
    out
}

/// Whitespace-split tokens of a line as `(char_start, text)`.
fn line_tokens(line: &str) -> Vec<(usize, String)> {
    let cs: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        if cs[i].is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        while i < cs.len() && !cs[i].is_whitespace() {
            i += 1;
        }
        out.push((start, cs[start..i].iter().collect()));
    }
    out
}

/// Delta-encoded LSP semantic tokens (`[dLine, dStart, len, type, 0]`*) over the
/// whole document. Columns are UTF-16 (LSP requirement); command-head tokens use
/// `classify`, `where`/`map`/`calc` args use `classify_expr`.
fn semantic_tokens(src: &str) -> Vec<u32> {
    let mut data = Vec::new();
    let (mut pl, mut ps) = (0u32, 0u32);
    for (li, line) in src.lines().enumerate() {
        let li = li as u32;
        let toks = line_tokens(line);
        let in_expr = toks
            .first()
            .map(|(_, t)| matches!(t.as_str(), "where" | "map" | "calc"))
            .unwrap_or(false);
        for (idx, (cstart, text)) in toks.iter().enumerate() {
            let tok = if idx == 0 {
                classify(text, true)
            } else if in_expr {
                classify_expr(text)
            } else {
                classify(text, false)
            };
            let Some(ty) = tok.legend_index() else {
                continue;
            };
            let col = col_utf16(line, *cstart);
            let len: u32 = text.chars().map(|c| c.len_utf16() as u32).sum();
            let dl = li - pl;
            let ds = if dl == 0 { col - ps } else { col };
            data.extend_from_slice(&[dl, ds, len, ty, 0]);
            pl = li;
            ps = col;
        }
    }
    data
}

/// Read one `Content-Length`-framed JSON-RPC message. `Ok(None)` on clean EOF.
pub fn read_message<R: BufRead>(r: &mut R) -> io::Result<Option<Value>> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(None); // EOF
        }
        let t = line.trim_end();
        if t.is_empty() {
            break; // end of headers
        }
        if let Some((k, v)) = t.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse::<usize>().ok();
            }
        }
    }
    let n = match len {
        Some(n) => n,
        None => return Ok(None),
    };
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf).unwrap_or(Value::Null)))
}

/// Write one `Content-Length`-framed JSON-RPC message (byte length, then flush).
pub fn write_message<W: Write>(w: &mut W, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg).unwrap_or_default();
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()
}

/// Run the LSP over stdio until EOF or `exit`.
pub fn run() {
    let stdin = io::stdin();
    let mut r = stdin.lock();
    let stdout = io::stdout();
    let mut w = stdout.lock();
    let mut server = Server::default();
    while let Ok(Some(msg)) = read_message(&mut r) {
        let is_exit = msg.get("method").and_then(Value::as_str) == Some("exit");
        for out in handle(&mut server, &msg) {
            if write_message(&mut w, &out).is_err() {
                return;
            }
        }
        if is_exit {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_idx_from_utf16_handles_non_bmp() {
        // "🚀ab": rocket is 2 UTF-16 units (1 char), then 'a','b'.
        let cs: Vec<char> = "🚀ab".chars().collect();
        assert_eq!(char_idx_from_utf16(&cs, 0), 0); // before rocket
        assert_eq!(char_idx_from_utf16(&cs, 2), 1); // after rocket -> 'a'
        assert_eq!(char_idx_from_utf16(&cs, 3), 2); // -> 'b'
        assert_eq!(char_idx_from_utf16(&cs, 99), 3); // clamps to len
    }

    #[test]
    fn word_at_resolves_token_after_non_bmp_prefix() {
        // Two rockets (4 UTF-16 units) + space, then `gauge` at UTF-16 col 5..10.
        // Indexing by the raw column (old bug) would land past the word.
        let src = "🚀🚀 gauge";
        assert_eq!(word_at(src, 0, 7).as_deref(), Some("gauge"));
    }

    #[test]
    fn corpus_covers_every_input_marker() {
        // Every stdin format marker `pipeline_from_body` accepts must have a doc.
        for m in [
            "in",
            "in.json",
            "in.html",
            "in.xml",
            "in.logfmt",
            "in.csv",
            "in.tsv",
            "in.yaml",
            "in.yml",
            "in.toml",
        ] {
            assert!(verb_help(m).is_some(), "CORPUS missing input marker `{m}`");
        }
    }

    #[test]
    fn utf16_columns_for_astral() {
        // A rocket (U+1F680) is one Unicode scalar but two UTF-16 code units.
        assert_eq!(line_len_utf16("ab"), 2);
        assert_eq!(line_len_utf16("a🚀b"), 4); // 1 + 2 + 1
        assert_eq!(col_utf16("a🚀b", 2), 3); // through the astral char
    }

    #[test]
    fn classify_disambiguates() {
        assert_eq!(classify("-3", false), Tok::Number);
        assert_eq!(classify("-max", false), Tok::Flag);
        assert_eq!(classify(".cpu", false), Tok::WidgetPath);
        assert_eq!(classify(".5", false), Tok::Number);
        assert_eq!(classify("/5\\d\\d/", false), Tok::Regex);
        assert_eq!(classify("sort", true), Tok::Verb);
        assert_eq!(classify("gauge", true), Tok::Verb);
        assert_eq!(classify("foo", true), Tok::Str);
        assert_eq!(classify("{", false), Tok::Brace);
        // jq/xpath literals at command position color as the query construct.
        assert_eq!(classify(".foo.bar", true), Tok::WidgetPath); // jq path
        assert_eq!(classify("//a", true), Tok::WidgetPath); // xpath
        assert_eq!(classify("@href", true), Tok::WidgetPath); // xpath attr step
        assert_eq!(classify("select(.x>1)", true), Tok::Verb); // jq filter
    }

    #[test]
    fn classify_expr_control_and_ops() {
        assert_eq!(classify_expr("and"), Tok::Verb);
        assert_eq!(classify_expr(".cpu"), Tok::ControlRef);
        assert_eq!(classify_expr(">="), Tok::Flag);
        assert_eq!(classify_expr("42"), Tok::Number);
    }

    #[test]
    fn every_sig_verb_is_in_corpus() {
        for s in SIGS {
            assert!(
                verb_help(s.verb).is_some(),
                "SIGS verb `{}` not in CORPUS",
                s.verb
            );
        }
    }

    #[test]
    fn flags_for_known_kinds() {
        let sl = flags_for("slider");
        assert!(sl.contains(&"-min") && sl.contains(&"-max") && sl.contains(&"-step"));
        assert!(flags_for("text").is_empty());
        assert!(flags_for("nope").is_empty());
    }

    #[test]
    fn directive_and_action_sigs_present() {
        for v in ["bind", "expect", "timeout"] {
            assert!(DIRECTIVE_SIGS.iter().any(|s| s.verb == v));
        }
        for v in ["flash", "alert"] {
            assert!(ACTION_SIGS.iter().any(|s| s.verb == v));
        }
    }

    #[test]
    fn completion_dot_context_lists_declared_paths() {
        let src = "gauge .cpu\napply .";
        // Cursor right after `apply .` (line 1, col 7) -> declared widget paths.
        let items = completions(src, 1, 7);
        assert_eq!(items[0]["label"], ".cpu");
        // A bare position offers the CORPUS verbs.
        let verbs = completions(src, 0, 0);
        assert!(verbs.iter().any(|i| i["label"] == "gauge"));
    }

    #[test]
    fn completion_dash_context_lists_flags() {
        let src = "slider .th -";
        let items = completions(src, 0, 12);
        let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
        assert!(labels.contains(&"-min") && labels.contains(&"-color"));
    }

    #[test]
    fn folding_pairs_braces_not_in_strings() {
        let folds = folding_ranges("source .x {\n in\n}");
        assert_eq!(folds.len(), 1);
        assert_eq!(folds[0]["startLine"], 0);
        assert_eq!(folds[0]["endLine"], 2);
        // A brace inside a string never opens a fold.
        assert!(folding_ranges("expect /x/ alert \"{\"").is_empty());
    }

    #[test]
    fn word_occurrences_whole_word_dotpath() {
        let src = "gauge .cpu\napply .cpu\nsource .cpu2";
        let occ = word_occurrences(src, ".cpu");
        assert_eq!(occ.len(), 2); // .cpu twice; .cpu2 is not a whole-word match
    }

    #[test]
    fn definition_jumps_to_widget_decl() {
        let src = "gauge .cpu\napply .cpu";
        // Cursor on `.cpu` in `apply .cpu`.
        let loc = word_at(src, 1, 8).and_then(|w| widget_decl_location(src, "file://x", &w));
        assert_eq!(loc.unwrap()["range"]["start"]["line"], 0);
    }

    #[test]
    fn signature_help_for_replace() {
        let v = signature_help("replace /a/ b", 0).unwrap();
        assert_eq!(v["signatures"][0]["label"], "replace /RE/ TO");
    }

    #[test]
    fn format_indents_nested_blocks() {
        assert_eq!(format_arb("source .x {\nin\n}"), "source .x {\n  in\n}\n");
    }

    #[test]
    fn semantic_tokens_delta_encoding() {
        // `gauge .cpu` -> Verb(0) then WidgetPath(1), delta-encoded.
        assert_eq!(
            semantic_tokens("gauge .cpu"),
            vec![0, 0, 5, 0, 0, 0, 6, 4, 1, 0]
        );
    }
}
