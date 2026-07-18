//! Interpreter: command tree -> a `Spec`. Recognizes the widget verbs,
//! `source .x { … }` whose body compiles to a query pipeline (see `query`), and
//! the `.x <- in` bind shorthand. Unknown widget verbs are ignored so specs stay
//! forward-compatible.

use std::collections::BTreeMap;

use regex::Regex;

use crate::ast::{Arg, Command};
use crate::query::{FieldSel, QueryOp};

#[derive(Debug, Clone, PartialEq, Eq)]
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
}

impl WidgetKind {
    fn from(verb: &str) -> Option<WidgetKind> {
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
        }
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
    /// Grid cell `(row, col)` set by a `grid` command; `None` = auto-stacked.
    pub grid: Option<(usize, usize)>,
}

#[derive(Debug, Default)]
pub struct Spec {
    pub widgets: Vec<Widget>,
}

/// Build a `Spec` from a parsed command tree.
pub fn build(cmds: &[Command]) -> Result<Spec, String> {
    let mut spec = Spec::default();
    build_into(&mut spec, cmds, 0)?;
    Ok(spec)
}

/// Process `cmds` into `spec`. `import NAME` resolves and inlines a module
/// (stdlib preset or user file); `depth` guards against import cycles.
fn build_into(spec: &mut Spec, cmds: &[Command], depth: usize) -> Result<(), String> {
    if depth > 16 {
        return Err("import: module nesting too deep (cycle?)".into());
    }
    for c in cmds {
        if c.name == "import" {
            let name = c
                .args
                .first()
                .and_then(Arg::as_str)
                .ok_or("import: missing module name")?;
            let src = resolve_module(name)?;
            let sub = crate::parser::parse(&src)?;
            build_into(spec, &sub, depth + 1)?;
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
                ));
            }
            spec.widgets.push(Widget {
                path: path.to_string(),
                kind,
                opts: parse_opts(&c.args[1..]),
                source: None,
                grid: None,
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
        } else if c.name == "grid" {
            let path = c
                .args
                .first()
                .and_then(Arg::as_str)
                .ok_or("grid: missing path")?;
            let o = parse_opts(&c.args[1..]);
            let cell = |k| o.get(k).and_then(|s: &String| s.parse::<usize>().ok()).unwrap_or(0);
            set_grid(spec, path, (cell("row"), cell("col")))?;
        } else if c.name.starts_with('.') {
            // `.path <- in` bind shorthand (empty pipeline). `configure` etc. later.
            if c.args.first().and_then(Arg::as_str) == Some("<-")
                && c.args.get(1).and_then(Arg::as_str) == Some("in")
            {
                let path = c.name.clone();
                set_source(spec, &path, Source { pipeline: vec![] })?;
            }
        }
        // Unknown verbs are ignored.
    }
    Ok(())
}

/// Resolve a module name to its source: a local `NAME.arb`, then
/// `~/.arb/lib/NAME.arb`, then a bundled stdlib preset.
fn resolve_module(name: &str) -> Result<String, String> {
    let local = format!("{name}.arb");
    if let Ok(s) = std::fs::read_to_string(&local) {
        return Ok(s);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = std::path::Path::new(&home)
            .join(".arb/lib")
            .join(format!("{name}.arb"));
        if let Ok(s) = std::fs::read_to_string(p) {
            return Ok(s);
        }
    }
    match name {
        "nums" => Ok(include_str!("../stdlib/nums.arb").to_string()),
        "logs" => Ok(include_str!("../stdlib/logs.arb").to_string()),
        _ => Err(format!("import: module `{name}` not found")),
    }
}

/// Collect `-flag value` pairs into an options map.
fn parse_opts(args: &[Arg]) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(flag) = args[i].as_str().and_then(|w| w.strip_prefix('-')) {
            let val = args
                .get(i + 1)
                .and_then(Arg::as_str)
                .unwrap_or("")
                .to_string();
            m.insert(flag.to_string(), val);
            i += 2;
        } else {
            i += 1;
        }
    }
    m
}

/// Compile a `source { … }` body into a query pipeline. Must start with `in`.
fn pipeline_from_body(cmds: &[Command]) -> Result<Vec<QueryOp>, String> {
    let mut ops = Vec::new();
    let mut saw_in = false;
    for c in cmds {
        match c.name.as_str() {
            "in" | "in.json" | "in.html" | "in.xml" => saw_in = true,
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
            "match" | "grep" => ops.push(QueryOp::Match(regex_arg(c)?)),
            "reject" | "grepv" => ops.push(QueryOp::Reject(regex_arg(c)?)),
            "field" => ops.push(QueryOp::Field(field_sel(&c.args)?)),
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
            other => return Err(format!("source: unknown verb `{other}`")),
        }
    }
    if !saw_in {
        return Err("source: pipeline must start with `in`".into());
    }
    Ok(ops)
}

/// Read a regex argument, stripping optional `/…/` delimiters.
fn regex_arg(c: &Command) -> Result<Regex, String> {
    let raw = c
        .args
        .first()
        .and_then(Arg::as_str)
        .ok_or_else(|| format!("{}: expected a pattern", c.name))?;
    let pat = raw
        .strip_prefix('/')
        .and_then(|s| s.strip_suffix('/'))
        .unwrap_or(raw);
    Regex::new(pat).map_err(|e| format!("{}: bad regex: {e}", c.name))
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

fn set_grid(spec: &mut Spec, path: &str, cell: (usize, usize)) -> Result<(), String> {
    for w in &mut spec.widgets {
        if w.path == path {
            w.grid = Some(cell);
            return Ok(());
        }
    }
    Err(format!("grid: no widget named `{path}`"))
}
