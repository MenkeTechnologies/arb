//! Native xpath-syntax front-end for `source { … }` / `out { … }` bodies. A body
//! command whose first token starts with `/`, `//`, or `@` is an xpath literal,
//! translated to a `Vec<QueryOp>` over arb's existing HTML ops (`Find`/`Attr`/
//! `Text`) — no new op. This is the xpath twin of [`crate::jq`].
//!
//! PRACTICAL subset (maps onto scraper's CSS engine): descendant `//tag`, child
//! `/a/b`, descendant chain `//a//b`, the `[@attr]` existence predicate, and the
//! trailing `/@attr` and `/text()` accessors; a standalone `@attr` step. That is
//! the slice of xpath with a faithful CSS equivalent. Everything richer — axes
//! (`..`, `ancestor::`, `following-sibling::`), positional/value predicates
//! (`[1]`, `[@a='b']`, `[last()]`), functions other than a trailing `text()`,
//! union `|`, `@*`, namespaced `ns:tag` — is a **hard error**, never a silent
//! mis-translation.

use crate::query::QueryOp;

/// Translate an xpath literal into arb query ops, or an `xpath: …` error for any
/// construct outside the supported subset.
pub fn translate(src: &str) -> Result<Vec<QueryOp>, String> {
    let s = src.trim();
    if s.is_empty() {
        return Err("xpath: empty expression".into());
    }
    // A standalone attribute step (`@href`): pulls the attribute off the current
    // element fragments (the twin of the native `attr NAME`).
    if let Some(rest) = s.strip_prefix('@') {
        return Ok(vec![QueryOp::Attr(validate_name(rest, s)?)]);
    }
    if !s.starts_with('/') {
        return Err(format!("xpath: expression must start with `/`, `//`, or `@`: `{src}`"));
    }
    if s.contains('|') {
        return Err(format!("xpath: union `|` is not supported: `{src}`"));
    }
    // Peel a trailing accessor (`/text()` or `/@attr`) off the location path.
    let (path, trailer) = if let Some(p) = s.strip_suffix("/text()") {
        (p, Some(Trailer::Text))
    } else if let Some(idx) = s.rfind("/@") {
        (&s[..idx], Some(Trailer::Attr(validate_name(&s[idx + 2..], s)?)))
    } else {
        (s, None)
    };
    let mut ops = vec![QueryOp::Find(path_to_css(path, s)?)];
    match trailer {
        Some(Trailer::Attr(a)) => ops.push(QueryOp::Attr(a)),
        Some(Trailer::Text) => ops.push(QueryOp::Text),
        None => {}
    }
    Ok(ops)
}

enum Trailer {
    Attr(String),
    Text,
}

/// Compile an xpath location path (already stripped of any trailing accessor)
/// into a scraper CSS selector. Leading `//` is the descendant axis, leading `/`
/// the child-from-root axis (approximated as a descendant match of the first
/// step); interior `//` → CSS descendant (` `), interior `/` → CSS child (` > `).
fn path_to_css(path: &str, whole: &str) -> Result<String, String> {
    // Split into (combinator, step) pairs. A leading `/` or `//` sets the first
    // step's combinator; both leading forms start the selector at the first tag.
    let mut css: Vec<String> = Vec::new();
    let mut rest = path;
    let mut first = true;
    while !rest.is_empty() {
        // Consume the axis separator.
        let descendant = if let Some(r) = rest.strip_prefix("//") {
            rest = r;
            true
        } else if let Some(r) = rest.strip_prefix('/') {
            rest = r;
            false
        } else {
            // Interior text with no separator should not happen (steps are split
            // on `/`), but guard against a malformed path.
            return Err(format!("xpath: malformed path near `{rest}` in `{whole}`"));
        };
        // The step is everything up to the next `/`.
        let end = rest.find('/').unwrap_or(rest.len());
        let (step, tail) = rest.split_at(end);
        rest = tail;
        let sel = step_to_css(step, whole)?;
        if first {
            // A leading `/root` or `//root` both anchor at a descendant match of
            // the first step (arb parses the whole stream as one document).
            css.push(sel);
            first = false;
        } else if descendant {
            css.push(format!(" {sel}"));
        } else {
            css.push(format!(" > {sel}"));
        }
    }
    if css.is_empty() {
        return Err(format!("xpath: empty location path in `{whole}`"));
    }
    Ok(css.concat())
}

