//! Interpreter: command tree -> a `Spec`. Recognizes the widget verbs,
//! `source .x { … }` whose body compiles to a query pipeline (see `query`), and
//! the `.x <- in` bind shorthand. Unknown widget verbs are ignored so specs stay
//! forward-compatible.

use std::collections::BTreeMap;
use std::time::Duration;

use regex::Regex;

use crate::ast::{Arg, Command};
use crate::query::{FieldSel, QueryOp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetKind {
    Text,
    Tail,
    List,
    Gauge,
    Bars,
    Histo,
    Spark,
    Chart,
    Table,
    Tabs,
    Block,
    Frame,
    /// An editable text field. Its live value is bound into pipelines via
    /// `apply .name` (parse the value as a query pipeline) — the megafilter/map.
    Input,
    /// An interactive fuzzy-select list over the stream (the fzf surface as a
    /// widget). Its presence puts the TUI in select mode: type to fuzzy-filter,
    /// arrows/Ctrl-N/P move the cursor, Tab marks, Enter emits the picked lines.
    /// `-prompt`/`-header` opts set the prompt line and a header above the list.
    Select,
    /// A text box whose value drives `where match(.name)` / `apply` — a labelled
    /// filter control (a specialised `input`).
    Filter,
    /// A multi-select of candidate values (`-opts {a b c}`, else distinct stream
    /// `-field` values). Its value is the comma-joined selected set for
    /// `where <field> in .name`.
    Facet,
    /// A numeric control (`-min`/`-max`/`-step`) whose value drives
    /// `where <field> < .name`; arrows/`+`/`-` adjust it.
    Slider,
    /// A boolean toggle (`-label`); its value is `"1"`/`"0"`.
    Check,
}

impl WidgetKind {
    pub(crate) fn from(verb: &str) -> Option<WidgetKind> {
        Some(match verb {
            "text" => WidgetKind::Text,
            "tail" => WidgetKind::Tail,
            "list" => WidgetKind::List,
            "gauge" => WidgetKind::Gauge,
            "bars" => WidgetKind::Bars,
            "histo" => WidgetKind::Histo,
            "spark" => WidgetKind::Spark,
            "chart" => WidgetKind::Chart,
            "table" => WidgetKind::Table,
            "tabs" => WidgetKind::Tabs,
            "block" => WidgetKind::Block,
            "frame" => WidgetKind::Frame,
            "input" => WidgetKind::Input,
            "select" => WidgetKind::Select,
            "filter" => WidgetKind::Filter,
            "facet" => WidgetKind::Facet,
            "slider" => WidgetKind::Slider,
            "check" => WidgetKind::Check,
            _ => return None,
        })
    }

    pub fn label(&self) -> &'static str {
        match self {
            WidgetKind::Text => "text",
            WidgetKind::Tail => "tail",
            WidgetKind::List => "list",
            WidgetKind::Gauge => "gauge",
            WidgetKind::Bars => "bars",
            WidgetKind::Histo => "histo",
            WidgetKind::Spark => "spark",
            WidgetKind::Chart => "chart",
            WidgetKind::Table => "table",
            WidgetKind::Tabs => "tabs",
            WidgetKind::Block => "block",
            WidgetKind::Frame => "frame",
            WidgetKind::Input => "input",
            WidgetKind::Select => "select",
            WidgetKind::Filter => "filter",
            WidgetKind::Facet => "facet",
            WidgetKind::Slider => "slider",
            WidgetKind::Check => "check",
        }
    }

    /// Whether this kind is an interactive control (value lives in the input
    /// registry and drives `apply`/`where … .name`), not a stream view.
    pub fn is_control(&self) -> bool {
        matches!(
            self,
            WidgetKind::Input | WidgetKind::Filter | WidgetKind::Facet | WidgetKind::Slider | WidgetKind::Check
        )
    }
}

/// A data source: reads stdin, then applies a query pipeline.
#[derive(Debug, Clone)]
pub struct Source {
    pub pipeline: Vec<QueryOp>,
}

#[derive(Debug, Clone)]
pub struct Widget {
    pub path: String,
    pub kind: WidgetKind,
    pub opts: BTreeMap<String, String>,
    pub source: Option<Source>,
    /// Optional search-key pipeline (`search .name { … }`), for a `select`
    /// widget: the fuzzy match runs against this derived key while the row still
    /// shows/emits the `source` display. fzf `--nth`, but pipeline-general (search
    /// a column, a lowercased key, an extracted field). `None` = search display.
    pub search: Option<Vec<QueryOp>>,
    /// Grid cell `(row, col)` set by a `grid` command; `None` = auto-stacked.
    pub grid: Option<(usize, usize)>,
    /// Grid span `(rows, cols)` from `grid -rowspan`/`-colspan` (`-span` = colspan);
    /// `(1, 1)` = a single cell. Lets a chart span multiple cells while gauges take one.
    pub span: (usize, usize),
}

/// A key binding: a control key that triggers a [`BindAction`] in the TUI.
/// Declared `bind C-<letter> <action>` — the arb-native way to drive the spec's
/// own state from the keyboard (set a control, quit), foundation for reactions.
#[derive(Debug, Clone)]
pub struct Bind {
    /// The raw control byte (e.g. Ctrl-U = 0x15).
    pub key: u8,
    pub action: BindAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindAction {
    /// Set an `input .name` widget's value — with `out { … apply .name }` this
    /// reshapes the live pipe on a keystroke (a keyboard-driven megafilter/map).
    SetInput { name: String, value: String },
    /// Quit the TUI.
    Quit,
    /// Ring the terminal bell (write 0x07 after the next draw).
    Beep,
    /// Flash a message in the status bar for a few seconds.
    Alert(String),
    /// Tint a widget's border/accent for a few seconds. `widget` is the path
    /// without the leading dot; `color` is a color name (green/red/…).
    Flash { widget: String, color: String },
    /// Run a shell command, fire-and-forget (never waits, never blocks the loop).
    Exec(String),
    /// Run several actions in order (block form `{ alert "x"; beep }`).
    Seq(Vec<BindAction>),
}

/// A mouse trigger for a `bind <…> ACTION` (Tk-style events). Extensible; only
/// `Click` (any button press) ships today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseTrigger {
    Click,
}

/// Parse a key spec to its raw byte. Accepts control-key forms (`C-u`, `c-u`,
/// `^u`) and the Tk named keys `<Enter>`/`<Esc>`/`<Tab>` and `<Key-X>` (a single
/// letter). Only these forms bind, so a bind never shadows plain filter typing.
pub fn parse_key(spec: &str) -> Option<u8> {
    match spec {
        "<Enter>" | "<Return>" => return Some(0x0d),
        "<Esc>" | "<Escape>" => return Some(0x1b),
        "<Tab>" => return Some(0x09),
        _ => {}
    }
    // `<Key-x>` -> the literal letter byte.
    if let Some(inner) = spec.strip_prefix("<Key-").and_then(|s| s.strip_suffix('>')) {
        let ch = inner.chars().next()?;
        if inner.chars().count() == 1 && ch.is_ascii_alphabetic() {
            return Some(ch as u8);
        }
        return None;
    }
    let letter = spec
        .strip_prefix("C-")
        .or_else(|| spec.strip_prefix("c-"))
        .or_else(|| spec.strip_prefix('^'))?;
    let ch = letter.chars().next()?;
    if letter.chars().count() != 1 || !ch.is_ascii_alphabetic() {
        return None;
    }
    Some((ch.to_ascii_uppercase() as u8) & 0x1f)
}

/// Parse a `Ns`/`Nms`/`Nm` duration literal (SPEC §3) into a `Duration`.
/// `ms` is matched before the bare `s` so `500ms` is not read as `500` seconds.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("ms") {
        return n.trim().parse::<u64>().ok().map(Duration::from_millis);
    }
    if let Some(n) = s.strip_suffix('s') {
        return n.trim().parse::<f64>().ok().map(Duration::from_secs_f64);
    }
    if let Some(n) = s.strip_suffix('m') {
        return n.trim().parse::<f64>().ok().map(|m| Duration::from_secs_f64(m * 60.0));
    }
    None
}

/// An event-driven reaction: when a stream line matches `pattern`, fire `action`.
/// Declared `expect /regex/ <action>` — the "react" half of Expect. Reuses
/// [`BindAction`], so a matching line can set a control or quit.
#[derive(Debug, Clone)]
pub struct Expect {
    pub pattern: Regex,
    pub action: BindAction,
}

/// An idle reaction (SPEC §13): when the input stream produces no new line for
/// `dur`, fire `action`. Latched once per idle span, re-armed on the next line.
/// Reuses [`BindAction`] exactly like [`Expect`], so `timeout 5s quit` works.
#[derive(Debug, Clone)]
pub struct Timeout {
    pub dur: Duration,
    pub action: BindAction,
}

