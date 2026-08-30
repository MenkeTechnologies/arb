//! The XPath 1.0 engine, against `xmllint` (libxml2 20913).
//!
//! Every expectation in `REFERENCE` was CAPTURED from a real
//! `xmllint --html --xpath EXPR` run over `FIXTURE`, never written from memory.
//! One normalization is applied, the same one `scripts/jq_parity.sh` documents:
//! xmllint prints an attribute NODE (` href="/x"`) where arb's stream emits the
//! VALUE (`/x`). It is applied per line, so values and their order are both
//! still compared and no selection difference can hide inside it.
//!
//! Headless and CI-safe: nothing here shells out, so the suite does not need
//! xmllint on the machine to run — the reference is baked in above.

use arb::parser::parse;
use arb::query::{eval, QueryResult};

const FIXTURE: &str = r#"<html lang="en-US"><body><div id="main" class="card"><h2>Title</h2><span>inner</span><a href="/x" rel="nf">X</a></div><div class="other"><p>alpha</p><p>beta</p><p>gamma</p></div><a href="/y">Y</a><ul><li>one</li><li>two</li><li>three</li></ul></body></html>"#;

/// Run one body command over `FIXTURE` and return its output lines.
fn run(expr: &str) -> Vec<String> {
    let src = format!("tail .x\nsource .x {{ in.html; {expr} }}");
    let cmds = parse(&src).unwrap_or_else(|e| panic!("`{expr}` did not parse: {e}"));
    let spec = arb::spec::build(&cmds).unwrap_or_else(|e| panic!("`{expr}` did not build: {e}"));
    let ops = spec.widgets[0]
        .source
        .as_ref()
        .expect("source")
        .pipeline
        .clone();
    match eval(&ops, &[FIXTURE.to_string()], 0.0) {
        QueryResult::Lines(l) => l,
        other => panic!("`{expr}` -> {other:?}"),
    }
}

/// Does the build refuse this expression?
fn refused(expr: &str) -> bool {
    let src = format!("tail .x\nsource .x {{ in.html; {expr} }}");
    match parse(&src) {
        Ok(cmds) => arb::spec::build(&cmds).is_err(),
        Err(_) => true,
    }
}

/// `(expression, what xmllint answered)`. All 13 axes, the node tests, the
/// positional-predicate rules, the 27-function core library, the operators, and
/// the four constructs that used to answer WRONG under a zero exit.
const REFERENCE: &[(&str, &[&str])] = &[
    ("//span/parent::div/@id", &["main"]),
    ("//span/ancestor::div/@id", &["main"]),
    ("//span/ancestor-or-self::span/text()", &["inner"]),
    ("//div/child::h2/text()", &["Title"]),
    ("//div/descendant::p/text()", &["alpha", "beta", "gamma"]),
    ("//div/descendant-or-self::h2/text()", &["Title"]),
    ("//p/following-sibling::p/text()", &["beta", "gamma"]),
    ("//p/preceding-sibling::p/text()", &["alpha", "beta"]),
    ("//h2/following::p/text()", &["alpha", "beta", "gamma"]),
    ("//ul/preceding::h2/text()", &["Title"]),
    ("//div/attribute::id", &["main"]),
    ("//div/self::div/@id", &["main"]),
    (
        "//li/..",
        &["<ul><li>one</li><li>two</li><li>three</li></ul>"],
    ),
    ("//p[1]/text()", &["alpha"]),
    ("//p[2]/text()", &["beta"]),
    ("//p[last()]/text()", &["gamma"]),
    ("//p[position()=2]/text()", &["beta"]),
    ("//p[last()-1]/text()", &["beta"]),
    ("//li[position()>1]/text()", &["two", "three"]),
    ("//p/following-sibling::p[1]/text()", &["beta", "gamma"]),
    ("//p/preceding-sibling::p[1]/text()", &["alpha", "beta"]),
    ("//a[@href]/text()", &["X", "Y"]),
    ("//a[not(@rel)]/text()", &["Y"]),
    ("//a[@href='/x']/text()", &["X"]),
    ("//a[@href!='/x']/text()", &["Y"]),
    ("//a[@href='/z' or @href='/y']/text()", &["Y"]),
    ("//div[@class='card'][@id='main']/h2/text()", &["Title"]),
    ("//a[text()='X']/@href", &["/x"]),
    ("//a[contains(text(),'X')]/@href", &["/x"]),
    ("//@href", &["/x", "/y"]),
    ("//a/@*", &["/x", "nf", "/y"]),
    ("//comment()", &[]),
    ("count(//p)", &["3"]),
    ("count(//*)", &["15"]),
    ("count(//text())", &["10"]),
    ("count(//node())", &["25"]),
    ("name(//span)", &["span"]),
    ("local-name(//span)", &["span"]),
    ("string(//h2)", &["Title"]),
    ("concat(\"a\",\"b\",\"c\")", &["abc"]),
    ("starts-with(\"abc\",\"ab\")", &["true"]),
    ("substring-before(\"a-b\",\"-\")", &["a"]),
    ("substring-after(\"a-b\",\"-\")", &["b"]),
    ("substring(\"hello\",2,3)", &["ell"]),
    ("string-length(\"abcd\")", &["4"]),
    ("normalize-space(\"  a   b  \")", &["a b"]),
    ("translate(\"abc\",\"abc\",\"xyz\")", &["xyz"]),
    ("boolean(//p)", &["true"]),
    ("not(//nosuch)", &["true"]),
    ("true()", &["true"]),
    ("false()", &["false"]),
    ("lang(\"en\")", &["false"]),
    ("number(\"42\")", &["42"]),
    ("sum(//p)", &["NaN"]),
    ("floor(1.7)", &["1"]),
    ("ceiling(1.2)", &["2"]),
    ("round(1.5)", &["2"]),
    ("round(-1.5)", &["-1"]),
    ("(1+2)*3", &["9"]),
    (
        "//h2|//li",
        &[
            "<h2>Title</h2>",
            "<li>one</li>",
            "<li>two</li>",
            "<li>three</li>",
        ],
    ),
    ("/li/text()", &[]),
    ("/div/h2/text()", &[]),
    ("/html/body/div/h2/text()", &["Title"]),
];

