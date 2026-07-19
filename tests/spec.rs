//! Parser + interpreter tests — headless, CI-safe (no terminal touched).

use arb::parser::parse;
use arb::spec::{build, WidgetKind};

/// A unique temp directory for a package-manager test (no tempfile dep). The
/// caller removes it; `label` keeps parallel tests from colliding.
fn temp_lib(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("arb-pm-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn install_list_and_uninstall_preset() {
    use arb::spec::{install_preset, list_user_presets, uninstall_preset};
    let dir = temp_lib("roundtrip");
    let src = "# my dashboard\ngauge .g -max 100\nsource .g { in; count }";
    let path = install_preset(&dir, "mydash", src).expect("install ok");
    assert!(path.exists());

    let listed = list_user_presets(&dir);
    assert_eq!(listed, vec![("mydash".to_string(), "my dashboard".to_string())]);

    assert!(uninstall_preset(&dir, "mydash").unwrap());
    assert!(!uninstall_preset(&dir, "mydash").unwrap()); // already gone
    assert!(list_user_presets(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn install_rejects_invalid_spec_and_bad_name() {
    use arb::spec::install_preset;
    let dir = temp_lib("reject");
    // Invalid spec is never written to the library.
    assert!(install_preset(&dir, "bad", "gauge .x {").is_err());
    assert!(!dir.join("bad.arb").exists());
    // Names that would escape the library are rejected.
    assert!(install_preset(&dir, "../evil", "gauge .g").is_err());
    assert!(install_preset(&dir, "a/b", "gauge .g").is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

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
fn import_absolute_arb_path_resolves() {
    // A path-like name ending in `.arb` is read verbatim — the resolver must not
    // append a second `.arb` (regression: `import "/x.arb"` -> "/x.arb.arb").
    let dir = temp_lib("import-path");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("mylib.arb");
    std::fs::write(&file, "gauge .fromfile -max 7").unwrap();
    let src = format!("import \"{}\"", file.display());
    let s = build(&parse(&src).unwrap()).unwrap();
    assert!(s.widgets.iter().any(|w| w.path == ".fromfile"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn configure_merges_widget_opts() {
    // `.g configure -max 200` retunes an already-declared widget (later wins).
    let s = build(&parse("gauge .g -max 100\n.g configure -max 200 -color red").unwrap()).unwrap();
    let w = s.widgets.iter().find(|w| w.path == ".g").unwrap();
    assert_eq!(w.opts.get("max").map(String::as_str), Some("200"));
    assert_eq!(w.opts.get("color").map(String::as_str), Some("red"));
    // configure on an unknown widget errors; a `<- in` bind still works.
    assert!(build(&parse(".nope configure -max 1").unwrap()).is_err());
    assert!(build(&parse("tail .x\n.x <- in").unwrap()).is_ok());
}

#[test]
fn import_as_alias_rejected_clearly() {
    // `import X as Y` namespacing is unbuilt; reject instead of silently dropping.
    let e = build(&parse("import nums as g").unwrap()).unwrap_err();
    assert!(e.contains("as"), "error should mention `as`: {e}");
    assert!(build(&parse("import nums").unwrap()).is_ok());
}

#[test]
fn parse_key_tk_named_keys() {
    use arb::spec::parse_key;
    assert_eq!(parse_key("<Enter>"), Some(0x0d));
    assert_eq!(parse_key("<Esc>"), Some(0x1b));
    assert_eq!(parse_key("<Tab>"), Some(0x09));
    assert_eq!(parse_key("<Key-q>"), Some(b'q'));
    assert_eq!(parse_key("<Key-qq>"), None); // must be a single letter
    assert_eq!(parse_key("C-u"), Some(0x15)); // control forms still parse
}

#[test]
fn tabs_widget_captures_labels_from_block() {
    // `-tabs {a b}` -> the widget's opts carry a comma-joined word list.
    let s = build(&parse("tabs .t -tabs {alpha beta}").unwrap()).unwrap();
    let w = s.widgets.iter().find(|w| w.path == ".t").unwrap();
    assert_eq!(w.opts.get("tabs").map(String::as_str), Some("alpha,beta"));
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
fn control_path_numeric_predicate_filters_by_live_value() {
    use std::collections::HashMap;
    // SPEC §12: a control path used as a value = its live state. `where lat < .th`
    // filters the passthrough by the `.th` control's current number.
    let s = build(&parse("input .th\nout { in; where lat < .th }").unwrap()).unwrap();
    let ops = s.out.as_ref().unwrap();
    let data = vec![r#"{"lat":2}"#.to_string(), r#"{"lat":9}"#.to_string()];

    // th = 5 -> keep only lat < 5.
    let mut inputs = HashMap::new();
    inputs.insert("th".to_string(), "5".to_string());
    match arb::query::eval(&arb::spec::resolve_pipeline(ops, &inputs), &data, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, vec![r#"{"lat":2}"#]),
        other => panic!("got {other:?}"),
    }
    // th unset -> the filter is dropped entirely (unset threshold = no filter).
    let empty: HashMap<String, String> = HashMap::new();
    match arb::query::eval(&arb::spec::resolve_pipeline(ops, &empty), &data, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, data),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn control_ref_parses_but_plain_field_predicate_still_works() {
    // `.th` is a control ref; a bareword field predicate must be unaffected.
    assert!(build(&parse("input .th\nout { in; where lat < .th }").unwrap()).is_ok());
    assert!(build(&parse("out { in; where price > 10 }").unwrap()).is_ok());
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

#[test]
fn color_hex_maps_names_case_insensitively() {
    use arb::spec::color_hex;
    assert_eq!(color_hex(Some("green")), "#00e676");
    assert_eq!(color_hex(Some("RED")), "#ff5252");
    assert_eq!(color_hex(Some(" Yellow ")), "#ffd740");
    assert_eq!(color_hex(Some("grey")), "#9e9e9e");
    assert_eq!(color_hex(None), "#00e5ff"); // default cyan
    assert_eq!(color_hex(Some("bogus")), "#00e5ff"); // unknown → default
}