/// Parse an action clause (`set .name VALUE…` | `quit`) shared by `bind` and
/// `expect`. `params` is the args AFTER the action verb.
fn parse_action(verb: &str, params: &[Arg]) -> Result<BindAction, String> {
    // The space-joined string params (used by alert/exec/set values).
    let joined = || {
        params
            .iter()
            .filter_map(Arg::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    };
    match verb {
        "quit" => Ok(BindAction::Quit),
        "beep" => Ok(BindAction::Beep),
        "alert" => Ok(BindAction::Alert(joined())),
        "exec" => {
            let cmd = joined();
            if cmd.is_empty() {
                return Err("exec: missing command".into());
            }
            Ok(BindAction::Exec(cmd))
        }
        "flash" => {
            let widget = params
                .first()
                .and_then(Arg::as_str)
                .ok_or("flash: missing widget (.name)")?;
            let widget = widget.strip_prefix('.').unwrap_or(widget).to_string();
            let color = params
                .get(1)
                .and_then(Arg::as_str)
                .unwrap_or("yellow")
                .to_string();
            Ok(BindAction::Flash { widget, color })
        }
        "set" => {
            let name = params
                .first()
                .and_then(Arg::as_str)
                .ok_or("set: missing input name (.name)")?;
            let name = name.strip_prefix('.').unwrap_or(name).to_string();
            let value = params[1..]
                .iter()
                .filter_map(Arg::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            Ok(BindAction::SetInput { name, value })
        }
        other => Err(format!(
            "unknown action `{other}` (set | quit | beep | alert | flash | exec)"
        )),
    }
}

/// Parse a block-form action body (`{ alert "x"; beep; flash .w red }`) into a
/// `Seq` — each top-level command is one action, run in order.
fn parse_action_block(cmds: &[Command]) -> Result<BindAction, String> {
    let actions = cmds
        .iter()
        .map(|c| parse_action(&c.name, &c.args))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(BindAction::Seq(actions))
}

/// Parse the action clause of a `bind`/`expect`: either a `{ … }` block (→ Seq)
/// or the single-line `verb params…` form. `rest` is the args after the key/regex.
fn parse_bind_or_expect_action(rest: &[Arg]) -> Result<BindAction, String> {
    match rest.first() {
        Some(Arg::Block(cmds)) => parse_action_block(cmds),
        Some(_) => {
            let verb = rest[0].as_str().unwrap_or("");
            parse_action(verb, &rest[1..])
        }
        None => Err("missing action (set | quit | beep | alert | flash | exec | { … })".into()),
    }
}

#[derive(Debug, Default)]
pub struct Spec {
    pub widgets: Vec<Widget>,
    /// Downstream output pipeline (`out { … }`): applied to the stream and
    /// written to stdout, so arb can *modify* a pipe, not just visualize it.
    pub out: Option<Vec<QueryOp>>,
    /// Key bindings (`bind C-<letter> <action>`).
    pub binds: Vec<Bind>,
    /// Event-driven reactions (`expect /re/ <action>`).
    pub expects: Vec<Expect>,
    /// Idle reactions (`timeout Ns <action>`).
    pub timeouts: Vec<Timeout>,
    /// Mouse reactions (`bind <Click> <action>`).
    pub mouse_binds: Vec<(MouseTrigger, BindAction)>,
    /// Terminal-resize reactions (`bind <Resize> <action>`).
    pub resize_binds: Vec<BindAction>,
    /// Input-source command (`spawn CMD` / `spawn { CMD }`, SPEC §7): arb
    /// launches CMD via `sh -c` and feeds its stdout into the stream in place of
    /// stdin (fire-and-forget; child detached, dies with arb). CLI `--run` wins
    /// if both are given. The interactive `send`/PTY react leg + `.ps.sel`
    /// selection widget are deferred. `None` = read stdin.
    pub spawn: Option<String>,
    /// Input-source file (`< FILE`, SPEC §7): read FILE as the stream in place
    /// of stdin. Folded into the producer as `cat -- FILE` in main. Mutually
    /// exclusive with `spawn` (and `--run`). `None` = read stdin.
    pub source_file: Option<String>,
    /// Poll source (`! CMD every Ns`, SPEC §7): re-run CMD via `sh -c` every
    /// interval, feeding each run's stdout into the stream. The interactive/serve
    /// loop runs in a background thread; the headless path runs CMD once (a
    /// reducer over an endless source can't terminate). `None` = read stdin.
    pub poll: Option<(String, Duration)>,
}

/// True if any single-stream input source (`spawn`/`< file`/`! poll`) is already
/// set — only one may feed the stream.
fn stream_source_set(s: &Spec) -> bool {
    s.spawn.is_some() || s.source_file.is_some() || s.poll.is_some()
}

/// Build a `Spec` from a parsed command tree.
pub fn build(cmds: &[Command]) -> Result<Spec, crate::err::SpecError> {
    let mut spec = Spec::default();
    build_into(&mut spec, cmds, 0)?;
    Ok(spec)
}

/// Resolve `apply .name` placeholders against live `input` widget values: each
/// `Apply(name)` is replaced by the query pipeline parsed from that input's
/// current text (empty/invalid → dropped). Called every render frame so a
/// before/after transform pane updates as the user edits the input.
pub fn resolve_pipeline(
    ops: &[QueryOp],
    inputs: &std::collections::HashMap<String, String>,
) -> Vec<QueryOp> {
    // Resolve a control name to its live numeric value (empty/non-numeric -> None)
    // or its raw string value (for `match`/`in .set` string predicates).
    let num = |n: &str| -> Option<f64> {
        inputs.get(n).and_then(|v| v.trim().parse::<f64>().ok())
    };
    let strv = |n: &str| -> Option<String> { inputs.get(n).cloned() };
    let mut out = Vec::with_capacity(ops.len());
    for op in ops {
        match op {
            QueryOp::Apply(name) => {
                if let Some(val) = inputs.get(name).filter(|v| !v.trim().is_empty()) {
                    // Parse the input text as a pipeline body (prefix `in;` so it
                    // is a valid source body) and splice its ops in.
                    if let Ok(cmds) = crate::parser::parse(&format!("in; {val}")) {
                        if let Ok(sub) = pipeline_from_body(&cmds) {
                            out.extend(sub);
                        }
                    }
                }
            }
            // `where lat < .th` (numeric) / `where match(.q)` / `where lvl in .lv`
            // (string). Numeric controls must all resolve or the filter is dropped
            // (unset threshold = no filter); string controls with an empty value
            // simply match everything, so they never force a drop.
            QueryOp::Where(e) => {
                let (mut nums, mut strs) = (Vec::new(), Vec::new());
                crate::expr::control_names_typed(e, &mut nums, &mut strs);
                if nums.is_empty() && strs.is_empty() {
                    out.push(op.clone());
                } else if nums.iter().all(|n| num(n).is_some()) {
                    out.push(QueryOp::Where(crate::expr::substitute_controls(e, &num, &strv)));
                }
                // else: a numeric control is unset -> drop this `where`.
            }
            // `map(x * .k)`: substitute resolvable controls; an unresolved one
            // stays a control (-> NaN) rather than dropping the transform.
            QueryOp::Map(e) => {
                out.push(QueryOp::Map(crate::expr::substitute_controls(e, &num, &strv)));
            }
            other => out.push(other.clone()),
        }
    }
    out
}

// ---- `import X as Y` namespacing ------------------------------------------
// A module built into a fresh sub-Spec has its render-time NAME references
// (widget paths, `apply`, control refs, `set`/`flash` targets) prefixed with the
// alias, then merged in. Intra-module by-path refs (source/search/grid/configure)
// already resolved during the fresh build, so only these string names remain.

/// Namespace a control/input name: `cpu` -> `g.cpu`.
fn prefix_name(name: &str, ns: &str) -> String {
    format!("{ns}.{name}")
}

/// Namespace `apply`/`where`/`map`/`calc` references in a query pipeline.
fn prefix_pipeline(ops: &mut [QueryOp], ns: &str) {
    for op in ops {
        match op {
            QueryOp::Apply(n) => *n = prefix_name(n, ns),
            QueryOp::Where(e) | QueryOp::Map(e) | QueryOp::Calc(e) => {
                crate::expr::prefix_controls(e, ns)
            }
            _ => {}
        }
    }
}

/// Namespace the widget/control names an action targets.
fn prefix_action(a: &mut BindAction, ns: &str) {
    match a {
        BindAction::SetInput { name, .. } => *name = prefix_name(name, ns),
        BindAction::Flash { widget, .. } => *widget = prefix_name(widget, ns),
        BindAction::Seq(v) => v.iter_mut().for_each(|x| prefix_action(x, ns)),
        _ => {}
    }
}

/// Prefix every render-time name in a freshly-built module Spec with `ns`.
fn prefix_spec(sub: &mut Spec, ns: &str) {
    for w in &mut sub.widgets {
        w.path = format!(".{ns}.{}", w.path.trim_start_matches('.'));
        if let Some(s) = &mut w.source {
            prefix_pipeline(&mut s.pipeline, ns);
        }
        if let Some(p) = &mut w.search {
            prefix_pipeline(p, ns);
        }
    }
    for b in &mut sub.binds {
        prefix_action(&mut b.action, ns);
    }
    for e in &mut sub.expects {
        prefix_action(&mut e.action, ns);
    }
    for t in &mut sub.timeouts {
        prefix_action(&mut t.action, ns);
    }
    for (_, a) in &mut sub.mouse_binds {
        prefix_action(a, ns);
    }
    for a in &mut sub.resize_binds {
        prefix_action(a, ns);
    }
    if let Some(out) = &mut sub.out {
        prefix_pipeline(out, ns);
    }
}

/// Merge a namespaced module Spec into `dst`.
fn merge_spec(dst: &mut Spec, src: Spec, ns: &str) -> Result<(), String> {
    dst.widgets.extend(src.widgets);
    dst.binds.extend(src.binds);
    dst.expects.extend(src.expects);
    dst.timeouts.extend(src.timeouts);
    dst.mouse_binds.extend(src.mouse_binds);
    dst.resize_binds.extend(src.resize_binds);
    if let Some(out) = src.out {
        if dst.out.is_some() {
            return Err(format!("import: `{ns}` defines `out`, but one is already set"));
        }
        dst.out = Some(out);
    }
    if let Some(sp) = src.spawn {
        if stream_source_set(dst) {
            return Err(format!("import: `{ns}` defines a spawn source, but one is already set"));
        }
        dst.spawn = Some(sp);
    }
    if let Some(f) = src.source_file {
        if stream_source_set(dst) {
            return Err(format!("import: `{ns}` defines a `<` file source, but one is already set"));
        }
        dst.source_file = Some(f);
    }
    if let Some(p) = src.poll {
        if stream_source_set(dst) {
            return Err(format!("import: `{ns}` defines a `!` poll source, but one is already set"));
        }
        dst.poll = Some(p);
    }
    Ok(())
}

/// Process `cmds` into `spec`. `import NAME` resolves and inlines a module
/// (stdlib preset or user file); `depth` guards against import cycles.
fn build_into(spec: &mut Spec, cmds: &[Command], depth: usize) -> Result<(), crate::err::SpecError> {
    if depth > 16 {
        return Err("import: module nesting too deep (cycle?)".into());
    }
    for c in cmds {
        // Process one command; a returned error with no span yet is anchored to
        // this command's verb (its char offset) so the LSP points at the line.
        let res: Result<(), crate::err::SpecError> = (|| -> Result<(), crate::err::SpecError> {
        if c.name == "import" {
            let name = c
                .args
                .first()
                .and_then(Arg::as_str)
                .ok_or("import: missing module name")?;
            // `import X as Y`: build the module into a fresh sub-Spec (so its
            // intra-module by-path refs resolve), namespace every render-time
            // name with `Y`, then merge — so `.Y.foo` and its controls resolve.
            if c.args.get(1).and_then(Arg::as_str) == Some("as") {
                let alias = c
                    .args
                    .get(2)
                    .and_then(Arg::as_str)
                    .ok_or("import: `as` requires an alias (import X as Y)")?;
                if alias.is_empty() || alias.contains(['.', '/', ' ']) {
                    return Err(format!("import: invalid namespace alias `{alias}`").into());
                }
                let src = resolve_module(name)?;
                let sub_cmds = crate::parser::parse(&src)?;
                let mut sub = Spec::default();
                build_into(&mut sub, &sub_cmds, depth + 1)?;
                prefix_spec(&mut sub, alias);
                merge_spec(spec, sub, alias)?;
            } else {
                let src = resolve_module(name)?;
                let sub = crate::parser::parse(&src)?;
                build_into(spec, &sub, depth + 1)?;
            }
        } else if let Some(kind) = WidgetKind::from(&c.name) {
            let path = c
                .args
                .first()
                .and_then(Arg::as_str)
                .ok_or_else(|| format!("{}: missing widget path", c.name))?;
            if !path.starts_with('.') {
                return Err(format!(
                    "{}: widget path must start with '.', got `{path}`",
                    c.name
                )
                .into());
            }
            spec.widgets.push(Widget {
                path: path.to_string(),
                kind,
                opts: parse_opts(&c.args[1..]),
                source: None,
                search: None,
                grid: None,
                span: (1, 1),
            });
        } else if c.name == "source" {
            let path = c
                .args
                .first()
                .and_then(Arg::as_str)
                .ok_or("source: missing path")?;
            let body = match c.args.get(1) {
                Some(Arg::Block(b)) => b,
                _ => return Err("source: expected `{ body }`".into()),
            };
            let pipeline = pipeline_from_body(body)?;
            set_source(spec, path, Source { pipeline })?;
        } else if c.name == "search" {
            // `search .name { in; … }` — a select widget's fuzzy-match key
            // pipeline (fzf --nth). The row still shows/emits its `source`.
            let path = c
                .args
                .first()
                .and_then(Arg::as_str)
                .ok_or("search: missing path")?;
            let body = match c.args.get(1) {
                Some(Arg::Block(b)) => b,
                _ => return Err("search: expected `{ body }`".into()),
            };
            let pipeline = pipeline_from_body(body)?;
            set_search(spec, path, pipeline)?;
        } else if c.name == "bind" {
            // `bind KEY ACTION` — KEY is a control key (`C-<letter>`, `<Enter>`…),
            // a mouse event (`<Click>`), or `<Resize>`; ACTION is a single verb or
            // a `{ … }` block.
            let keyspec = c
                .args
                .first()
                .and_then(Arg::as_str)
                .ok_or("bind: missing key (e.g. C-u, <Click>)")?;
            let action =
                parse_bind_or_expect_action(&c.args[1..]).map_err(|e| format!("bind: {e}"))?;
            match keyspec {
                "<Click>" | "<Button-1>" => spec.mouse_binds.push((MouseTrigger::Click, action)),
                "<Resize>" | "<Configure>" => spec.resize_binds.push(action),
                _ => {
                    let key = parse_key(keyspec).ok_or_else(|| {
                        format!("bind: `{keyspec}` is not a bindable key (use C-<letter>, <Enter>/<Esc>/<Tab>/<Key-x>, <Click>, <Resize>)")
                    })?;
                    spec.binds.push(Bind { key, action });
                }
            }
        } else if c.name == "expect" {
            match c.args.first() {
                // Block form `expect { /re/ ACTION; /re2/ ACTION }` (Tcl Expect's
                // multi-pattern block): each inner command is one clause — its
                // NAME is the /regex/, its args are the ACTION. Emit one Expect
                // per clause; all fire live per line (tui.rs loops every entry).
                Some(Arg::Block(clauses)) => {
                    for clause in clauses {
                        let pattern =
                            compile_regex(&clause.name).map_err(|e| format!("expect: {e}"))?;
                        let action = parse_bind_or_expect_action(&clause.args)
                            .map_err(|e| format!("expect: {e}"))?;
                        spec.expects.push(Expect { pattern, action });
                    }
                }
                // Single-clause form `expect /regex/ ACTION` — ACTION as for `bind`.
                _ => {
                    let pattern = regex_arg(c)?;
                    let action = parse_bind_or_expect_action(&c.args[1..])
                        .map_err(|e| format!("expect: {e}"))?;
                    spec.expects.push(Expect { pattern, action });
                }
            }
        } else if c.name == "timeout" {
            // `timeout Ns ACTION` — fire ACTION when the stream goes idle for Ns.
            let dspec = c
                .args
                .first()
                .and_then(Arg::as_str)
                .ok_or("timeout: missing duration (e.g. 5s, 500ms)")?;
            let dur = parse_duration(dspec)
                .ok_or_else(|| format!("timeout: `{dspec}` is not a duration (use Ns/Nms/Nm)"))?;
            let action =
                parse_bind_or_expect_action(&c.args[1..]).map_err(|e| format!("timeout: {e}"))?;
            spec.timeouts.push(Timeout { dur, action });
        } else if c.name == "out" {
            let body = match c.args.first() {
                Some(Arg::Block(b)) => b,
                _ => return Err("out: expected `{ body }`".into()),
            };
            spec.out = Some(pipeline_from_body(body)?);
        } else if c.name == "spawn" {
            // `spawn CMD…` or `spawn { CMD }`: launch CMD via `sh -c` and use its
            // stdout as the stream (input source, in place of stdin).
            let cmd = match c.args.first() {
                Some(Arg::Block(b)) => block_to_shell(b),
                _ => c.args.iter().filter_map(Arg::as_str).collect::<Vec<_>>().join(" "),
            };
            if cmd.trim().is_empty() {
                return Err("spawn: missing command".into());
            }
            if stream_source_set(spec) {
                return Err("spawn: a stream source is already declared".into());
            }
            spec.spawn = Some(cmd);
        } else if c.name == "<" {
            // `< FILE`: read FILE as the stream (input source, in place of stdin).
            // An unquoted absolute path (`/var/log/x`) is truncated by the lexer's
            // `/…/` regex branch — quote it (`< "/var/log/x"`).
            let file = c
                .args
                .first()
                .and_then(Arg::as_str)
                .filter(|f| !f.trim().is_empty())
                .ok_or("`<`: missing file path")?;
            if stream_source_set(spec) {
                return Err("`<`: a stream source is already declared".into());
            }
            spec.source_file = Some(file.to_string());
        } else if c.name == "!" {
            // `! CMD every Ns`: re-run CMD every interval. CMD may be bare words
            // or a `{ … }` block; the interval follows the `every` keyword.
            let evy = c
                .args
                .iter()
                .position(|a| a.as_str() == Some("every"))
                .ok_or("`!` source: expected `CMD every Ns`")?;
            let dspec = c
                .args
                .get(evy + 1)
                .and_then(Arg::as_str)
                .ok_or("`!` source: `every` needs a duration (e.g. 1s, 500ms)")?;
            let dur = parse_duration(dspec).ok_or_else(|| {
                format!("`!` source: `{dspec}` is not a duration (use Ns/Nms/Nm)")
            })?;
            let cmd = match c.args.first() {
                Some(Arg::Block(b)) => block_to_shell(b),
                _ => c.args[..evy].iter().filter_map(Arg::as_str).collect::<Vec<_>>().join(" "),
            };
            if cmd.trim().is_empty() {
                return Err("`!` source: missing command".into());
            }
            if stream_source_set(spec) {
                return Err("`!`: a stream source is already declared".into());
            }
            spec.poll = Some((cmd, dur));
        } else if c.name == "grid" {
            let path = c
                .args
                .first()
                .and_then(Arg::as_str)
                .ok_or("grid: missing path")?;
            let o = parse_opts(&c.args[1..]);
            let opt = |k: &str, d: usize| {
                o.get(k).and_then(|s: &String| s.parse::<usize>().ok()).unwrap_or(d)
            };
            // `-span` is a colspan shorthand; `-rowspan`/`-colspan` are explicit.
            let colspan = opt("colspan", opt("span", 1)).max(1);
            let rowspan = opt("rowspan", 1).max(1);
            set_grid(spec, path, (opt("row", 0), opt("col", 0)), (rowspan, colspan))?;
        } else if c.name.starts_with('.') {
            if c.args.first().and_then(Arg::as_str) == Some("<-")
                && c.args.get(1).and_then(Arg::as_str) == Some("in")
            {
                // `.path <- in` bind shorthand (empty pipeline).
                let path = c.name.clone();
                set_source(spec, &path, Source { pipeline: vec![] })?;
            } else if c.args.first().and_then(Arg::as_str) == Some("configure") {
                // `.path configure -max 200`: merge new opts into the target
                // widget (build-time; later keys win). Runtime reconfigure via a
                // bind/expect mutating live opts is a separate, larger feature.
                let opts = parse_opts(&c.args[1..]);
                configure_widget(spec, &c.name, opts)?;
            }
        }
        // Unknown verbs are ignored.
        Ok(())
        })();
        res.map_err(|e| e.or_span(c.pos, c.name.chars().count()))?;
    }
    Ok(())
}

/// Resolve a module name to its source: a local `NAME.arb`, then
/// `~/.arb/lib/NAME.arb`, then a bundled stdlib preset.
fn resolve_module(name: &str) -> Result<String, String> {
    // A path-like or already-suffixed name (`import "./lib.arb"`, `import
    // "/abs/lib.arb"`) is read verbatim — don't double up the extension.
    if name.ends_with(".arb") || name.contains('/') {
        if let Ok(s) = std::fs::read_to_string(name) {
            return Ok(s);
        }
    }
    let local = format!("{name}.arb");
    if let Ok(s) = std::fs::read_to_string(&local) {
        return Ok(s);
    }
    if let Some(dir) = lib_dir() {
        if let Ok(s) = std::fs::read_to_string(dir.join(format!("{name}.arb"))) {
            return Ok(s);
        }
    }
    // Installed registry packages (`~/.arb/pkg`) — the SPEC §17 `pkg` tier.
    if let Some(dir) = crate::pkg::pkg_dir() {
        if let Some(s) = crate::pkg::read_pkg_module(&dir, name) {
            return Ok(s);
        }
    }
    bundled_module(name)
        .map(str::to_string)
        .ok_or_else(|| format!("import: module `{name}` not found"))
}

/// Names of the bundled stdlib presets.
pub const STDLIB_NAMES: &[&str] = &[
    "nums", "logs", "http", "json", "table", "top", "docker", "k8s", "nginx", "git", "systemd",
    "redis", "postgres", "mysql", "mongodb", "kafka", "prometheus", "elasticsearch", "rabbitmq",
    "apache", "haproxy", "journalctl", "dmesg", "ps", "htop", "iostat", "vmstat", "ss", "dig",
    "curl", "gh", "terraform", "aws", "gcloud", "azure", "ansible", "consul", "vault", "etcd",
    "nomad", "envoy", "memcached", "varnish", "pgbouncer", "celery", "sidekiq", "gunicorn",
    "supervisor", "fail2ban", "iptables", "conntrack", "sar",
    "nats",
    "tomcat",
    "puma",
    "lighttpd",
    "helm",
    "podman",
    "containerd",
    "crictl",
    "istioctl",
    "linkerd",
    "cilium",
    "argocd",
    "fluxcd",
    "velero",
    "kustomize",
    "skopeo",
    "buildah",
    "stern",
    "kubectx",
    "jenkins",
    "gitlabrunner",
    "circleci",
    "drone",
    "buildkite",
    "concourse",
    "woodpecker",
    "spinnaker",
    "tekton",
    "teamcity",
    "packer",
    "vagrant",
    "pulumi",
    "chef",
    "puppet",
    "salt",
    "cloudformation",
    "cdk",
    "doctl",
    "hcloud",
    "fly",
    "heroku",
    "linode",
    "vultr",
    "scaleway",
    "cassandra",
    "scylla",
    "cockroachdb",
    "clickhouse",
    "influxdb",
    "neo4j",
    "couchdb",
    "dynamodb",
    "mariadb",
    "duckdb",
    "sqlite",
    "mssql",
    "timescaledb",
    "arangodb",
    "tidb",
    "victoriametrics",
    "questdb",
    "riak",
    "dgraph",
    "opensearch",
    "grafana",
    "loki",
    "tempo",
    "jaeger",
    "zipkin",
    "datadog",
    "newrelic",
    "sentry",
    "statsd",
    "telegraf",
    "collectd",
    "netdata",
    "zabbix",
    "nagios",
    "icinga",
    "pulsar",
    "activemq",
    "nsq",
    "beanstalkd",
    "caddy",
    "traefik",
    "uwsgi",
    "phpfpm",
    "tcpdump",
    "tshark",
    "nmap",
    "netstat",
    "iftop",
    "nethogs",
    "vnstat",
    "mtr",
    "zfs",
    "btrfs",
    "smartctl",
    "nvme",
    "rclone",
];

fn bundled_module(name: &str) -> Option<&'static str> {
    Some(match name {
        "nums" => include_str!("../stdlib/nums.arb"),
        "logs" => include_str!("../stdlib/logs.arb"),
        "http" => include_str!("../stdlib/http.arb"),
        "json" => include_str!("../stdlib/json.arb"),
        "table" => include_str!("../stdlib/table.arb"),
        "top" => include_str!("../stdlib/top.arb"),
        "docker" => include_str!("../stdlib/docker.arb"),
        "k8s" => include_str!("../stdlib/k8s.arb"),
        "nginx" => include_str!("../stdlib/nginx.arb"),
        "git" => include_str!("../stdlib/git.arb"),
        "systemd" => include_str!("../stdlib/systemd.arb"),
        "redis" => include_str!("../stdlib/redis.arb"),
        "postgres" => include_str!("../stdlib/postgres.arb"),
        "mysql" => include_str!("../stdlib/mysql.arb"),
        "mongodb" => include_str!("../stdlib/mongodb.arb"),
        "kafka" => include_str!("../stdlib/kafka.arb"),
        "prometheus" => include_str!("../stdlib/prometheus.arb"),
        "elasticsearch" => include_str!("../stdlib/elasticsearch.arb"),
        "rabbitmq" => include_str!("../stdlib/rabbitmq.arb"),
        "apache" => include_str!("../stdlib/apache.arb"),
        "haproxy" => include_str!("../stdlib/haproxy.arb"),
        "journalctl" => include_str!("../stdlib/journalctl.arb"),
        "dmesg" => include_str!("../stdlib/dmesg.arb"),
        "ps" => include_str!("../stdlib/ps.arb"),
        "htop" => include_str!("../stdlib/htop.arb"),
        "iostat" => include_str!("../stdlib/iostat.arb"),
        "vmstat" => include_str!("../stdlib/vmstat.arb"),
        "ss" => include_str!("../stdlib/ss.arb"),
        "dig" => include_str!("../stdlib/dig.arb"),
        "curl" => include_str!("../stdlib/curl.arb"),
        "gh" => include_str!("../stdlib/gh.arb"),
        "terraform" => include_str!("../stdlib/terraform.arb"),
        "aws" => include_str!("../stdlib/aws.arb"),
        "gcloud" => include_str!("../stdlib/gcloud.arb"),
        "azure" => include_str!("../stdlib/azure.arb"),
        "ansible" => include_str!("../stdlib/ansible.arb"),
        "consul" => include_str!("../stdlib/consul.arb"),
        "vault" => include_str!("../stdlib/vault.arb"),
        "etcd" => include_str!("../stdlib/etcd.arb"),
        "nomad" => include_str!("../stdlib/nomad.arb"),
        "envoy" => include_str!("../stdlib/envoy.arb"),
        "memcached" => include_str!("../stdlib/memcached.arb"),
        "varnish" => include_str!("../stdlib/varnish.arb"),
        "pgbouncer" => include_str!("../stdlib/pgbouncer.arb"),
        "celery" => include_str!("../stdlib/celery.arb"),
        "sidekiq" => include_str!("../stdlib/sidekiq.arb"),
        "gunicorn" => include_str!("../stdlib/gunicorn.arb"),
        "supervisor" => include_str!("../stdlib/supervisor.arb"),
        "fail2ban" => include_str!("../stdlib/fail2ban.arb"),
        "iptables" => include_str!("../stdlib/iptables.arb"),
        "conntrack" => include_str!("../stdlib/conntrack.arb"),
        "sar" => include_str!("../stdlib/sar.arb"),
        "nats" => include_str!("../stdlib/nats.arb"),
        "tomcat" => include_str!("../stdlib/tomcat.arb"),
        "puma" => include_str!("../stdlib/puma.arb"),
        "lighttpd" => include_str!("../stdlib/lighttpd.arb"),
        "helm" => include_str!("../stdlib/helm.arb"),
        "podman" => include_str!("../stdlib/podman.arb"),
        "containerd" => include_str!("../stdlib/containerd.arb"),
        "crictl" => include_str!("../stdlib/crictl.arb"),
        "istioctl" => include_str!("../stdlib/istioctl.arb"),
        "linkerd" => include_str!("../stdlib/linkerd.arb"),
        "cilium" => include_str!("../stdlib/cilium.arb"),
        "argocd" => include_str!("../stdlib/argocd.arb"),
        "fluxcd" => include_str!("../stdlib/fluxcd.arb"),
        "velero" => include_str!("../stdlib/velero.arb"),
        "kustomize" => include_str!("../stdlib/kustomize.arb"),
        "skopeo" => include_str!("../stdlib/skopeo.arb"),
        "buildah" => include_str!("../stdlib/buildah.arb"),
        "stern" => include_str!("../stdlib/stern.arb"),
        "kubectx" => include_str!("../stdlib/kubectx.arb"),
        "jenkins" => include_str!("../stdlib/jenkins.arb"),
        "gitlabrunner" => include_str!("../stdlib/gitlabrunner.arb"),
        "circleci" => include_str!("../stdlib/circleci.arb"),
        "drone" => include_str!("../stdlib/drone.arb"),
        "buildkite" => include_str!("../stdlib/buildkite.arb"),
        "concourse" => include_str!("../stdlib/concourse.arb"),
        "woodpecker" => include_str!("../stdlib/woodpecker.arb"),
        "spinnaker" => include_str!("../stdlib/spinnaker.arb"),
        "tekton" => include_str!("../stdlib/tekton.arb"),
        "teamcity" => include_str!("../stdlib/teamcity.arb"),
        "packer" => include_str!("../stdlib/packer.arb"),
        "vagrant" => include_str!("../stdlib/vagrant.arb"),
        "pulumi" => include_str!("../stdlib/pulumi.arb"),
        "chef" => include_str!("../stdlib/chef.arb"),
        "puppet" => include_str!("../stdlib/puppet.arb"),
        "salt" => include_str!("../stdlib/salt.arb"),
        "cloudformation" => include_str!("../stdlib/cloudformation.arb"),
        "cdk" => include_str!("../stdlib/cdk.arb"),
        "doctl" => include_str!("../stdlib/doctl.arb"),
        "hcloud" => include_str!("../stdlib/hcloud.arb"),
        "fly" => include_str!("../stdlib/fly.arb"),
        "heroku" => include_str!("../stdlib/heroku.arb"),
        "linode" => include_str!("../stdlib/linode.arb"),
        "vultr" => include_str!("../stdlib/vultr.arb"),
        "scaleway" => include_str!("../stdlib/scaleway.arb"),
        "cassandra" => include_str!("../stdlib/cassandra.arb"),
        "scylla" => include_str!("../stdlib/scylla.arb"),
        "cockroachdb" => include_str!("../stdlib/cockroachdb.arb"),
        "clickhouse" => include_str!("../stdlib/clickhouse.arb"),
        "influxdb" => include_str!("../stdlib/influxdb.arb"),
        "neo4j" => include_str!("../stdlib/neo4j.arb"),
        "couchdb" => include_str!("../stdlib/couchdb.arb"),
        "dynamodb" => include_str!("../stdlib/dynamodb.arb"),
        "mariadb" => include_str!("../stdlib/mariadb.arb"),
        "duckdb" => include_str!("../stdlib/duckdb.arb"),
        "sqlite" => include_str!("../stdlib/sqlite.arb"),
        "mssql" => include_str!("../stdlib/mssql.arb"),
        "timescaledb" => include_str!("../stdlib/timescaledb.arb"),
        "arangodb" => include_str!("../stdlib/arangodb.arb"),
        "tidb" => include_str!("../stdlib/tidb.arb"),
        "victoriametrics" => include_str!("../stdlib/victoriametrics.arb"),
        "questdb" => include_str!("../stdlib/questdb.arb"),
        "riak" => include_str!("../stdlib/riak.arb"),
        "dgraph" => include_str!("../stdlib/dgraph.arb"),
        "opensearch" => include_str!("../stdlib/opensearch.arb"),
        "grafana" => include_str!("../stdlib/grafana.arb"),
        "loki" => include_str!("../stdlib/loki.arb"),
        "tempo" => include_str!("../stdlib/tempo.arb"),
        "jaeger" => include_str!("../stdlib/jaeger.arb"),
        "zipkin" => include_str!("../stdlib/zipkin.arb"),
        "datadog" => include_str!("../stdlib/datadog.arb"),
        "newrelic" => include_str!("../stdlib/newrelic.arb"),
        "sentry" => include_str!("../stdlib/sentry.arb"),
        "statsd" => include_str!("../stdlib/statsd.arb"),
        "telegraf" => include_str!("../stdlib/telegraf.arb"),
        "collectd" => include_str!("../stdlib/collectd.arb"),
        "netdata" => include_str!("../stdlib/netdata.arb"),
        "zabbix" => include_str!("../stdlib/zabbix.arb"),
        "nagios" => include_str!("../stdlib/nagios.arb"),
        "icinga" => include_str!("../stdlib/icinga.arb"),
        "pulsar" => include_str!("../stdlib/pulsar.arb"),
        "activemq" => include_str!("../stdlib/activemq.arb"),
        "nsq" => include_str!("../stdlib/nsq.arb"),
        "beanstalkd" => include_str!("../stdlib/beanstalkd.arb"),
        "caddy" => include_str!("../stdlib/caddy.arb"),
        "traefik" => include_str!("../stdlib/traefik.arb"),
        "uwsgi" => include_str!("../stdlib/uwsgi.arb"),
        "phpfpm" => include_str!("../stdlib/phpfpm.arb"),
        "tcpdump" => include_str!("../stdlib/tcpdump.arb"),
        "tshark" => include_str!("../stdlib/tshark.arb"),
        "nmap" => include_str!("../stdlib/nmap.arb"),
        "netstat" => include_str!("../stdlib/netstat.arb"),
        "iftop" => include_str!("../stdlib/iftop.arb"),
        "nethogs" => include_str!("../stdlib/nethogs.arb"),
        "vnstat" => include_str!("../stdlib/vnstat.arb"),
        "mtr" => include_str!("../stdlib/mtr.arb"),
        "zfs" => include_str!("../stdlib/zfs.arb"),
        "btrfs" => include_str!("../stdlib/btrfs.arb"),
        "smartctl" => include_str!("../stdlib/smartctl.arb"),
        "nvme" => include_str!("../stdlib/nvme.arb"),
        "rclone" => include_str!("../stdlib/rclone.arb"),
        _ => return None,
    })
}

/// List available presets as `(name, description)` — bundled stdlib plus any
/// user modules in `~/.arb/lib`. The description is the preset's first `#` line.
pub fn list_presets() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = STDLIB_NAMES
        .iter()
        .map(|n| {
            (
                n.to_string(),
                first_comment(bundled_module(n).unwrap_or("")),
            )
        })
        .collect();
    if let Some(dir) = lib_dir() {
        out.extend(list_user_presets(&dir));
    }
    out
}

