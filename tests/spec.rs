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

#[test]
fn select_projection_maps_display_while_keeping_original() {
    // `--with-nth`: the select source projects the display; project_line applies
    // it per raw line. `field 2` -> the 2nd field; `grep` -> drops non-matches.
    let s = build(&parse("select .f\nsource .f { in; field 2 }").unwrap()).unwrap();
    let proj = &s.widgets[0].source.as_ref().unwrap().pipeline;
    assert_eq!(arb::tui::project_line(proj, "alice 42 x"), vec!["42"]);

    // A filtering projection yields zero display rows for a non-match (the raw
    // line drops out of the candidate list) and one for a match.
    let g = build(&parse("select .f\nsource .f { in; grep /err/ }").unwrap()).unwrap();
    let gp = &g.widgets[0].source.as_ref().unwrap().pipeline;
    assert!(arb::tui::project_line(gp, "all ok").is_empty());
    assert_eq!(arb::tui::project_line(gp, "an err here"), vec!["an err here"]);

    // Empty pipeline = identity.
    assert_eq!(arb::tui::project_line(&[], "raw line"), vec!["raw line"]);
}

#[test]
fn parse_key_control_specs() {
    use arb::spec::parse_key;
    assert_eq!(parse_key("C-u"), Some(0x15)); // Ctrl-U
    assert_eq!(parse_key("c-r"), Some(0x12)); // Ctrl-R
    assert_eq!(parse_key("^a"), Some(0x01)); // Ctrl-A
    assert_eq!(parse_key("u"), None); // bare printable — not a control key
    assert_eq!(parse_key("C-1"), None); // non-letter
    assert_eq!(parse_key("C-uu"), None); // more than one letter
}

#[test]
fn bind_parses_set_and_quit() {
    use arb::spec::BindAction;
    let s = build(
        &parse("input .x\nbind C-u set .x upper\nbind C-q quit\nout { in; apply .x }").unwrap(),
    )
    .unwrap();
    assert_eq!(s.binds.len(), 2);
    assert_eq!(s.binds[0].key, 0x15);
    assert_eq!(
        s.binds[0].action,
        BindAction::SetInput { name: "x".into(), value: "upper".into() }
    );
    assert_eq!(s.binds[1].key, 0x11); // Ctrl-Q
    assert_eq!(s.binds[1].action, BindAction::Quit);
}

#[test]
fn bind_set_joins_multi_token_value() {
    use arb::spec::BindAction;
    let s = build(&parse("input .x\nbind C-f set .x field 2").unwrap()).unwrap();
    assert_eq!(
        s.binds[0].action,
        BindAction::SetInput { name: "x".into(), value: "field 2".into() }
    );
}

#[test]
fn bind_rejects_non_control_key_and_unknown_action() {
    assert!(build(&parse("bind u quit").unwrap()).is_err());
    assert!(build(&parse("input .x\nbind C-u frobnicate .x").unwrap()).is_err());
}

#[test]
fn expect_parses_pattern_and_action() {
    use arb::spec::BindAction;
    let s = build(
        &parse("input .x\nexpect /ERROR/ set .x upper\nexpect /shutdown/ quit").unwrap(),
    )
    .unwrap();
    assert_eq!(s.expects.len(), 2);
    // The pattern is a real regex fired against stream lines.
    assert!(s.expects[0].pattern.is_match("2026 ERROR disk full"));
    assert!(!s.expects[0].pattern.is_match("all good"));
    assert_eq!(
        s.expects[0].action,
        BindAction::SetInput { name: "x".into(), value: "upper".into() }
    );
    assert!(s.expects[1].pattern.is_match("graceful shutdown"));
    assert_eq!(s.expects[1].action, BindAction::Quit);
}

#[test]
fn expect_rejects_unknown_action() {
    assert!(build(&parse("input .x\nexpect /re/ frobnicate .x").unwrap()).is_err());
}

#[test]
fn interactive_out_maps_stream_with_live_input() {
    use std::collections::HashMap;
    // The megafilter/map: `out { in; apply .x }` resolved against a live input,
    // then applied per line (exactly what the tee reader does downstream).
    let s = build(&parse("input .x\ntail .t\nsource .t { in }\nout { in; apply .x }").unwrap())
        .unwrap();
    let out = s.out.as_ref().expect("out pipeline");

    // Input `.x = "upper"` → each piped line is uppercased downstream.
    let mut inputs = HashMap::new();
    inputs.insert("x".to_string(), "upper".to_string());
    let resolved = arb::spec::resolve_pipeline(out, &inputs);
    assert!(arb::query::is_line_streamable(&resolved));
    assert_eq!(arb::tui::project_line(&resolved, "bob"), vec!["BOB"]);

    // Empty input → identity passthrough (the pipe is unchanged until you type).
    let resolved0 = arb::spec::resolve_pipeline(out, &HashMap::new());
    assert_eq!(arb::tui::project_line(&resolved0, "bob"), vec!["bob"]);

    // A filtering transform drops lines from the downstream pipe live.
    inputs.insert("x".to_string(), "grep /o/".to_string());
    let resolved_g = arb::spec::resolve_pipeline(out, &inputs);
    assert_eq!(arb::tui::project_line(&resolved_g, "bob"), vec!["bob"]);
    assert!(arb::tui::project_line(&resolved_g, "alice").is_empty());
}

#[test]
fn search_binding_sets_widget_search_key() {
    // fzf `--nth`: `search .f { in; field 1 }` derives the fuzzy key while the row
    // shows/emits the full display. The search pipeline is stored on the widget
    // and applies per line via project_line.
    let s = build(&parse("select .f\nsource .f { in }\nsearch .f { in; field 1 }").unwrap())
        .unwrap();
    let w = &s.widgets[0];
    assert!(w.source.is_some());
    let key = w.search.as_ref().expect("search pipeline stored");
    assert_eq!(arb::tui::project_line(key, "alice 42"), vec!["alice"]);
    // Display projection is identity here → shows the whole line.
    assert_eq!(
        arb::tui::project_line(&w.source.as_ref().unwrap().pipeline, "alice 42"),
        vec!["alice 42"]
    );
}

#[test]
fn search_binding_unknown_widget_errors() {
    assert!(build(&parse("select .f\nsearch .g { in; field 1 }").unwrap()).is_err());
}

#[test]
fn select_widget_parses_with_opts() {
    // fzf-as-a-spec: a `select` widget with prompt/header opts.
    let s = build(&parse("select .files -prompt \"pick> \" -header files\nsource .files { in }").unwrap())
        .unwrap();
    assert_eq!(s.widgets.len(), 1);
    assert_eq!(s.widgets[0].kind, WidgetKind::Select);
    assert_eq!(s.widgets[0].opts.get("prompt").map(String::as_str), Some("pick> "));
    assert_eq!(s.widgets[0].opts.get("header").map(String::as_str), Some("files"));
    assert!(s.widgets[0].source.is_some());
}
