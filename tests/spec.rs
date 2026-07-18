//! Parser + interpreter tests — headless, CI-safe (no terminal touched).

use arb::parser::parse;
use arb::spec::{build, WidgetKind};

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
    let src = s.widgets[0].source.as_ref().unwrap();
    assert_eq!(src.pipeline.len(), 0);
}

#[test]
fn bind_shorthand_binds_stdin() {
    let s = build(&parse("tail .x\n.x <- in").unwrap()).unwrap();
    assert!(s.widgets[0].source.is_some());
}

#[test]
fn source_pipeline_ops_count() {
    let s = build(&parse("tail .x\nsource .x { in; match /err/; count }").unwrap()).unwrap();
    assert_eq!(s.widgets[0].source.as_ref().unwrap().pipeline.len(), 2);
}

#[test]
fn source_requires_in() {
    assert!(build(&parse("tail .x\nsource .x { count }").unwrap()).is_err());
}

#[test]
fn source_unknown_verb_errors() {
    assert!(build(&parse("tail .x\nsource .x { in; bogus }").unwrap()).is_err());
}

#[test]
fn comments_and_semicolons() {
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
fn grid_command_places_widgets() {
    let s = build(
        &parse("gauge .a\ngauge .b\ntail .c\ngrid .a -row 0 -col 0\ngrid .b -row 0 -col 1\ngrid .c -row 1 -col 0").unwrap(),
    )
    .unwrap();
    assert_eq!(s.widgets[0].grid, Some((0, 0)));
    assert_eq!(s.widgets[1].grid, Some((0, 1)));
    assert_eq!(s.widgets[2].grid, Some((1, 0)));
}

#[test]
fn grid_for_missing_widget_errors() {
    assert!(build(&parse("grid .nope -row 0 -col 0").unwrap()).is_err());
}

#[test]
fn import_stdlib_preset_instantiates_widgets() {
    let s = build(&parse("import nums").unwrap()).unwrap();
    assert!(s.widgets.iter().any(|w| w.path == ".avg"));
    assert!(s.widgets.iter().any(|w| w.path == ".max"));
}

#[test]
fn all_stdlib_presets_build() {
    for name in arb::spec::STDLIB_NAMES {
        assert!(
            build(&parse(&format!("import {name}")).unwrap()).is_ok(),
            "preset `{name}` failed to build"
        );
    }
}

#[test]
fn list_presets_includes_stdlib() {
    let names: Vec<String> = arb::spec::list_presets().into_iter().map(|(n, _)| n).collect();
    for want in ["nums", "logs", "http", "json", "table", "top"] {
        assert!(names.contains(&want.to_string()), "missing preset {want}");
    }
}

#[test]
fn import_unknown_module_errors() {
    assert!(build(&parse("import nope").unwrap()).is_err());
}

#[test]
fn import_then_extend_with_own_widget() {
    let s = build(&parse("import nums\ngauge .mine -max 5").unwrap()).unwrap();
    assert!(s.widgets.iter().any(|w| w.path == ".mine"));
    assert!(s.widgets.len() >= 4);
}

#[test]
fn out_block_defines_downstream_pipeline() {
    let s = build(&parse("out { in; match /ERROR/ }").unwrap()).unwrap();
    assert!(s.out.is_some());
    assert_eq!(s.out.as_ref().unwrap().len(), 1);
}

#[test]
fn out_and_widgets_coexist() {
    let s = build(&parse("tail .t; source .t { in }; out { in; field 1 }").unwrap()).unwrap();
    assert_eq!(s.widgets.len(), 1);
    assert!(s.out.is_some());
}

#[test]
fn unknown_verb_ignored() {
    let s = build(&parse("frobnicate .x\ntext .a").unwrap()).unwrap();
    assert_eq!(s.widgets.len(), 1);
    assert_eq!(s.widgets[0].path, ".a");
}

#[test]
fn regex_literal_spans_quotes_and_spaces() {
    // A `/.../` regex containing quotes and spaces must lex as one token (was
    // "unterminated string" before the lexer learned regex literals).
    let src = "tail .t\nsource .t { in; match /\" (4|5)[0-9][0-9] / }";
    let spec = build(&parse(src).unwrap()).expect("quote/space regex should build");
    assert_eq!(spec.widgets.len(), 1);
}

#[test]
fn apply_resolves_input_pipeline() {
    use std::collections::HashMap;
    // A source using `apply .q` — the input .q holds a transform pipeline.
    let s = build(&parse("list .after\nsource .after { in; apply .q }").unwrap()).unwrap();
    let ops = &s.widgets[0].source.as_ref().unwrap().pipeline;
    // With .q = "field 2; upper", the resolved pipeline applies field+upper.
    let mut inputs = HashMap::new();
    inputs.insert("q".to_string(), "field 2; upper".to_string());
    let resolved = arb::spec::resolve_pipeline(ops, &inputs);
    let lines = vec!["a bob".to_string(), "c dan".to_string()];
    match arb::query::eval(&resolved, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, vec!["BOB", "DAN"]),
        other => panic!("got {other:?}"),
    }
    // Empty input → no transform (identity).
    let empty: HashMap<String, String> = HashMap::new();
    let resolved0 = arb::spec::resolve_pipeline(ops, &empty);
    match arb::query::eval(&resolved0, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, lines),
        other => panic!("got {other:?}"),
    }
}