fn first_comment(src: &str) -> String {
    src.lines()
        .find_map(|l| l.trim().strip_prefix('#'))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Canonical accent color for a widget's `-color NAME` opt, as a `#rrggbb` hex —
/// the single source of truth shared by the TUI (parsed to an RGB color) and the
/// web dashboard (used directly in CSS/SVG). Unknown/absent names default to cyan.
/// Parse a scalar control bound (`-min`/`-max`/`-step`): a plain number, a
/// duration (`5s`/`500ms`/`2m`/`1h`) as seconds, or a size (`1kb`/`4mb`/`2gb`)
/// as bytes. Unparseable -> 0.
pub fn parse_scalar(s: &str) -> f64 {
    let s = s.trim();
    if let Some(d) = parse_duration(s) {
        return d.as_secs_f64();
    }
    let lower = s.to_ascii_lowercase();
    for (suf, mult) in [("kb", 1024.0), ("mb", 1048576.0), ("gb", 1073741824.0)] {
        if let Some(n) = lower.strip_suffix(suf) {
            if let Ok(v) = n.trim().parse::<f64>() {
                return v * mult;
            }
        }
    }
    s.parse::<f64>().unwrap_or(0.0)
}

pub fn color_hex(name: Option<&str>) -> &'static str {
    match name.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("green") => "#00e676",
        Some("red") => "#ff5252",
        Some("yellow") => "#ffd740",
        Some("orange") => "#ffab40",
        Some("magenta") | Some("pink") => "#e040fb",
        Some("blue") => "#40c4ff",
        Some("white") => "#e0e0e0",
        Some("gray") | Some("grey") => "#9e9e9e",
        _ => "#00e5ff", // cyan (default)
    }
}

