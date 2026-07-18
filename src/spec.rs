//! Interpreter: command tree -> a `Spec` (a flat list of widgets with options
//! and an optional data source). M1 recognizes the widget verbs, `source .x {
//! in }`, and the `.x <- in` bind shorthand. Unknown verbs are ignored so the
//! language can grow without breaking older specs.

use std::collections::BTreeMap;

use crate::ast::{Arg, Command};

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

/// M1 sources. Only stdin so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Stdin,
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
            set_source(&mut spec, path, source_from_body(body)?)?;
        } else if c.name.starts_with('.') {
            // `.path <- in` bind shorthand. `configure` etc. land later.
            if c.args.first().and_then(Arg::as_str) == Some("<-")
                && c.args.get(1).and_then(Arg::as_str) == Some("in")
            {
                let path = c.name.clone();
                set_source(&mut spec, &path, Source::Stdin)?;
            }
        }
        // Unknown verbs are ignored in M1.
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

fn source_from_body(cmds: &[Command]) -> Result<Source, String> {
    if cmds.iter().any(|c| c.name == "in") {
        Ok(Source::Stdin)
    } else {
        Err("source: only `in` (stdin) is supported in M1".into())
    }
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
