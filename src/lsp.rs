//! Language Server (`arb --lsp`): a std-only, no-async LSP over stdio JSON-RPC
//! for `.arb` specs. It parses + builds each open document and republishes the
//! parser/interpreter error as a diagnostic, plus `documentSymbol` (the widgets)
//! and `hover` (verb help). The transport shape is the standard `Content-Length`
//! framing; the language logic (`diagnose`/`handle`) is pure and unit-testable.
//!
//! Diagnostics anchor to the error's real source span (the offending verb token
//! or lexer position). Columns are char counts, not LSP UTF-16 units, and spans
//! are per-command (the verb), not per-argument — matching the rest of this file.

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
            Diag { message: err.msg, line, start_col, end_col: end_col.max(start_col + 1) }
        }
        None => {
            let ec = src.lines().next().map(|l| l.chars().count() as u32).unwrap_or(0);
            Diag { message: err.msg, line: 0, start_col: 0, end_col: ec.max(1) }
        }
    };
    vec![diag]
}

/// Map a char offset to (0-based line, 0-based column). Columns are char counts
/// (matching the rest of this file), not LSP UTF-16 units.
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
            col += 1;
        }
    }
    (line, col)
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
            },
            "serverInfo": { "name": "arb", "version": env!("CARGO_PKG_VERSION") },
        }))],
        "initialized" | "exit" => vec![],
        "shutdown" => vec![reply(Value::Null)],
        "textDocument/didOpen" => {
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
            let text = params["textDocument"]["text"].as_str().unwrap_or("").to_string();
            let notif = publish_diagnostics(&uri, &text);
            server.docs.insert(uri, text);
            vec![notif]
        }
        "textDocument/didChange" => {
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
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
            let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
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
                Some(md) => vec![reply(json!({ "contents": { "kind": "markdown", "value": md } }))],
                None => vec![reply(Value::Null)],
            }
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
        let (Some(verb), Some(path)) = (it.next(), it.next()) else { continue };
        if crate::spec::WidgetKind::from(verb).is_some() && path.starts_with('.') {
            let len = line.chars().count() as u32;
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
    let ci = (ch as usize).min(chars.len());
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
const CORPUS: &[(&str, &str, &str)] = &[
    // ── Input ──
    ("in", "Input", "Begin the pipeline, reading the raw stdin stream line by line."),
    ("in.json", "Input", "Begin the pipeline reading stdin (JSON-lines intent; a marker, same as in)."),
    ("in.html", "Input", "Begin the pipeline reading stdin as HTML (marker, same as in; use with sel/find)."),
    ("in.xml", "Input", "Begin the pipeline reading stdin as XML (marker, same as in)."),
    ("in.logfmt", "Input", "Begin the pipeline reading stdin as logfmt (marker, same as in)."),
    // ── Query ──
    ("find", "Query", "Parse stream as HTML, emit the outer HTML of each element matching the selector."),
    ("attr", "Query", "From element fragments, emit each element's named attribute; drop those lacking it."),
    ("text", "Query", "From element fragments, emit each element's inner text; non-elements pass through."),
    ("match", "Query", "Keep lines matching the regex."),
    ("grep", "Query", "Keep lines matching the regex."),
    ("reject", "Query", "Drop lines matching the regex."),
    ("grepv", "Query", "Drop lines matching the regex."),
    ("field", "Query", "Replace each line with a selected field (whitespace column or JSON key path)."),
    ("fields", "Query", "Project multiple 1-based whitespace columns, space-joined, in the given order."),
    ("each", "Query", "Flatten JSON-array lines into one line per element; non-array lines pass through."),
    ("count", "Query", "Reduce to the current line count (scalar)."),
    ("rate", "Query", "Reduce to lines-per-second over the elapsed window (scalar)."),
    ("tally", "Query", "Group identical lines and count them, sorted by count desc then key asc."),
    ("sum", "Query", "Sum of lines parsed as numbers; non-numeric lines are ignored (scalar)."),
    ("min", "Query", "Minimum of numeric lines, or 0 if none (scalar)."),
    ("max", "Query", "Maximum of numeric lines, or 0 if none (scalar)."),
    ("avg", "Query", "Mean of numeric lines, or 0 if none (scalar)."),
    ("keys", "Query", "Flatten a JSON object's keys into one line each; non-object lines pass through."),
    ("vals", "Query", "Flatten a JSON object's values into one line each; non-object lines pass through."),
    ("pick", "Query", "Project a JSON object to the named keys, keeping order; missing keys dropped."),
    ("sort", "Query", "Sort the lines; -n sorts numerically, -r reverses."),
    ("uniq", "Query", "Drop duplicate lines globally, keeping the first occurrence."),
    ("rev", "Query", "Reverse the order of the lines."),
    ("first", "Query", "Keep only the first line."),
    ("last", "Query", "Keep only the last line."),
    ("upper", "Query", "Uppercase each line."),
    ("lower", "Query", "Lowercase each line."),
    ("trim", "Query", "Strip leading and trailing whitespace from each line."),
    ("replace", "Query", "Regex replace-all per line (replace /RE/ TO); TO may use $1 captures."),
    ("join", "Query", "Collapse all lines into one, joined by a separator (default a space)."),
    ("nth", "Query", "Keep only the Nth line (1-based)."),
    ("take", "Query", "Keep the first N lines."),
    ("drop", "Query", "Drop the first N lines."),
    ("calc", "Query", "Reduce to a scalar from an arithmetic expression over the line count (x)."),
    ("where", "Query", "Keep lines whose numeric value / field predicate holds (fusevm or Rust eval)."),
    ("map", "Query", "Replace each line with an expression's value (field-aware; x = line-as-number)."),
    ("sort_by", "Query", "Stable-sort JSON records by FIELD (numeric if all parse, else lexicographic)."),
    ("unique_by", "Query", "Keep the first record per distinct FIELD value, preserving input order."),
    ("count_by", "Query", "Group JSON records by FIELD and count; value->count sorted by count desc."),
    ("group_by", "Query", "Group lines by FIELD into one JSON-array line per value, keys ascending."),
    ("min_by", "Query", "Emit the single record whose numeric FIELD is smallest."),
    ("max_by", "Query", "Emit the single record whose numeric FIELD is largest."),
    ("has", "Query", "Keep only JSON-object lines that contain KEY; all other lines are dropped."),
    ("entries", "Query", "Expand each JSON object into one key/value object line per key (jq to_entries)."),
    ("flatten", "Query", "Emit each element of JSON-array lines, one level deeper than each."),
    ("add", "Query", "Reduce a JSON array line to one value (sum numbers, else concat their strings)."),
    ("over", "Query", "Keep lines parsing as a number strictly greater than N; drop non-numeric."),
    ("under", "Query", "Keep numeric lines strictly less than N; drop non-numeric lines."),
    ("between", "Query", "Keep numeric lines x where lo <= x <= hi inclusive; drop non-numeric lines."),
    ("enumerate", "Query", "Prefix each line with its 1-based index and a tab."),
    ("words", "Query", "Split each line on whitespace and emit one word per line."),
    ("dedup", "Query", "Collapse runs of adjacent identical lines to one (classic uniq)."),
    ("tailn", "Query", "Keep only the last N lines."),
    ("pad", "Query", "Right-pad each line with spaces to a minimum visible width N (no truncation)."),
    ("lpad", "Query", "Left-pad each line with spaces to a minimum width N."),
    ("grepf", "Query", "Keep lines whose FIELD (JSON key or 1-based column) matches the regex."),
    ("apply", "Query", "Placeholder replaced at render by the pipeline typed into input .name; else no-op."),
    ("basename", "Query", "Path basename: the part after the last / (the whole line if none)."),
    ("dirname", "Query", "Path dirname: the part before the last / (. if none)."),
    ("commafy", "Query", "Group a numeric line's integer part with thousands separators; else pass through."),
    ("bytes", "Query", "Humanize a byte count (1024-based): 1536 -> 1.5 KB; non-numeric passes through."),
    ("duration", "Query", "Humanize seconds: 3661 -> 1h 1m; non-numeric lines pass through."),
    ("flip", "Query", "Reverse the Unicode characters of each line."),
    ("b64", "Query", "Base64-encode each line."),
    ("b64d", "Query", "Base64-decode each line; invalid or non-UTF8 lines pass through unchanged."),
    ("hex", "Query", "Lowercase hex-encode each line, two hex digits per UTF-8 byte."),
    ("unhex", "Query", "Decode a hex string to UTF-8; on any error the line passes through unchanged."),
    ("urlenc", "Query", "Percent-encode each line, escaping every non-alphanumeric byte (RFC 3986)."),
    ("urldec", "Query", "Percent-decode each line to UTF-8; invalid UTF-8 passes through unchanged."),
    ("extract", "Query", "Emit the first regex match per line (group 1 if any); drop lines with no match."),
    ("split", "Query", "Explode each line by the literal DELIM into multiple lines (one part per line)."),
    ("substr", "Query", "Character substring [A,B) 0-based, clamped to the line length."),
    ("chars", "Query", "Explode each line into one output line per Unicode character."),
    ("title", "Query", "Title-case each line: capitalize each word's first letter, lowercase the rest."),
    ("repeat", "Query", "Replace each line with its content repeated N times, concatenated."),
    ("set", "Query", "Set key K to string value V in each JSON object line; non-objects pass through."),
    ("del", "Query", "Remove key K from each JSON object line; non-object lines pass through."),
    ("rename", "Query", "Rename JSON object key OLD to NEW, keeping the value; no-op if OLD absent."),
    ("default", "Query", "Set string key K to V only when K is absent from the JSON object."),
    ("merge", "Query", "Reduce all JSON object lines into one object (later keys win); emits one line."),
    ("floor", "Query", "Floor each numeric line to the nearest lower integer; non-numeric passes through."),
    ("ceil", "Query", "Round each numeric line up to the nearest integer; non-numeric passes through."),
    ("round", "Query", "Round each numeric line to the nearest integer."),
    ("abs", "Query", "Absolute value of each numeric line."),
    ("clamp", "Query", "Clamp each numeric line into the inclusive range [LO, HI]; non-numeric untouched."),
    ("len", "Query", "Replace each line with its character count."),
    ("wc", "Query", "Replace each line with its word count."),
    ("index", "Query", "Keep only the Nth line (1-based); out-of-range yields no lines."),
    ("cut", "Query", "Split each line by DELIM and keep the Nth (1-based) field."),
    ("contains", "Query", "Keep lines containing a literal substring."),
    ("starts", "Query", "Keep lines starting with a literal prefix."),
    ("ends", "Query", "Keep lines ending with a literal suffix."),
    ("nonempty", "Query", "Drop empty or whitespace-only lines."),
    ("numeric", "Query", "Keep only lines that parse as a number."),
    ("distinct", "Query", "Reduce to the count of distinct lines (scalar)."),
    ("sample", "Query", "Keep every Nth line (1-based)."),
    ("range", "Query", "Max minus min of numeric lines (scalar)."),
    ("bins", "Query", "Bucket numeric lines into N equal-width ranges -> (range, count) pairs."),
    ("cumsum", "Query", "Running cumulative total of the numeric series."),
    ("delta", "Query", "Consecutive differences of the numeric series (n values -> n-1 deltas)."),
    ("ewma", "Query", "Exponentially-weighted moving average, smoothing factor alpha in (0,1]; s0=x0."),
    ("sma", "Query", "Simple moving average over a trailing window of N (length-preserving)."),
    ("median", "Query", "Median of numeric lines (scalar)."),
    ("percentile", "Query", "Nth percentile (0-100) of numeric values, linear interpolation between ranks."),
    ("p50", "Query", "50th percentile (median) of the numeric values (scalar)."),
    ("p90", "Query", "90th percentile of the numeric values (scalar)."),
    ("p95", "Query", "95th percentile of the numeric values (scalar)."),
    ("p99", "Query", "99th percentile of the numeric values, for latency tails (scalar)."),
    ("stddev", "Query", "Population standard deviation of numeric lines (scalar)."),
    ("product", "Query", "Product of numeric lines (scalar)."),
    ("append", "Query", "Suffix every line with a literal string."),
    ("prepend", "Query", "Prefix every line with a literal string."),
    ("slice", "Query", "Keep lines from index A to B inclusive (1-based)."),
    ("sel", "Query", "Parse stream as HTML, emit each CSS-selector match's text (or -attr value)."),
    // ── Widget ──
    ("text", "Widget", "Shows the last stream line, or a scalar/top-pair result, in a bordered pane."),
    ("tail", "Widget", "Scrolling list of the newest lines that fit (tail -f); -limit/-lines caps rows."),
    ("list", "Widget", "Scrollable list of stream lines in a bordered pane; -limit caps rows shown."),
    ("gauge", "Widget", "Renders a scalar as a progress bar filled to value/max (-max, default 100)."),
    ("bars", "Widget", "Horizontal bar chart of tally/count pairs; -top caps the number of bars (def 20)."),
    ("histo", "Widget", "Bar chart of value-distribution pairs; same rendering as bars, -top caps bars."),
    ("spark", "Widget", "Compact braille sparkline of the numeric series, newest points that fit width."),
    ("chart", "Widget", "Braille line chart of the numeric series with auto-scaled x/y axes."),
    ("table", "Widget", "Columnar table of records; -cols sets headers, newest rows that fit are shown."),
    ("tabs", "Widget", "Tab bar from -tabs {a b}; first tab selected (labelled selector, no per-tab body)."),
    ("block", "Widget", "Bordered container box rendering its bound stream content as a list."),
    ("frame", "Widget", "Framed container rendering its bound stream content as a list."),
    ("input", "Widget", "Editable text field; its value is spliced into pipelines via apply .name."),
    ("select", "Widget", "Interactive fuzzy-select over the stream; type to filter, Tab marks, Enter emits."),
    ("filter", "Widget", "Labelled filter control; its value drives where match(.name) / apply."),
    ("facet", "Widget", "Multi-select of values (-opts or distinct -field); drives where <field> in .name."),
    ("slider", "Widget", "Numeric control (-min/-max/-step) driving where <field> < .name; arrows adjust."),
    ("check", "Widget", "Boolean toggle (-label); its value is the string \"1\" or \"0\"."),
    // ── Directive ──
    ("import", "Directive", "Inline a module (stdlib preset or user .arb file) into the spec."),
    ("import as", "Directive", "Build a module into namespace ALIAS, prefixing its names, then merge it in."),
    ("source", "Directive", "Attach a query pipeline (body must start with in) as a widget's data source."),
    ("search", "Directive", "Attach a select widget's fuzzy-match key pipeline; the row still shows source."),
    ("bind", "Directive", "Bind a control key to an action (set|quit|beep|alert|flash|exec, or a { } block)."),
    ("expect", "Directive", "Fire an action when a stream line matches /regex/ (event-driven reaction)."),
    ("timeout", "Directive", "Fire an action when the stream is idle for a duration (Ns/Nms/Nm)."),
    ("out", "Directive", "Apply a query pipeline to the stream and write it to stdout (modify a pipe)."),
    ("grid", "Directive", "Place a widget in the layout grid (-row/-col, -span/-rowspan/-colspan)."),
    ("configure", "Directive", "Merge new -opts into an already-declared widget (build-time; later keys win)."),
    // ── Action ──
    ("quit", "Action", "Quit the TUI."),
    ("beep", "Action", "Ring the terminal bell (0x07 after the next draw)."),
    ("alert", "Action", "Flash a message in the status bar for a few seconds."),
    ("exec", "Action", "Run a shell command, fire-and-forget; never waits or blocks the loop."),
    ("flash", "Action", "Tint a widget's border/accent for a few seconds (-color, default yellow)."),
    ("set", "Action", "Set an input .name widget's value; with out { apply .name } reshapes the live pipe."),
];

/// The language corpus, exposed for the offline docs generator (`gen-docs`).
pub fn corpus() -> &'static [(&'static str, &'static str, &'static str)] {
    CORPUS
}

/// One-line help for a widget / query verb / directive, resolved from the
/// shared `CORPUS` (first entry whose name matches). Names carrying more than
/// one role (e.g. `text` as both widget and query verb) hover as the first
/// listed sense.
fn verb_help(word: &str) -> Option<&'static str> {
    CORPUS.iter().find(|(n, _, _)| *n == word).map(|(_, _, d)| *d)
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
