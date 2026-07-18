//! Parser + interpreter tests — headless, CI-safe (no terminal touched).

use arb::parser::parse;
use arb::spec::{build, Source, WidgetKind};

#[test]
fn parses_widgets_with_opts() {
    let s = build(&parse("text .a -label hi -max 100\ntail .b").unwrap()).unwrap();
    assert_eq!(s.widgets.len(), 2);
    assert_eq!(s.widgets[0].path, ".a");
    assert_eq!(s.widgets[0].kind, WidgetKind::Text);
    assert_eq!(s.widgets[0].opts.get("label").map(String::as_str), Some("hi"));
    assert_eq!(s.widgets[0].opts.get("max").map(String::as_str), Some("100"));
    assert_eq!(s.widgets[1].kind, WidgetKind::Tail);
}

#[test]
fn source_block_binds_stdin() {
    let s = build(&parse("tail .b\nsource .b { in }").unwrap()).unwrap();
    assert_eq!(s.widgets[0].source, Some(Source::Stdin));
}

#[test]
fn bind_shorthand_binds_stdin() {
    let s = build(&parse("tail .x\n.x <- in").unwrap()).unwrap();
    assert_eq!(s.widgets[0].source, Some(Source::Stdin));
}

#[test]
fn comments_and_semicolons() {
    // `;#` = separator then a comment; both widgets still parse.
    let s = build(&parse("# header\ntext .a ;# trailing note\ntail .b").unwrap()).unwrap();
    assert_eq!(s.widgets.len(), 2);
}

#[test]
fn quoted_string_opt_value() {
    let s = build(&parse("text .a -label \"hello world\"").unwrap()).unwrap();
    assert_eq!(
        s.widgets[0].opts.get("label").map(String::as_str),
        Some("hello world")
    );
}

#[test]
fn rejects_non_dot_path() {
    assert!(build(&parse("text foo").unwrap()).is_err());
}

#[test]
fn source_for_missing_widget_errors() {
    assert!(build(&parse("source .nope { in }").unwrap()).is_err());
}

#[test]
fn unknown_verb_ignored() {
    // A future/unknown verb should not break older specs.
    let s = build(&parse("frobnicate .x\ntext .a").unwrap()).unwrap();
    assert_eq!(s.widgets.len(), 1);
    assert_eq!(s.widgets[0].path, ".a");
}
