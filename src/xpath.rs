//! Native XPath front-end for `source { … }` / `out { … }` bodies. A body
//! command whose first token starts with `/`, `//`, or `@` is an XPath literal.
//!
//! **This is a real XPath 1.0 engine**, not a translation layer:
//! [`crate::xpath_syntax`] lexes and parses the full grammar of the W3C
//! Recommendation (<https://www.w3.org/TR/1999/REC-xpath-19991116/>) and
//! [`crate::xpath_eval`] evaluates it over the parsed document — all 13 axes,
//! all four node tests, predicates with correct proximity positions, and the
//! 27-function core library.
//!
//! It replaces a translator that compiled XPath-shaped syntax to a CSS selector.
//! That approach could not carry an axis or a function at all, and it did worse
//! than refuse them: it compiled a ROOTED path `/li/text()` to the CSS
//! descendant selector `li`, so a path XPath says selects NOTHING answered with
//! three nodes and exit 0. `[@a='x' or @a='y']` and a chained predicate
//! `[@a='x'][@b='y']` went the other way, answering EMPTY where XPath selects
//! nodes. A wrong answer under a success exit is the one outcome SPEC §8 rules
//! out, and the four are pinned in `tests/xpath.rs` against `xmllint`.
//!
//! ## Where the expression is evaluated
//!
//! arb's pipeline is a LINE stream, and an XPath step can appear either as a
//! whole query or as one stage of a chain (`//div[@class='card']` then `@href`).
//! So the context follows the expression's own shape, which is XPath's rule
//! already:
//!
//! * An ABSOLUTE expression (`/…`, `//…`) is evaluated ONCE over the whole
//!   stream parsed as one document, because `/` means the document root.
//! * A RELATIVE expression (`@href`, `text()`, `a/b`, `count(p)`) is evaluated
//!   PER LINE with the context node set to that line's first element, so it
//!   composes with whatever step produced those lines.

use crate::query::QueryOp;
use crate::xpath_syntax::{self, Expr, PathStart};

/// Parse an XPath literal, or report why it is not one.
///
/// Parsing happens HERE, at spec-build time, so a malformed expression is a hard
/// error before any input is read — never a silent empty node-set at run time.
pub fn translate(src: &str) -> Result<Vec<QueryOp>, String> {
    let s = src.trim();
    if s.is_empty() {
        return Err("xpath: empty expression".into());
    }
    // A jq FORMAT STRING also starts with `@`, and the body dispatcher sends
    // every leading-`@` command here. Read as an attribute step, `@base64`
    // would select an attribute nobody has and yield an empty result with a
    // ZERO exit — a jq construct silently answering "nothing" instead of
    // erroring, which is precisely what SPEC §8 rules out. The format names are
    // a closed set, so name them and refuse. (An element really carrying an
    // attribute with one of these names is still reachable through the native
    // `attr` verb, which is not `@`-dispatched.)
    if let Some(rest) = s.strip_prefix('@') {
        const JQ_FORMATS: [&str; 9] = [
            "text", "json", "html", "uri", "csv", "tsv", "sh", "base64", "base64d",
        ];
        if JQ_FORMATS.contains(&rest) {
            return Err(format!(
                "xpath: `@{rest}` is a jq format string, which is outside the \
                 supported subset (a leading `@` here is an xpath attribute step; \
                 use `attr {rest}` for a literal attribute of that name)"
            ));
        }
    }
    // A bare NCName is a legal XPath relative location path (`bogus` is
    // `child::bogus`), and at COMMAND position that is a trap: a typo'd arb verb
    // would parse, select nothing, and exit 0 — the silent answer this engine
    // exists to remove, reintroduced through the front door. So an expression
    // reaching arb's command position must carry a character only XPath uses.
    //
    // Nothing becomes unreachable: the same step is spelled `//bogus`,
    // `./bogus` or `child::bogus`, any of which is unambiguous. This is the
    // `keys`/`names` rule again — a language does not get to claim a spelling
    // another meaning already owns.
    if !s.contains(['/', '@', ':', '(', '[', '|']) {
        return Err(format!(
            "xpath: `{src}` has no xpath-only syntax, so it is read as a verb name              (spell the step `//{src}`, `./{src}` or `child::{src}` to force xpath)"
        ));
    }
    let expr = xpath_syntax::parse(s).map_err(|e| format!("xpath: {e} in `{src}`"))?;
    // Only a RELATIVE LOCATION PATH composes with a previous pipeline step, and
    // only it is evaluated per line. Everything else — an absolute path, a
    // function call, a comparison, a union — is one question about the DOCUMENT
    // and is answered once. Deciding this from the source text instead (a
    // leading `/`) made `count(//p)` run once per input line and print eight
    // separate counts.
    let per_line = matches!(expr, Expr::Path(PathStart::Relative, _));
    Ok(vec![QueryOp::XPath(Box::new(XPath {
        src: s.to_string(),
        expr,
        per_line,
    }))])
}

