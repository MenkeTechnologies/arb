//! Static HTML export: render a [`Spec`] into a single self-contained dashboard
//! page. No external resources, no CDNs — one inline `<style>` block and the
//! widget panels laid out on a CSS grid. Used to snapshot a pipeline TUI as a
//! shareable HTML file.

use std::fmt::Write as _;

use crate::spec::{Spec, Widget, WidgetKind};

/// Render `spec` as a complete, self-contained static HTML page.
///
/// The returned string is a full document (`<!doctype html>` … `</html>`) with
/// an inline dark "cyberpunk" stylesheet and one bordered panel per widget.
pub fn render_html(spec: &Spec) -> String {
    let mut out = String::new();
    out.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n");
    out.push_str("<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    out.push_str("<title>arb dashboard</title>\n");
    out.push_str(STYLE);
    out.push_str("</head>\n<body>\n");
    out.push_str("<h1>arb dashboard</h1>\n");

    if spec.widgets.is_empty() {
        out.push_str("<p class=\"empty\">no widgets</p>\n");
    } else {
        out.push_str("<main class=\"grid\">\n");
        for w in &spec.widgets {
            render_panel(&mut out, w);
        }
        out.push_str("</main>\n");
    }

    out.push_str("</body>\n</html>\n");
    out
}

/// Emit one `<section>` panel for a single widget.
fn render_panel(out: &mut String, w: &Widget) {
    out.push_str("<section class=\"panel\">\n");
    let _ = writeln!(
        out,
        "<header class=\"phead\"><span class=\"ppath\">{}</span><span class=\"pkind\">{}</span></header>",
        escape(&w.path),
        escape(w.kind.label()),
    );
    let _ = writeln!(
        out,
        "<div class=\"pbody\">{}</div>",
        escape(&body_text(w)),
    );
    let badge = if w.source.is_some() {
        "<footer class=\"psrc live\">\u{25cf} bound to source</footer>\n"
    } else {
        "<footer class=\"psrc\">\u{25cb} no source</footer>\n"
    };
    out.push_str(badge);
    out.push_str("</section>\n");
}

/// Human-readable placeholder describing a widget kind, pulling any relevant
/// options out of `w.opts` so the panel body echoes its configuration.
fn body_text(w: &Widget) -> String {
    let opt = |k: &str| w.opts.get(k).map(String::as_str);
    match w.kind {
        WidgetKind::Text | WidgetKind::Tail => match opt("lines") {
            Some(n) => format!("{} (lines={})", w.kind.label(), n),
            None => w.kind.label().to_string(),
        },
        WidgetKind::List => match opt("limit") {
            Some(n) => format!("list (limit={})", n),
            None => "list".to_string(),
        },
        WidgetKind::Gauge => match opt("max") {
            Some(m) => format!("gauge (max={})", m),
            None => "gauge".to_string(),
        },
        WidgetKind::Bars | WidgetKind::Histo | WidgetKind::Spark => match opt("max") {
            Some(m) => format!("{} (max={})", w.kind.label(), m),
            None => w.kind.label().to_string(),
        },
        WidgetKind::Chart => match opt("height") {
            Some(h) => format!("chart (height={})", h),
            None => "chart".to_string(),
        },
        WidgetKind::Table => match opt("cols") {
            Some(c) => format!("table (cols={})", c),
            None => "table".to_string(),
        },
        WidgetKind::Slider => {
            format!("slider ({}..{})", opt("min").unwrap_or("0"), opt("max").unwrap_or(""))
        }
        WidgetKind::Facet => match opt("opts").or_else(|| opt("field")) {
            Some(o) => format!("facet ({o})"),
            None => "facet".to_string(),
        },
        WidgetKind::Tabs
        | WidgetKind::Block
        | WidgetKind::Frame
        | WidgetKind::Input
        | WidgetKind::Filter
        | WidgetKind::Check
        | WidgetKind::Select => {
            match opt("title").or_else(|| opt("placeholder")).or_else(|| opt("prompt")).or_else(|| opt("label")) {
                Some(t) => format!("{} ({})", w.kind.label(), t),
                None => w.kind.label().to_string(),
            }
        }
    }
}

/// Escape the five HTML-special characters so dynamic text can't break markup.
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Inline dark "cyberpunk" stylesheet. Self-contained: no fonts, no CDNs.
pub(crate) const STYLE: &str = "<style>\n\
:root { --bg: #0a0e14; --panel: #0e131c; --edge: #1b2735; --cyan: #00e5ff; --fg: #c7d0da; --dim: #6b7a8d; }\n\
* { box-sizing: border-box; }\n\
body { margin: 0; padding: 1.5rem; background: var(--bg); color: var(--fg);\n\
  font-family: 'SFMono-Regular', Menlo, Consolas, 'Liberation Mono', monospace; font-size: 14px; }\n\
h1 { margin: 0 0 1.25rem; color: var(--cyan); font-size: 1.4rem; font-weight: 600;\n\
  letter-spacing: 0.08em; text-transform: uppercase; border-bottom: 1px solid var(--edge); padding-bottom: 0.6rem; }\n\
.empty { color: var(--dim); }\n\
.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(260px, 1fr)); gap: 1rem; }\n\
.panel { background: var(--panel); border: 1px solid var(--edge); border-radius: 4px; overflow: hidden; }\n\
.phead { display: flex; align-items: baseline; justify-content: space-between; gap: 0.75rem;\n\
  padding: 0.55rem 0.75rem; border-bottom: 1px solid var(--edge); background: rgba(0, 229, 255, 0.04); }\n\
.ppath { color: var(--cyan); font-weight: 600; overflow-wrap: anywhere; }\n\
.pkind { color: var(--dim); font-size: 0.8em; text-transform: uppercase; letter-spacing: 0.06em; }\n\
.pbody { padding: 0.9rem 0.75rem; color: var(--fg); min-height: 3.2rem; }\n\
.psrc { padding: 0.4rem 0.75rem; border-top: 1px solid var(--edge); font-size: 0.78em; color: var(--dim); }\n\
.psrc.live { color: var(--cyan); }\n\
</style>\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Widget, WidgetKind};
    use std::collections::BTreeMap;

    fn widget(path: &str, kind: WidgetKind, opts: &[(&str, &str)]) -> Widget {
        let mut map = BTreeMap::new();
        for (k, v) in opts {
            map.insert((*k).to_string(), (*v).to_string());
        }
        Widget {
            path: path.to_string(),
            kind,
            opts: map,
            source: None,
            search: None,
            grid: None,
            span: (1, 1),
        }
    }

    #[test]
    fn renders_document_shell_and_widget() {
        let spec = Spec {
            widgets: vec![widget(".cpu", WidgetKind::Gauge, &[("max", "100")])],
            ..Default::default()
        };
        let html = render_html(&spec);
        assert!(html.contains("<html"));
        assert!(html.contains("</html>"));
        assert!(html.contains("<title>arb dashboard</title>"));
        assert!(html.contains(".cpu"));
        assert!(html.contains("gauge"));
        assert!(html.contains("gauge (max=100)"));
    }

    #[test]
    fn includes_each_kind_label() {
        let spec = Spec {
            widgets: vec![
                widget(".a", WidgetKind::Table, &[]),
                widget(".b", WidgetKind::Spark, &[]),
            ],
            ..Default::default()
        };
        let html = render_html(&spec);
        assert!(html.contains(".a"));
        assert!(html.contains(".b"));
        assert!(html.contains("table"));
        assert!(html.contains("spark"));
    }

    #[test]
    fn shows_source_binding_badge() {
        use crate::spec::Source;
        let mut bound = widget(".s", WidgetKind::Tail, &[]);
        bound.source = Some(Source { pipeline: vec![] });
        let spec = Spec {
            widgets: vec![bound, widget(".u", WidgetKind::Text, &[])],
            ..Default::default()
        };
        let html = render_html(&spec);
        assert!(html.contains("bound to source"));
        assert!(html.contains("no source"));
    }

    #[test]
    fn escapes_html_special_chars() {
        let spec = Spec {
            widgets: vec![widget(".x<&>\"", WidgetKind::Text, &[])],
            ..Default::default()
        };
        let html = render_html(&spec);
        assert!(html.contains("&lt;&amp;&gt;&quot;"));
        assert!(!html.contains(".x<&>\""));
    }
}
