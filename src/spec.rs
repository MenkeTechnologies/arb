//! Interpreter: command tree -> a `Spec`. Recognizes the widget verbs,
//! `source .x { … }` whose body compiles to a query pipeline (see `query`), and
//! the `.x <- in` bind shorthand. Unknown widget verbs are ignored so specs stay
//! forward-compatible.

use std::collections::BTreeMap;

use regex::Regex;

use crate::ast::{Arg, Command};
use crate::query::QueryOp;

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
}

#[derive(Debug, Default)]
pub struct Spec {
    pub widgets: Vec<Widget>,
}

/// Build a `Spec` from a parsed command tree.
pub fn build(cmds: &[Command]) -> Result<Spec, String> {
    let mut spec = Spec::default();
    for c in cmds {
        if let Some(kind) = WidgetKind::from(&c.name) {
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
            set_source(&mut spec, path, Source { pipeline })?;
        } else if c.name.starts_with('.') {
            // `.path <- in` bind shorthand (empty pipeline). `configure` etc. later.
            if c.args.first().and_then(Arg::as_str) == Some("<-")
                && c.args.get(1).and_then(Arg::as_str) == Some("in")
            {
                let path = c.name.clone();
                set_source(&mut spec, &path, Source { pipeline: vec![] })?;
            }
        }
        // Unknown verbs are ignored.
    }
    Ok(spec)
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
            "in" => saw_in = true,
            "match" | "grep" => ops.push(QueryOp::Match(regex_arg(c)?)),
            "reject" | "grepv" => ops.push(QueryOp::Reject(regex_arg(c)?)),
            "field" => {
                let n = c
                    .args
                    .first()
                    .and_then(Arg::as_str)
                    .and_then(|s| s.parse::<usize>().ok())
                    .ok_or("field: expected a 1-based column number (M2a: numeric fields)")?;
                ops.push(QueryOp::Field(n));
            }
            "count" => ops.push(QueryOp::Count),
            "rate" => ops.push(QueryOp::Rate),
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

fn set_source(spec: &mut Spec, path: &str, src: Source) -> Result<(), String> {
    for w in &mut spec.widgets {
        if w.path == path {
            w.source = Some(src);
            return Ok(());
        }
    }
    Err(format!("source: no widget named `{path}`"))
}