/// One location step → a CSS type/universal selector, optionally with a single
/// `[@attr]` existence predicate. Rejects every richer predicate/function/axis.
fn step_to_css(step: &str, whole: &str) -> Result<String, String> {
    if step.is_empty() {
        return Err(format!("xpath: empty step in `{whole}` (a stray `/`?)"));
    }
    // Split off an optional `[…]` predicate.
    let (tag, pred) = match step.find('[') {
        Some(b) => {
            if !step.ends_with(']') {
                return Err(format!("xpath: malformed predicate in step `{step}`"));
            }
            (&step[..b], Some(&step[b + 1..step.len() - 1]))
        }
        None => (step, None),
    };
    // The tag: a bare name or the `*` wildcard. No namespaces, no functions.
    let tag_css = if tag == "*" {
        "*".to_string()
    } else if is_ident(tag) {
        tag.to_string()
    } else if tag.contains("::") || tag == ".." || tag == "." {
        return Err(format!("xpath: axis `{tag}` is not supported (subset: `//`, `/`, `@`)"));
    } else if tag.contains('(') {
        return Err(format!("xpath: function `{tag}` is not supported"));
    } else {
        return Err(format!("xpath: unsupported step `{step}` in `{whole}`"));
    };
    let Some(pred) = pred else {
        return Ok(tag_css);
    };
    // The only supported predicate is `[@attr]` (attribute exists). Positional
    // (`[1]`), value (`[@a='b']`), and function (`[last()]`, `[text()=…]`)
    // predicates are out of subset.
    let attr = pred.strip_prefix('@').ok_or_else(|| {
        format!("xpath: only the `[@attr]` existence predicate is supported, not `[{pred}]`")
    })?;
    if !is_ident(attr) {
        return Err(format!("xpath: unsupported predicate `[{pred}]` (only `[@attr]`)"));
    }
    Ok(format!("{tag_css}[{attr}]"))
}

/// An attribute/tag name: a non-empty run of `[A-Za-z0-9_-]`, no namespace colon,
/// no wildcard, no quotes.
fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Validate a trailing `@attr` name, erroring on `@*` / namespaced / empty.
fn validate_name(name: &str, whole: &str) -> Result<String, String> {
    if is_ident(name) {
        Ok(name.to_string())
    } else {
        Err(format!("xpath: unsupported attribute name in `{whole}` (use `@name`)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{eval, QueryOp, QueryResult};

    fn ops(src: &str) -> Vec<QueryOp> {
        translate(src).unwrap()
    }

    /// Render an xpath literal over one HTML document, returning the output lines.
    fn run(src: &str, html: &str) -> Vec<String> {
        match eval(&translate(src).unwrap(), &[html.to_string()], 0.0) {
            QueryResult::Lines(v) => v,
            other => panic!("expected Lines, got {other:?}"),
        }
    }

    #[test]
    fn descendant_and_child_paths() {
        assert!(matches!(ops("//a").as_slice(), [QueryOp::Find(s)] if s == "a"));
        assert!(matches!(ops("//div//span").as_slice(), [QueryOp::Find(s)] if s == "div span"));
        assert!(matches!(ops("//div/span").as_slice(), [QueryOp::Find(s)] if s == "div > span"));
        assert!(matches!(ops("/html/body/p").as_slice(), [QueryOp::Find(s)] if s == "html > body > p"));
    }

    #[test]
    fn attr_predicate_and_accessors() {
        assert!(matches!(ops("//a[@href]").as_slice(), [QueryOp::Find(s)] if s == "a[href]"));
        assert!(matches!(ops("//a/@href").as_slice(), [QueryOp::Find(s), QueryOp::Attr(a)] if s == "a" && a == "href"));
        assert!(matches!(ops("//a/text()").as_slice(), [QueryOp::Find(s), QueryOp::Text] if s == "a"));
        assert!(matches!(ops("@href").as_slice(), [QueryOp::Attr(a)] if a == "href"));
        assert!(matches!(ops("//*").as_slice(), [QueryOp::Find(s)] if s == "*"));
    }

    #[test]
    fn end_to_end_over_html() {
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
    fn unsupported_constructs_error_not_mishandle() {
        for bad in [
            "//a[1]",              // positional predicate
            "//a[@href='x']",      // value predicate
            "//a[last()]",         // function predicate
            "//following-sibling::b", // axis
            "//..",               // parent axis
            ".",                   // relative/context (jq territory, not xpath here)
            "//a|//b",             // union
            "count(//a)",          // function
            "//ns:a",              // namespace
            "@*",                  // wildcard attribute
            "//a[text()='x']",     // text predicate
            "foo",                 // not an xpath literal at all
        ] {
            assert!(translate(bad).is_err(), "expected `{bad}` to error");
        }
    }
}