/// The user preset library directory: `$ARB_LIB` when set (used by tests and for
/// relocating the library), else `$HOME/.arb/lib`. `None` if neither is set.
pub fn lib_dir() -> Option<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("ARB_LIB") {
        return Some(std::path::PathBuf::from(d));
    }
    std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".arb/lib"))
}

/// Install a spec `src` into `dir` as `NAME.arb` — the shareable-package unit.
/// The spec is validated (parse + build) first; an invalid package is rejected
/// so the library only ever holds runnable dashboards. Returns the written path.
pub fn install_preset(dir: &std::path::Path, name: &str, src: &str) -> Result<std::path::PathBuf, String> {
    if name.is_empty() || name.contains(['/', '\\', '.']) {
        return Err(format!("install: invalid preset name `{name}`"));
    }
    let cmds = crate::parser::parse(src).map_err(|e| format!("install: invalid spec: {e}"))?;
    build(&cmds).map_err(|e| format!("install: invalid spec: {e}"))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("install: {e}"))?;
    let path = dir.join(format!("{name}.arb"));
    std::fs::write(&path, src).map_err(|e| format!("install: {e}"))?;
    Ok(path)
}

/// Remove `NAME.arb` from `dir`. Returns whether a preset was actually removed.
pub fn uninstall_preset(dir: &std::path::Path, name: &str) -> std::io::Result<bool> {
    let path = dir.join(format!("{name}.arb"));
    if path.exists() {
        std::fs::remove_file(&path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// List installed user presets in `dir` as `(name, description)`, sorted by name.
/// The description is the preset's first `#` comment line.
pub fn list_user_presets(dir: &std::path::Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) == Some("arb") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    let src = std::fs::read_to_string(&path).unwrap_or_default();
                    out.push((stem.to_string(), first_comment(&src)));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Collect `-flag value` pairs into an options map.
fn parse_opts(args: &[Arg]) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(flag) = args[i].as_str().and_then(|w| w.strip_prefix('-')) {
            // A block value (`-tabs {a b}`) becomes a comma-joined word list:
            // each top-level command contributes its name plus its word args
            // (Tcl-flavored `{a b}` is one command `a b`), so `{a b}` -> "a,b".
            let val = match args.get(i + 1) {
                Some(Arg::Block(cmds)) => {
                    let mut words = Vec::new();
                    for c in cmds {
                        words.push(c.name.clone());
                        words.extend(c.args.iter().filter_map(|a| a.as_str().map(str::to_string)));
                    }
                    words.join(",")
                }
                Some(a) => a.as_str().unwrap_or("").to_string(),
                None => String::new(),
            };
            m.insert(flag.to_string(), val);
            i += 2;
        } else {
            i += 1;
        }
    }
    m
}

/// Compile a `source { … }` body into a query pipeline. Must start with `in`.
fn pipeline_from_body(cmds: &[Command]) -> Result<Vec<QueryOp>, crate::err::SpecError> {
    let mut ops = Vec::new();
    let mut saw_in = false;
    for c in cmds {
        // Process one op; on error, anchor the diagnostic to THIS (nested) verb's
        // source position, not the outer `source`/`out` command (the IIFE lets the
        // arms' `return Err` unwind to the per-command span attach below).
        let step = (|| -> Result<(), crate::err::SpecError> {
        // jq front-end: a body command whose first token is a jq literal (starts
        // with `.`, or a `select(…)`/`map(…)` stage) is translated to arb ops.
        // Inside a `source` body a leading `.` is unambiguous — widget-path decls
        // never appear here. The whole command text (verb + args) is reconstructed
        // so the jq `|` pipe, which is not an arb separator, can be split by `jq`.
        if c.name.starts_with('.')
            || c.name.starts_with("select(")
            || c.name.starts_with("map(")
        {
            let mut parts = vec![c.name.clone()];
            parts.extend(c.args.iter().filter_map(Arg::as_str).map(str::to_string));
            let jq_ops = crate::jq::translate(&parts.join(" "))?;
            ops.extend(jq_ops);
            return Ok(());
        }
        // xpath front-end: a body command whose first token is an xpath literal
        // (`/…`, `//…`, or `@…`) translates to arb's Find/Attr/Text ops. Disjoint
        // from the jq test above (`.`/`select(`/`map(`) and from native verbs
        // (alnum), so the three coexist in one body unambiguously.
        if c.name.starts_with('/') || c.name.starts_with('@') {
            let mut parts = vec![c.name.clone()];
            parts.extend(c.args.iter().filter_map(Arg::as_str).map(str::to_string));
            let xp_ops = crate::xpath::translate(&parts.join(" "))?;
            ops.extend(xp_ops);
            return Ok(());
        }
        match c.name.as_str() {
            "in" | "in.json" | "in.html" | "in.xml" | "in.logfmt" => saw_in = true,
            "in.csv" => {
                saw_in = true;
                ops.push(QueryOp::Csv);
            }
            "in.tsv" => {
                saw_in = true;
                ops.push(QueryOp::Tsv);
            }
            "in.yaml" | "in.yml" => {
                saw_in = true;
                ops.push(QueryOp::Yaml);
            }
            "in.toml" => {
                saw_in = true;
                ops.push(QueryOp::Toml);
            }
            "sel" => {
                let words: Vec<&str> = c.args.iter().filter_map(Arg::as_str).collect();
                let mut css_parts = Vec::new();
                let mut attr = None;
                let mut i = 0;
                while i < words.len() {
                    if words[i] == "-attr" {
                        attr = words.get(i + 1).map(|s| s.to_string());
                        i += 2;
                    } else {
                        css_parts.push(words[i]);
                        i += 1;
                    }
                }
                let css = css_parts.join(" ");
                if css.trim().is_empty() {
                    return Err("sel: expected a CSS selector".into());
                }
                ops.push(QueryOp::Sel { css, attr });
            }
            "find" => {
                let css = c
                    .args
                    .iter()
                    .filter_map(Arg::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                if css.trim().is_empty() {
                    return Err("find: expected a tag/selector".into());
                }
                ops.push(QueryOp::Find(css));
            }
            "attr" => {
                let name = str_arg(c);
                if name.is_empty() {
                    return Err("attr: expected an attribute name".into());
                }
                ops.push(QueryOp::Attr(name));
            }
            "text" => ops.push(QueryOp::Text),
            "match" | "grep" => ops.push(QueryOp::Match(regex_arg(c)?)),
            "reject" | "grepv" => ops.push(QueryOp::Reject(regex_arg(c)?)),
            "field" => ops.push(QueryOp::Field(field_sel(&c.args)?)),
            "fields" => {
                let cols: Vec<usize> = c
                    .args
                    .iter()
                    .filter_map(Arg::as_str)
                    .filter_map(|s| s.parse::<usize>().ok())
                    .collect();
                if cols.is_empty() {
                    return Err("fields: expected column numbers (e.g. fields 1 3)".into());
                }
                ops.push(QueryOp::Fields(cols));
            }
            "each" => ops.push(QueryOp::Each),
            "count" => ops.push(QueryOp::Count),
            "rate" => ops.push(QueryOp::Rate),
            "tally" => ops.push(QueryOp::Tally),
            "sum" => ops.push(QueryOp::Sum),
            "min" => ops.push(QueryOp::Min),
            "max" => ops.push(QueryOp::Max),
            "avg" => ops.push(QueryOp::Avg),
            "keys" => ops.push(QueryOp::Keys),
            "vals" => ops.push(QueryOp::Vals),
            "pick" => {
                let keys: Vec<String> = c
                    .args
                    .iter()
                    .filter_map(Arg::as_str)
                    .map(str::to_string)
                    .collect();
                if keys.is_empty() {
                    return Err("pick: expected one or more key names".into());
                }
                ops.push(QueryOp::Pick(keys));
            }
            "sort" => {
                let flags: Vec<&str> = c.args.iter().filter_map(Arg::as_str).collect();
                ops.push(QueryOp::Sort {
                    numeric: flags.contains(&"-n"),
                    reverse: flags.contains(&"-r"),
                });
            }
            "uniq" => ops.push(QueryOp::Uniq),
            "rev" => ops.push(QueryOp::Rev),
            "first" => ops.push(QueryOp::First),
            "last" => ops.push(QueryOp::Last),
            "upper" => ops.push(QueryOp::Upper),
            "lower" => ops.push(QueryOp::Lower),
            "trim" => ops.push(QueryOp::Trim),
            "replace" => {
                let re = regex_arg(c)?;
                let to = c
                    .args
                    .get(1)
                    .and_then(Arg::as_str)
                    .unwrap_or("")
                    .to_string();
                ops.push(QueryOp::Replace(re, to));
            }
            "join" => {
                let sep = c
                    .args
                    .first()
                    .and_then(Arg::as_str)
                    .unwrap_or(" ")
                    .to_string();
                ops.push(QueryOp::Join(sep));
            }
            "nth" => ops.push(QueryOp::Nth(count_arg(c, "nth")?)),
            "take" => ops.push(QueryOp::Take(count_arg(c, "take")?)),
            "drop" => ops.push(QueryOp::Drop(count_arg(c, "drop")?)),
            "calc" => {
                let src = c
                    .args
                    .iter()
                    .filter_map(Arg::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                ops.push(QueryOp::Calc(crate::expr::parse(&src)?));
            }
            "where" => {
                let src = c
                    .args
                    .iter()
                    .filter_map(Arg::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                ops.push(QueryOp::Where(crate::expr::parse(&src)?));
            }
            "map" => {
                let src = c
                    .args
                    .iter()
                    .filter_map(Arg::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                ops.push(QueryOp::Map(crate::expr::parse(&src)?));
            }
            "sort_by" => {
                let field = str_arg(c);
                if field.is_empty() {
                    return Err("sort_by: expected a field name".into());
                }
                ops.push(QueryOp::SortBy(field));
            }
            "unique_by" => {
                let field = str_arg(c);
                if field.is_empty() {
                    return Err("unique_by: expected a field name".into());
                }
                ops.push(QueryOp::UniqueBy(field));
            }
            "count_by" => {
                let field = str_arg(c);
                if field.is_empty() {
                    return Err("count_by: expected a field name".into());
                }
                ops.push(QueryOp::CountBy(field));
            }
            "group_by" => {
                let field = str_arg(c);
                if field.is_empty() {
                    return Err("group_by: expected a field name".into());
                }
                ops.push(QueryOp::GroupBy(field));
            }
            "min_by" => {
                let field = str_arg(c);
                if field.is_empty() {
                    return Err("min_by: expected a field name".into());
                }
                ops.push(QueryOp::MinBy(field));
            }
            "max_by" => {
                let field = str_arg(c);
                if field.is_empty() {
                    return Err("max_by: expected a field name".into());
                }
                ops.push(QueryOp::MaxBy(field));
            }
            "has" => {
                let key = str_arg(c);
                if key.is_empty() {
                    return Err("has: expected a key name".into());
                }
                ops.push(QueryOp::Has(key));
            }
            "entries" => {
                if !c.args.is_empty() { return Err("entries: takes no arguments".into()); }
                ops.push(QueryOp::Entries);
            }
            "flatten" => ops.push(QueryOp::Flatten),
            "add" => ops.push(QueryOp::Add),
            "over" => {
                let n = c
                    .args
                    .first()
                    .and_then(Arg::as_str)
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or_else(|| "over: expected a numeric threshold".to_string())?;
                ops.push(QueryOp::Over(n));
            }
            "under" => {
                let n: f64 = str_arg(c)
                    .parse()
                    .map_err(|_| "under: expected a number".to_string())?;
                ops.push(QueryOp::Under(n));
            }
            "between" => {
                let lo = c
                    .args
                    .first()
                    .and_then(Arg::as_str)
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or_else(|| "between: expected two numbers A B".to_string())?;
                let hi = c
                    .args
                    .get(1)
                    .and_then(Arg::as_str)
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or_else(|| "between: expected two numbers A B".to_string())?;
                ops.push(QueryOp::Between(lo, hi));
            }
            "enumerate" => {
                ops.push(QueryOp::Enumerate);
            }
            "words" => {
                if !c.args.is_empty() { return Err("words: takes no arguments".into()); }
                ops.push(QueryOp::Words);
            }
            "dedup" => ops.push(QueryOp::Dedup),
            "tailn" => ops.push(QueryOp::Tailn(count_arg(c, "tailn")?)),
            "pad" => {
                let n = count_arg(c, "pad")?;
                ops.push(QueryOp::Pad(n));
            }
            "lpad" => ops.push(QueryOp::Lpad(count_arg(c, "lpad")?)),
            "grepf" => {
                let field = c
                    .args
                    .first()
                    .and_then(Arg::as_str)
                    .ok_or_else(|| "grepf: expected FIELD and /re/".to_string())?
                    .to_string();
                let raw = c
                    .args
                    .get(1)
                    .and_then(Arg::as_str)
                    .ok_or_else(|| "grepf: expected a pattern".to_string())?;
                let pat = raw
                    .strip_prefix('/')
                    .and_then(|s| s.strip_suffix('/'))
                    .unwrap_or(raw);
                let re = regex::Regex::new(pat).map_err(|e| format!("grepf: bad regex: {e}"))?;
                ops.push(QueryOp::Grepf(field, re));
            }
            "apply" => {
                let name = str_arg(c);
                let name = name.strip_prefix('.').unwrap_or(&name).to_string();
                ops.push(QueryOp::Apply(name));
            }
            "basename" => ops.push(QueryOp::Basename),
            "dirname" => ops.push(QueryOp::Dirname),
            "commafy" => ops.push(QueryOp::Commafy),
            "bytes" => ops.push(QueryOp::Bytes),
            "duration" => ops.push(QueryOp::Duration),
            "flip" => {
                ops.push(QueryOp::Flip);
            }
            "b64" => {
                ops.push(QueryOp::B64);
            }
            "b64d" => {
                ops.push(QueryOp::B64d);
            }
            "hex" => ops.push(QueryOp::Hex),
            "unhex" => {
                ops.push(QueryOp::Unhex);
            }
            "urlenc" => {
                ops.push(QueryOp::Urlenc);
            }
            "urldec" => {
                ops.push(QueryOp::Urldec);
            }
            "extract" => {
                ops.push(QueryOp::Extract(regex_arg(c)?));
            }
            "split" => {
                let delim = str_arg(c);
                if delim.is_empty() { return Err("split: expected a non-empty delimiter".into()); }
                ops.push(QueryOp::Split(delim));
            }
            "substr" => {
                let args: Vec<usize> = c
                    .args
                    .iter()
                    .filter_map(Arg::as_str)
                    .filter_map(|s| s.parse::<usize>().ok())
                    .collect();
                if args.len() != 2 {
                    return Err("substr: expected two non-negative integer args A B".into());
                }
                ops.push(QueryOp::Substr(args[0], args[1]));
            }
            "chars" => ops.push(QueryOp::Chars),
            "title" => {
                ops.push(QueryOp::Title);
            }
            "repeat" => {
                let n = count_arg(c, "repeat")?;
                ops.push(QueryOp::Repeat(n));
            }
            "set" => {
                let key = str_arg(c);
                if key.is_empty() { return Err("set: expected key and value".into()); }
                let val = c.args.iter().filter_map(Arg::as_str).nth(1).unwrap_or("").to_string();
                ops.push(QueryOp::Set(key, val));
            }
            "del" => {
                let key = str_arg(c);
                if key.is_empty() { return Err("del: expected a key name".into()); }
                ops.push(QueryOp::Del(key));
            }
            "rename" => {
                let args: Vec<String> = c.args.iter().filter_map(Arg::as_str).map(str::to_string).collect();
                if args.len() != 2 || args[0].is_empty() || args[1].is_empty() {
                    return Err("rename: expected OLD NEW key names".into());
                }
                ops.push(QueryOp::Rename(args[0].clone(), args[1].clone()));
            }
            "default" => {
                let args: Vec<String> = c.args.iter().filter_map(Arg::as_str).map(str::to_string).collect();
                if args.len() != 2 {
                    return Err("default: expected exactly two args: key value".into());
                }
                ops.push(QueryOp::Default(args[0].clone(), args[1].clone()));
            }
            "merge" => {
                ops.push(QueryOp::Merge);
            }
            "floor" => {
                ops.push(QueryOp::Floor);
            }
            "ceil" => {
                ops.push(QueryOp::Ceil);
            }
            "clamp" => {
                let mut it = c.args.iter().filter_map(Arg::as_str);
                let lo = it.next().and_then(|s| s.parse::<f64>().ok());
                let hi = it.next().and_then(|s| s.parse::<f64>().ok());
                match (lo, hi) {
                    (Some(lo), Some(hi)) => ops.push(QueryOp::Clamp(lo, hi)),
                    _ => return Err("clamp: expected LO HI numeric args".into()),
                }
            }
            "contains" => ops.push(QueryOp::Contains(str_arg(c))),
            "starts" => ops.push(QueryOp::Starts(str_arg(c))),
            "ends" => ops.push(QueryOp::Ends(str_arg(c))),
            "nonempty" => ops.push(QueryOp::Nonempty),
            "numeric" => ops.push(QueryOp::Numeric),
            "len" => ops.push(QueryOp::Len),
            "wc" => ops.push(QueryOp::Wc),
            "abs" => ops.push(QueryOp::Abs),
            "round" => ops.push(QueryOp::Round),
            "delta" => ops.push(QueryOp::Delta),
            "cumsum" => ops.push(QueryOp::Cumsum),
            "sma" => ops.push(QueryOp::Sma(count_arg(c, "sma")?)),
            "ewma" => {
                let a = str_arg(c)
                    .parse::<f64>()
                    .map_err(|_| "ewma: expected a smoothing factor 0–1 (e.g. 0.3)".to_string())?;
                ops.push(QueryOp::Ewma(a));
            }
            "prepend" => ops.push(QueryOp::Prepend(str_arg(c))),
            "append" => ops.push(QueryOp::Append(str_arg(c))),
            "cut" => {
                let delim = str_arg(c);
                let n = c
                    .args
                    .get(1)
                    .and_then(Arg::as_str)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                ops.push(QueryOp::Cut(delim, n));
            }
            "median" => ops.push(QueryOp::Median),
            "percentile" => {
                let p = str_arg(c)
                    .parse::<f64>()
                    .map_err(|_| "percentile: expected a number 0–100 (e.g. 99)".to_string())?;
                ops.push(QueryOp::Percentile(p));
            }
            "p50" => ops.push(QueryOp::Percentile(50.0)),
            "p90" => ops.push(QueryOp::Percentile(90.0)),
            "p95" => ops.push(QueryOp::Percentile(95.0)),
            "p99" => ops.push(QueryOp::Percentile(99.0)),
            "stddev" => ops.push(QueryOp::Stddev),
            "range" => ops.push(QueryOp::Range),
            "product" => ops.push(QueryOp::Product),
            "distinct" => ops.push(QueryOp::Distinct),
            "sample" => ops.push(QueryOp::Sample(count_arg(c, "sample")?)),
            "bins" => ops.push(QueryOp::Bins(count_arg(c, "bins")?)),
            "index" => ops.push(QueryOp::Index(count_arg(c, "index")?)),
            "slice" => {
                let a = c
                    .args
                    .first()
                    .and_then(Arg::as_str)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(1);
                let b = c
                    .args
                    .get(1)
                    .and_then(Arg::as_str)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(usize::MAX);
                ops.push(QueryOp::Slice(a, b));
            }
            other => return Err(format!("source: unknown verb `{other}`").into()),
        }
        Ok(())
        })();
        step.map_err(|e| e.or_span(c.pos, c.name.chars().count()))?;
    }
    if !saw_in {
        return Err("source: pipeline must start with `in`".into());
    }
    Ok(ops)
}

/// Compile a `/pattern/` (or bare `pattern`) literal into a `Regex`. Shared by
/// the arg form (`match /re/`, single-clause `expect /re/ …`) and the
/// `expect { }` block-clause form, where the `/re/` arrives as the clause
/// command's *name*.
fn compile_regex(raw: &str) -> Result<Regex, String> {
    let pat = raw
        .strip_prefix('/')
        .and_then(|s| s.strip_suffix('/'))
        .unwrap_or(raw);
    Regex::new(pat).map_err(|e| format!("bad regex: {e}"))
}

/// Reconstruct a shell string from a `{ … }` block body: each top-level command
/// contributes its verb + word args, commands joined by `; ` (`{ ps aux }` ->
/// "ps aux"; `{ tail -f a.log; grep err }` -> "tail -f a.log; grep err").
fn block_to_shell(cmds: &[Command]) -> String {
    cmds.iter()
        .map(|c| {
            let mut w = vec![c.name.clone()];
            w.extend(c.args.iter().filter_map(|a| a.as_str().map(str::to_string)));
            w.join(" ")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Read a regex argument, stripping optional `/…/` delimiters. The error carries
/// this command's source span so a bad regex anchors to the offending verb (e.g.
/// `match` inside a `source { … }` body), not the enclosing directive.
fn regex_arg(c: &Command) -> Result<Regex, crate::err::SpecError> {
    let span = |msg: String| crate::err::SpecError::from(msg).or_span(c.pos, c.name.chars().count());
    let raw = c
        .args
        .first()
        .and_then(Arg::as_str)
        .ok_or_else(|| span(format!("{}: expected a pattern", c.name)))?;
    compile_regex(raw).map_err(|e| span(format!("{}: {e}", c.name)))
}

/// Parse a required count argument for `take`/`drop`.
/// The first arg as a string (empty if absent) — for verbs taking a literal.
fn str_arg(c: &Command) -> String {
    c.args
        .first()
        .and_then(Arg::as_str)
        .unwrap_or("")
        .to_string()
}

fn count_arg(c: &Command, verb: &str) -> Result<usize, String> {
    c.args
        .first()
        .and_then(Arg::as_str)
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or_else(|| format!("{verb}: expected a count"))
}

/// A single numeric arg selects a whitespace column; anything else is a JSON
/// key path (`field a b c` -> a.b.c).
fn field_sel(args: &[Arg]) -> Result<FieldSel, String> {
    let words: Vec<&str> = args.iter().filter_map(Arg::as_str).collect();
    if words.is_empty() {
        return Err("field: expected a column number or key path".into());
    }
    if words.len() == 1 {
        if let Ok(n) = words[0].parse::<usize>() {
            return Ok(FieldSel::Col(n));
        }
    }
    Ok(FieldSel::Key(words.iter().map(|s| s.to_string()).collect()))
}

fn set_source(spec: &mut Spec, path: &str, src: Source) -> Result<(), String> {
    for w in &mut spec.widgets {
        if w.path == path {
            w.source = Some(src);
            return Ok(());
        }
    }
    Err(format!("source: no widget named `{path}`"))
}

/// `.path configure -k v …`: merge `opts` into the named widget's options
/// (later keys win), so a spec can retune a widget after declaring it.
fn configure_widget(
    spec: &mut Spec,
    path: &str,
    opts: BTreeMap<String, String>,
) -> Result<(), String> {
    for w in &mut spec.widgets {
        if w.path == path {
            w.opts.extend(opts);
            return Ok(());
        }
    }
    Err(format!("configure: no widget named `{path}`"))
}

fn set_search(spec: &mut Spec, path: &str, pipeline: Vec<QueryOp>) -> Result<(), String> {
    for w in &mut spec.widgets {
        if w.path == path {
            w.search = Some(pipeline);
            return Ok(());
        }
    }
    Err(format!("search: no widget named `{path}`"))
}

fn set_grid(
    spec: &mut Spec,
    path: &str,
    cell: (usize, usize),
    span: (usize, usize),
) -> Result<(), String> {
    for w in &mut spec.widgets {
        if w.path == path {
            w.grid = Some(cell);
            w.span = span;
            return Ok(());
        }
    }
    Err(format!("grid: no widget named `{path}`"))
}