/// A compiled XPath expression, carried by the query op.
#[derive(Debug, Clone)]
pub struct XPath {
    pub src: String,
    pub expr: Expr,
    /// Whether to evaluate once per input line (see the module docs).
    pub per_line: bool,
}

impl XPath {
    /// Evaluate over the current stream lines, returning the output lines.
    pub fn run(&self, lines: &[String]) -> Result<Vec<String>, String> {
        use crate::xpath_eval::{eval, render, Doc};
        if !self.per_line {
            let doc = Doc::parse(&lines.join("\n"));
            let v = eval(&doc, &self.expr, doc.root())
                .map_err(|e| format!("xpath: {e} in `{}`", self.src))?;
            return Ok(render(&doc, &v));
        }
        // Relative: one evaluation per line, context = that line's first
        // element, so a chain like `//a` then `@href` behaves as it reads.
        let mut out = Vec::new();
        for l in lines {
            let doc = Doc::parse(l);
            let ctx = first_element(&doc).unwrap_or_else(|| doc.root());
            let v =
                eval(&doc, &self.expr, ctx).map_err(|e| format!("xpath: {e} in `{}`", self.src))?;
            out.extend(render(&doc, &v));
        }
        Ok(out)
    }
}

/// The first element of a re-parsed fragment, skipping the `html`/`body`
/// wrappers the parser inserts around one. That is the node a previous pipeline
/// step actually produced.
fn first_element(doc: &crate::xpath_eval::Doc) -> Option<crate::xpath_eval::XNode> {
    use crate::xpath_eval::{Kind, XNode};
    let mut best: Option<XNode> = None;
    for n in doc.document_nodes() {
        if doc.kind(n) == Kind::Element {
            let name = doc.name(n).unwrap_or_default();
            if name == "html" || name == "body" || name == "head" {
                continue;
            }
            best = Some(n);
            break;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{eval, QueryResult};

    /// Render an xpath literal over one HTML document, returning the lines.
    fn run(src: &str, html: &str) -> Vec<String> {
        match eval(&translate(src).unwrap(), &[html.to_string()], 0.0) {
            QueryResult::Lines(v) => v,
            other => panic!("expected Lines, got {other:?}"),
        }
    }

    #[test]
    fn descendant_and_child_paths() {
        let doc = "<html><body><div><span>A</span></div><span>B</span></body></html>";
        assert_eq!(run("//span/text()", doc), vec!["A", "B"]);
        assert_eq!(run("//div/span/text()", doc), vec!["A"]);
        assert_eq!(run("/html/body/span/text()", doc), vec!["B"]);
    }

    #[test]
    fn attr_predicate_and_accessors() {
        let doc = "<div><a href=\"x\">1</a><a href=\"y\">2</a></div>";
        assert_eq!(run("//a/@href", doc), vec!["x", "y"]);
        assert_eq!(run("//a/text()", doc), vec!["1", "2"]);
        // A path step then a standalone attr step (two ops) is the same result.
        let two = [translate("//a").unwrap(), translate("@href").unwrap()].concat();
        match eval(&two, &[doc.to_string()], 0.0) {
            QueryResult::Lines(v) => assert_eq!(v, vec!["x", "y"]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_malformed_expression_is_a_parse_error_not_an_empty_answer() {
        for bad in [
            "//a[",            // unclosed predicate
            "//a[@href",       // unclosed predicate
            "count(",          // unclosed call
            "//a[@class=btn",  // unterminated
            "'unterminated",   // unterminated literal
            "//a!",            // stray `!`
            "//a/@href extra", // trailing junk
            "//nosuchfn()",    // not a node type
            "//bogus-axis::a", // not an axis
            "",                // empty
        ] {
            assert!(translate(bad).is_err(), "expected `{bad}` to be refused");
        }
    }
}
