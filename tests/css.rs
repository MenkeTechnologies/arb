//! The CSS leg of the query engine: `sel { … }`.
//!
//! Two failures are pinned here, both of the same shape as the xpath leg's — an
//! answer that looked like an answer:
//!
//! * A selector `scraper` could not PARSE mapped to an empty result, so
//!   `a[href=/x]` — which no CSS engine accepts — was indistinguishable from a
//!   selector that legitimately matches nothing.
//! * An attribute value in DOUBLE quotes was unquoted by arb's command lexer and
//!   never re-quoted, so `a[href="/x"]` reached the selector parser as
//!   `a[href=/x]` and hit the case above. CSS accepts `'` and `"` alike.
//!
//! Each expectation is the selection of the equivalent XPath, taken from
//! `xmllint --html --xpath` on the same document; the translation is stated per
//! case and kept trivial so it cannot smuggle in a difference of its own.

use arb::parser::parse;
use arb::query::{eval, QueryResult};

const DOC: &str = r#"<html><body><div class="card"><h2>Title</h2><a href="/x" rel="nf">X</a></div><a href="/y">Y</a><ul><li>one</li><li>two</li></ul></body></html>"#;

fn sel(css: &str) -> Result<Vec<String>, String> {
    let src = format!("tail .x\nsource .x {{ in.html; sel {{ {css} }} }}");
    let cmds = parse(&src).map_err(|e| e.to_string())?;
    let spec = arb::spec::build(&cmds).map_err(|e| e.to_string())?;
    let ops = spec.widgets[0]
        .source
        .as_ref()
        .ok_or("no source")?
        .pipeline
        .clone();
    match eval(&ops, &[DOC.to_string()], 0.0) {
        QueryResult::Lines(l) => Ok(l),
        QueryResult::Error(e) => Err(e),
        other => Err(format!("{other:?}")),
    }
}

/// An attribute value may be written with EITHER quote, as in CSS itself.
///
/// References, `xmllint --html --xpath` on the same document:
///   `//a[@href='/x']/text()`  -> `X`
///   `//a[@rel='nf']/text()`   -> `X`
///   `//div[@class='card']//h2/text()` -> `Title`
#[test]
fn an_attribute_value_may_be_double_quoted() {
    for css in [r#"a[href="/x"]"#, "a[href='/x']"] {
        assert_eq!(
            sel(css).unwrap_or_else(|e| panic!("`{css}` failed: {e}")),
            vec!["X"],
            "`{css}` must select the same element as //a[@href='/x']"
        );
    }
    assert_eq!(sel(r#"a[rel="nf"]"#).unwrap(), vec!["X"]);
    assert_eq!(sel(r#"div[class="card"] h2"#).unwrap(), vec!["Title"]);
    // A value that is a bare identifier was never affected, and still is not.
    assert_eq!(sel("li").unwrap(), vec!["one", "two"]);
}

/// A MALFORMED selector is an error, not "no matches".
///
/// Each of these is rejected by `scraper` (Selectors 3/4 via `cssparser`), so
/// answering anything at all for one — including an empty node list — is the
/// silent reinterpretation SPEC §8 rules out.
#[test]
fn a_malformed_selector_is_an_error_not_an_empty_match() {
    for css in ["a[href=/x]", "div..card", "a[[href]]", ">", "a:::hover"] {
        let r = sel(css);
        assert!(r.is_err(), "`{css}` must be refused, got {r:?}");
        let msg = r.unwrap_err();
        assert!(
            msg.contains("not a valid CSS selector"),
            "`{css}` must say WHY, got: {msg}"
        );
    }
}

/// A selector that is well-formed and legitimately matches nothing is still an
/// empty answer — the point is that the two cases are now distinguishable, not
/// that everything errors.
#[test]
fn a_valid_selector_matching_nothing_is_still_empty() {
    assert_eq!(sel("table").unwrap(), Vec::<String>::new());
    assert_eq!(sel(r#"a[href="/nope"]"#).unwrap(), Vec::<String>::new());
}