#[test]
fn the_engine_answers_what_xmllint_answers() {
    let mut bad = Vec::new();
    for (expr, want) in REFERENCE {
        let got = run(expr);
        let want: Vec<String> = want.iter().map(|s| (*s).to_string()).collect();
        if got != want {
            bad.push(format!(
                "  {expr}\n    arb    : {got:?}\n    xmllint: {want:?}"
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} diverged:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// The four constructs that answered WRONG under a ZERO exit before this engine
/// existed, called out separately because a wrong answer that looks like an
/// answer is the one outcome SPEC §8 rules out.
///
/// Two answered EMPTY where XPath selects nodes (`or` in a predicate, and a
/// chained predicate), and two answered a NON-EMPTY node set where XPath selects
/// nothing (a rooted path, which the old front-end compiled to a CSS descendant
/// match). All four are in `REFERENCE` above as well; this test is the statement
/// of intent, so deleting it would be conspicuous.
#[test]
fn the_four_silent_wrong_answers_are_gone() {
    // Answered empty; xmllint selects both headings.
    assert_eq!(
        run("//div[@class='card' or @class='other']/@class"),
        vec!["card", "other"]
    );
    // Answered empty; xmllint selects the card.
    assert_eq!(
        run("//div[@class='card'][@id='main']/h2/text()"),
        vec!["Title"]
    );
    // Answered three `<li>`s; xmllint selects NOTHING, because the document's
    // root child is `html` and `/li` asks for a `li` child of the ROOT.
    assert!(run("/li/text()").is_empty());
    assert!(run("/div/h2/text()").is_empty());
    // ...and the rooted path that IS correct still works, so the fix is not
    // "refuse everything rooted".
    assert_eq!(run("/html/body/div/h2/text()"), vec!["Title"]);
}

/// Whether an expression is evaluated PER LINE or once over the whole document
/// follows the union's branches, not the fact that it is a union.
///
/// Only a relative location path composes with the previous pipeline step. A
/// union was classified as one document question outright, so `@href|@id` — two
/// relative steps — lost its context node and selected NOTHING while either
/// branch alone selected.
///
/// Asserted on the classification rather than end to end, because
/// `scripts/jq_parity.sh` cannot reach it: every xpath probe there is compared
/// against `xmllint --xpath`, which evaluates from the DOCUMENT root and so
/// cannot express a relative context. Every union probed there is absolute for
/// that reason — which is why this went unnoticed.
#[test]
fn per_line_follows_a_unions_branches() {
    let per_line = |src: &str| -> bool {
        match arb::xpath::translate(src)
            .unwrap_or_else(|e| panic!("`{src}` did not translate: {e}"))
            .first()
        {
            Some(arb::query::QueryOp::XPath(x)) => x.per_line,
            other => panic!("`{src}` -> {other:?}"),
        }
    };

    // A relative path composes per line; so does a union of relative paths.
    assert!(per_line("@href"));
    assert!(per_line("@href|@id"));
    assert!(per_line("h2/text()|p/text()"));

    // An absolute path is one question about the document, and so is a union of
    // them — this is the case that was already right.
    assert!(!per_line("//a/@href"));
    assert!(!per_line("//a/@href|//a/@rel"));
    assert!(!per_line("/html/body/div/h2/text()"));

    // A MIXED union stays document-scoped: there is one context and the
    // absolute branch does not want it.
    assert!(!per_line("@href|//a/@rel"));
    assert!(!per_line("//a/@rel|@href"));

    // And the rule this classification exists for is untouched: a function call
    // is answered once, not once per input line.
    assert!(!per_line("count(//p)"));
    assert!(!per_line("normalize-space(//a)"));
}

/// A union still selects both branches end to end, and a branch that selects
/// nothing contributes nothing rather than emptying the result.
#[test]
fn a_union_selects_every_branch() {
    assert_eq!(run("//a/@href"), vec!["/x", "/y"]);
    assert_eq!(run("//a/@rel"), vec!["nf"]);
    assert_eq!(
        run("//h2/text()|//p/text()"),
        vec!["Title", "alpha", "beta", "gamma"]
    );
    assert_eq!(run("//h2/text()|//nosuch/text()"), vec!["Title"]);
    // The per-line rule still answers a document question once.
    assert_eq!(run("count(//p)"), vec!["3"]);
}

/// A predicate on a REVERSE axis counts proximity along THAT axis (§2.4), so
/// `preceding-sibling::p[1]` is the NEAREST preceding sibling, not the first in
/// document order. Getting this backwards is the classic XPath engine bug and it
/// is invisible unless the axis has three or more nodes.
#[test]
fn a_predicate_on_a_reverse_axis_counts_along_that_axis() {
    // `beta` and `gamma` each have a nearest preceding `p`: `alpha`, `beta`.
    assert_eq!(
        run("//p/preceding-sibling::p[1]/text()"),
        vec!["alpha", "beta"]
    );
    // The forward twin, for contrast.
    assert_eq!(
        run("//p/following-sibling::p[1]/text()"),
        vec!["beta", "gamma"]
    );
    // `[last()]` on the reverse axis is the FARTHEST one.
    assert_eq!(
        run("//p[3]/preceding-sibling::p[last()]/text()"),
        vec!["alpha"]
    );
}

/// MALFORMED xpath is a hard error at BUILD time, before any input is read.
/// This is the property that makes the engine safe to trust: it can be wrong
/// about nothing silently.
#[test]
fn malformed_xpath_is_refused_at_build_time() {
    for bad in [
        "//a[",            // unclosed predicate
        "//a[@href",       // unclosed predicate
        "count(//a",       // unclosed call
        "//bogus-axis::a", // not one of the 13 axes
        "//a/@href extra", // trailing junk after a complete expression
        "//a[@class=]",    // missing operand
        "//nosuchfn(1)",   // not a core function
        "//a[@class='x]",  // unterminated literal
    ] {
        assert!(refused(bad), "`{bad}` must be refused");
    }
}

/// `contains` is the ONE core-function spelling jq already answers, so at arb's
/// command position it stays jq's — XPath is tried only as the last resort, and
/// a language matched first would SHADOW the other (the `keys`/`names` lesson).
/// Nothing is lost: XPath's two-argument `contains` is reachable where it is
/// actually used, inside a predicate, and that is checked against xmllint here
/// (`xmllint --html --xpath "//p[contains(text(),'lph')]/text()"` prints
/// `alpha`).
#[test]
fn contains_stays_jqs_at_command_position_and_xpaths_inside_a_predicate() {
    assert_eq!(run("//p[contains(text(),'lph')]/text()"), vec!["alpha"]);
    assert_eq!(run("//a[contains(@href,'y')]/@href"), vec!["/y"]);
    // The bare spelling reaches jq, which answers for its own one-argument form
    // rather than XPath's two-argument one. It must NOT be an error, and it must
    // NOT be XPath's answer.
    assert!(!refused("contains(\"abc\",\"bc\")"));
}

/// A bare NCName is a legal XPath relative location path, and at arb's COMMAND
/// position that would be a trap: a typo'd verb would parse as `child::typo`,
/// select nothing and exit 0 — reintroducing the silent answer through the front
/// door. It stays an `unknown verb` diagnostic, and the step is still reachable
/// under a spelling that is unambiguous.
#[test]
fn a_bare_name_stays_a_verb_not_an_xpath_step() {
    assert!(refused("bogusverb"));
    // The same step, spelled so it can only be xpath.
    assert_eq!(run("//h2/text()"), vec!["Title"]);
    assert_eq!(run("descendant::h2/text()"), vec!["Title"]);
}

/// XPath's operators, exercised where they are actually used: inside a
/// predicate. At arb's COMMAND position a bare `7 div 2` carries no
/// xpath-only character, so it stays a verb name (see the test above) —
/// parenthesize it, or use it in a predicate, both of which are unambiguous.
#[test]
fn arithmetic_and_comparison_operators() {
    assert_eq!(run("(1+2)*3"), vec!["9"]);
    assert_eq!(run("(7 div 2)"), vec!["3.5"]);
    assert_eq!(run("(7 mod 2)"), vec!["1"]);
    assert_eq!(run("(-1.5)"), vec!["-1.5"]);
    // `round()` breaks ties toward POSITIVE infinity (§4.4), where Rust's
    // `f64::round` breaks them away from zero: `round(-1.5)` is -1, not -2.
    assert_eq!(run("round(-1.5)"), vec!["-1"]);
    assert_eq!(run("round(1.5)"), vec!["2"]);
    // Comparison inside a predicate, over a node-set (existential, §3.4).
    assert_eq!(run("//li[position()>=2]/text()"), vec!["two", "three"]);
    assert_eq!(run("//p[position()=last()-1]/text()"), vec!["beta"]);
    // A node-set compared to a string is existential: true when SOME node
    // matches, which is why `!=` is not the negation of `=` here.
    assert_eq!(run("//a[@href!='/x']/@href"), vec!["/y"]);
}

/// The `following` and `preceding` axes over NESTED structure.
///
/// Both are computed from the fact that a subtree is CONTIGUOUS in document
/// order: `following` is the slice at or after the end of the context node's
/// subtree, and `preceding` is the slice before it minus the nodes whose
/// subtree end is past it (its ancestors). That replaced a per-context-node
/// rebuild-and-sort of the whole document, which was quadratic — over a table
/// of 400 `td`s `//td/following::td` took 0.17s and over 1600 it took 2.28s,
/// 4x the nodes for 13x the time; it is 0.03s and 0.47s now, same answers.
///
/// The assumption needs DEPTH to exercise, so this fixture nests. References,
/// `xmllint --html --xpath` on it:
///   `//i/following::p/text()`     -> 4, 5, 6
///   `//i/preceding::p/text()`     -> 1
///   `//span/following::p/text()`  -> 4, 5, 6
///   `//span/preceding::p/text()`  -> 1   (its own descendants excluded)
///   `//b/ancestor::div/@id`       -> a
#[test]
fn following_and_preceding_over_nested_structure() {
    const NEST: &str = r#"<html><body><div id="a"><p>1</p><span><b>2</b><i>3</i></span><p>4</p></div><div id="b"><p>5</p></div><p>6</p></body></html>"#;
    let run_on = |expr: &str| -> Vec<String> {
        let src = format!("tail .x\nsource .x {{ in.html; {expr} }}");
        let cmds = parse(&src).unwrap_or_else(|e| panic!("`{expr}`: {e}"));
        let spec = arb::spec::build(&cmds).unwrap_or_else(|e| panic!("`{expr}`: {e}"));
        let ops = spec.widgets[0]
            .source
            .as_ref()
            .expect("source")
            .pipeline
            .clone();
        match eval(&ops, &[NEST.to_string()], 0.0) {
            QueryResult::Lines(l) => l,
            other => panic!("`{expr}` -> {other:?}"),
        }
    };
    assert_eq!(run_on("//i/following::p/text()"), vec!["4", "5", "6"]);
    assert_eq!(run_on("//i/preceding::p/text()"), vec!["1"]);
    // A context node's own DESCENDANTS are excluded from both axes, which is
    // the half a flat fixture cannot catch.
    assert_eq!(run_on("//span/following::p/text()"), vec!["4", "5", "6"]);
    assert_eq!(run_on("//span/preceding::p/text()"), vec!["1"]);
    assert_eq!(run_on("//b/ancestor::div/@id"), vec!["a"]);
}

/// A CDATA section is CHARACTER DATA (§5.7), not a node type of its own.
///
/// html5ever implements the HTML5 tree-construction algorithm, where CDATA is
/// legal only in foreign content (SVG/MathML) and is otherwise a bogus COMMENT,
/// so `<a><![CDATA[x<y&z]]></a>` lost its content outright. References,
/// `xmllint --xpath` on that document:
///   `string(//a)`          -> `x<y&z`
///   `string-length(//a)`   -> `5`
///   `count(//a/text())`    -> `1`
///   `substring(//a,2,3)`   -> `<y&`
/// arb answered ``, `0`, `0` — silently dropping input, which is the failure
/// this engine exists to remove.
#[test]
fn a_cdata_section_is_character_data() {
    const CD: &str = "<r><a><![CDATA[x<y&z]]></a><b>plain</b></r>";
    let on = |expr: &str| -> Vec<String> {
        let src = format!("tail .x\nsource .x {{ in.xml; {expr} }}");
        let cmds = parse(&src).unwrap_or_else(|e| panic!("`{expr}`: {e}"));
        let spec = arb::spec::build(&cmds).unwrap_or_else(|e| panic!("`{expr}`: {e}"));
        let ops = spec.widgets[0]
            .source
            .as_ref()
            .expect("source")
            .pipeline
            .clone();
        match eval(&ops, &[CD.to_string()], 0.0) {
            QueryResult::Lines(l) => l,
            other => panic!("`{expr}` -> {other:?}"),
        }
    };
    assert_eq!(on("string(//a)"), vec!["x<y&z"]);
    assert_eq!(on("string-length(//a)"), vec!["5"]);
    assert_eq!(on("count(//a/text())"), vec!["1"]);
    assert_eq!(on("substring(//a,2,3)"), vec!["<y&"]);
    // The sibling is untouched, so the rewrite is not eating the document.
    assert_eq!(on("count(//b)"), vec!["1"]);
    assert_eq!(on("string(//b)"), vec!["plain"]);
}

/// Entities are decoded into the STRING-VALUE, and both engines agree there.
///
/// They disagree only in SERIALIZATION — `xmllint --xpath` re-escapes when it
/// prints a text node, so its `//p/text()` shows `a&amp;b` where arb's stream
/// emits the raw `a&b`. That is an output convention, not a data model, and
/// these probe the MODEL so neither engine's escaping is involved. References,
/// `xmllint --html --xpath` on the fixture below: `16`, `a&b <`, `1`, `true`.
#[test]
fn entities_are_decoded_into_the_string_value() {
    const ENT: &str = r#"<html><body><p id="e">a&amp;b &lt;tag&gt; &#65; &nbsp;end</p><p title="x&amp;y">t</p></body></html>"#;
    let on = |expr: &str| -> Vec<String> {
        let src = format!("tail .x\nsource .x {{ in.html; {expr} }}");
        let cmds = parse(&src).unwrap_or_else(|e| panic!("`{expr}`: {e}"));
        let spec = arb::spec::build(&cmds).unwrap_or_else(|e| panic!("`{expr}`: {e}"));
        let ops = spec.widgets[0]
            .source
            .as_ref()
            .expect("source")
            .pipeline
            .clone();
        match eval(&ops, &[ENT.to_string()], 0.0) {
            QueryResult::Lines(l) => l,
            other => panic!("`{expr}` -> {other:?}"),
        }
    };
    assert_eq!(on(r#"string-length(//p[@id="e"])"#), vec!["16"]);
    assert_eq!(on(r#"substring(//p[@id="e"],1,5)"#), vec!["a&b <"]);
    assert_eq!(on(r#"count(//p[contains(text(),"<tag>")])"#), vec!["1"]);
    assert_eq!(on(r#"starts-with(//p[@id="e"],"a&b")"#), vec!["true"]);
    // An entity in an ATTRIBUTE value decodes the same way.
    assert_eq!(on(r#"string-length(//p/@title)"#), vec!["3"]);
    assert_eq!(on(r#"count(//p[@title="x&y"])"#), vec!["1"]);
}
