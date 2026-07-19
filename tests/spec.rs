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
    assert_eq!(
        listed,
        vec![("mydash".to_string(), "my dashboard".to_string())]
    );

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
    assert_eq!(
        s.widgets[0].opts.get("label").map(String::as_str),
        Some("hi")
    );
    assert_eq!(
        s.widgets[0].opts.get("max").map(String::as_str),
        Some("100")
    );
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
fn every_stdlib_preset_carries_passing_self_tests() {
    // Each bundled preset ships `test { … }` blocks that exercise its query
    // pipelines; run them here so a preset whose transform silently breaks fails
    // CI (the in-language `arb --test`, enforced in Rust).
    for name in arb::spec::STDLIB_NAMES {
        let spec = build(&parse(&format!("import {name}")).unwrap())
            .unwrap_or_else(|e| panic!("preset `{name}` failed to build: {e}"));
        let report = arb::testrun::run(&spec.tests);
        assert!(
            !spec.tests.is_empty(),
            "preset `{name}` has no in-language tests"
        );
        assert_eq!(
            report.failed, 0,
            "preset `{name}` has failing self-tests:\n{}",
            report.text
        );
    }
}

#[test]
fn list_presets_includes_stdlib() {
    let names: Vec<String> = arb::spec::list_presets()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
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
fn import_as_namespaces_widget_paths() {
    // `import X as g` prefixes every imported widget path with `.g.`.
    let dir = temp_lib("ns-paths");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("mod.arb");
    std::fs::write(&file, "gauge .cpu -max 100\ntail .stream").unwrap();
    let s = build(&parse(&format!("import \"{}\" as g", file.display())).unwrap()).unwrap();
    assert!(s.widgets.iter().any(|w| w.path == ".g.cpu"));
    assert!(s.widgets.iter().any(|w| w.path == ".g.stream"));
    assert!(!s.widgets.iter().any(|w| w.path == ".cpu"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn import_as_prefixes_control_predicate_end_to_end() {
    use std::collections::HashMap;
    // A module's `where lat < .th` control ref is namespaced to `.g.th`, so it
    // resolves against the `g.th` input, not the bare `th`.
    let dir = temp_lib("ns-pred");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("mod.arb");
    std::fs::write(&file, "input .th\nout { in; where lat < .th }").unwrap();
    let s = build(&parse(&format!("import \"{}\" as g", file.display())).unwrap()).unwrap();
    assert!(s.widgets.iter().any(|w| w.path == ".g.th"));
    let ops = s.out.as_ref().unwrap();
    let data = vec![r#"{"lat":2}"#.to_string(), r#"{"lat":9}"#.to_string()];
    // Namespaced key `g.th` drives the filter.
    let mut inputs = HashMap::new();
    inputs.insert("g.th".to_string(), "5".to_string());
    match arb::query::eval(&arb::spec::resolve_pipeline(ops, &inputs), &data, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, vec![r#"{"lat":2}"#]),
        other => panic!("got {other:?}"),
    }
    // The un-prefixed key `th` no longer matches -> control unset -> filter dropped.
    let mut bare = HashMap::new();
    bare.insert("th".to_string(), "5".to_string());
    match arb::query::eval(&arb::spec::resolve_pipeline(ops, &bare), &data, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, data),
        other => panic!("got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn import_as_nested_namespaces_compose() {
    let dir = temp_lib("ns-nest");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("b.arb"), "gauge .x -max 1").unwrap();
    let a = dir.join("a.arb");
    std::fs::write(
        &a,
        format!("import \"{}\" as b", dir.join("b.arb").display()),
    )
    .unwrap();
    let s = build(&parse(&format!("import \"{}\" as a", a.display())).unwrap()).unwrap();
    assert!(
        s.widgets.iter().any(|w| w.path == ".a.b.x"),
        "paths: {:?}",
        s.widgets.iter().map(|w| &w.path).collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn import_as_alias_errors() {
    // Missing / invalid alias are rejected; plain import still works.
    assert!(build(&parse("import nums as").unwrap()).is_err());
    assert!(build(&parse("import nums as bad.name").unwrap()).is_err());
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
fn where_match_control_filters_by_substring() {
    use std::collections::HashMap;
    // `where match(.q)` keeps lines containing the filter control's text.
    let s = build(&parse("filter .q\nout { in; where match(.q) }").unwrap()).unwrap();
    let ops = s.out.as_ref().unwrap();
    let data = vec![
        "apple".to_string(),
        "banana".to_string(),
        "grape".to_string(),
    ];
    let mut inputs = HashMap::new();
    inputs.insert("q".to_string(), "ap".to_string());
    match arb::query::eval(&arb::spec::resolve_pipeline(ops, &inputs), &data, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, vec!["apple", "grape"]),
        other => panic!("got {other:?}"),
    }
    // Empty control -> matches everything (no filter).
    let empty: HashMap<String, String> = HashMap::new();
    match arb::query::eval(&arb::spec::resolve_pipeline(ops, &empty), &data, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, data),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn where_field_in_set_filters_by_facet_selection() {
    use std::collections::HashMap;
    // `where level in .lv` keeps records whose `level` is in the selected set.
    let s =
        build(&parse("facet .lv -field level\nout { in; where level in .lv }").unwrap()).unwrap();
    let ops = s.out.as_ref().unwrap();
    let data = vec![
        r#"{"level":"info","m":"a"}"#.to_string(),
        r#"{"level":"error","m":"b"}"#.to_string(),
        r#"{"level":"warn","m":"c"}"#.to_string(),
    ];
    let mut inputs = HashMap::new();
    inputs.insert("lv".to_string(), "error,warn".to_string());
    match arb::query::eval(&arb::spec::resolve_pipeline(ops, &inputs), &data, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(
                ls,
                vec![
                    r#"{"level":"error","m":"b"}"#,
                    r#"{"level":"warn","m":"c"}"#
                ]
            );
        }
        other => panic!("got {other:?}"),
    }
    // Empty selection -> no filter.
    let empty: HashMap<String, String> = HashMap::new();
    match arb::query::eval(&arb::spec::resolve_pipeline(ops, &empty), &data, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, data),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn where_combines_string_and_set_predicates() {
    use std::collections::HashMap;
    let s = build(
        &parse("filter .q\nfacet .lv -field level\nout { in; where match(.q) and level in .lv }")
            .unwrap(),
    )
    .unwrap();
    let ops = s.out.as_ref().unwrap();
    let data = vec![
        r#"{"level":"error","m":"disk"}"#.to_string(),
        r#"{"level":"info","m":"disk"}"#.to_string(),
        r#"{"level":"error","m":"net"}"#.to_string(),
    ];
    let mut inputs = HashMap::new();
    inputs.insert("q".to_string(), "disk".to_string());
    inputs.insert("lv".to_string(), "error".to_string());
    match arb::query::eval(&arb::spec::resolve_pipeline(ops, &inputs), &data, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec![r#"{"level":"error","m":"disk"}"#])
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn parse_scalar_numbers_durations_sizes() {
    use arb::spec::parse_scalar;
    assert_eq!(parse_scalar("42"), 42.0);
    assert_eq!(parse_scalar("5s"), 5.0);
    assert_eq!(parse_scalar("500ms"), 0.5);
    assert_eq!(parse_scalar("2m"), 120.0);
    assert_eq!(parse_scalar("1kb"), 1024.0);
    assert_eq!(parse_scalar("bad"), 0.0);
}

#[test]
fn control_widgets_recognized() {
    let s = build(
        &parse(
            "filter .q\nfacet .lv -opts {a b c}\nslider .th -min 0 -max 5\ncheck .on -label live",
        )
        .unwrap(),
    )
    .unwrap();
    use arb::spec::WidgetKind;
    assert_eq!(s.widgets[0].kind, WidgetKind::Filter);
    assert_eq!(s.widgets[1].kind, WidgetKind::Facet);
    assert_eq!(s.widgets[2].kind, WidgetKind::Slider);
    assert_eq!(s.widgets[3].kind, WidgetKind::Check);
    // facet -opts {a b c} lands as a comma-joined option list.
    assert_eq!(
        s.widgets[1].opts.get("opts").map(String::as_str),
        Some("a,b,c")
    );
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
    assert_eq!(
        arb::tui::project_line(gp, "an err here"),
        vec!["an err here"]
    );

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
fn parses_beep_alert_exec_flash_actions() {
    use arb::spec::BindAction;
    let s = build(
        &parse(
            "bind C-b beep\n\
             bind C-a alert disk full\n\
             expect /panic/ exec notify-send arb\n\
             expect /5\\d\\d/ flash .log red",
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(s.binds[0].action, BindAction::Beep);
    assert_eq!(s.binds[1].action, BindAction::Alert("disk full".into()));
    assert_eq!(
        s.expects[0].action,
        BindAction::Exec("notify-send arb".into())
    );
    assert_eq!(
        s.expects[1].action,
        BindAction::Flash {
            widget: "log".into(),
            color: "red".into()
        }
    );
    // exec with no command, and an unknown action, are rejected.
    assert!(build(&parse("bind C-x exec").unwrap()).is_err());
    assert!(build(&parse("bind C-x frobnicate").unwrap()).is_err());
}

#[test]
fn bind_routes_mouse_and_resize_events() {
    use arb::spec::{BindAction, MouseTrigger};
    let s = build(
        &parse(
            "bind <Click> quit\nbind <Resize> beep\nbind C-q quit\nbind <Click> { alert x; beep }",
        )
        .unwrap(),
    )
    .unwrap();
    // <Click> -> mouse_binds, <Resize> -> resize_binds, C-q -> binds (routing
    // must precede parse_key so the angle-words never error).
    assert_eq!(s.mouse_binds.len(), 2);
    assert_eq!(s.mouse_binds[0], (MouseTrigger::Click, BindAction::Quit));
    assert!(matches!(s.mouse_binds[1].1, BindAction::Seq(_)));
    assert_eq!(s.resize_binds, vec![BindAction::Beep]);
    assert_eq!(s.binds.len(), 1);
    assert_eq!(s.binds[0].action, BindAction::Quit);
    // A bogus angle-word still errors.
    assert!(build(&parse("bind <Bogus> quit").unwrap()).is_err());
}

#[test]
fn parses_block_form_action_sequence() {
    use arb::spec::BindAction;
    // `expect /re/ { alert "x"; beep }` -> a Seq of the two actions.
    let s = build(&parse("expect /5\\d\\d/ { alert 5xx; beep }").unwrap()).unwrap();
    match &s.expects[0].action {
        BindAction::Seq(v) => {
            assert_eq!(v.len(), 2);
            assert_eq!(v[0], BindAction::Alert("5xx".into()));
            assert_eq!(v[1], BindAction::Beep);
        }
        other => panic!("expected Seq, got {other:?}"),
    }
    // Single-line form still yields a scalar action, not a Seq.
    let s2 = build(&parse("bind C-q quit").unwrap()).unwrap();
    assert_eq!(s2.binds[0].action, BindAction::Quit);
}

#[test]
fn timeout_parses_duration_and_action() {
    use arb::spec::BindAction;
    use std::time::Duration;
    let s = build(
        &parse("timeout 5s quit\ntimeout 500ms beep\ntimeout 2m alert idle\ntimeout 3s { alert x; beep }").unwrap(),
    )
    .unwrap();
    assert_eq!(s.timeouts.len(), 4);
    assert_eq!(s.timeouts[0].dur, Duration::from_secs(5));
    assert_eq!(s.timeouts[0].action, BindAction::Quit);
    assert_eq!(s.timeouts[1].dur, Duration::from_millis(500));
    assert_eq!(s.timeouts[1].action, BindAction::Beep);
    assert_eq!(s.timeouts[2].dur, Duration::from_secs(120));
    assert_eq!(s.timeouts[2].action, BindAction::Alert("idle".into()));
    assert!(matches!(s.timeouts[3].action, BindAction::Seq(ref v) if v.len() == 2));
    // errors: bad duration, unknown action, missing action.
    assert!(build(&parse("timeout xyz quit").unwrap()).is_err());
    assert!(build(&parse("timeout 5s frobnicate").unwrap()).is_err());
    assert!(build(&parse("timeout 5s").unwrap()).is_err());
}

#[test]
fn parse_duration_units() {
    use arb::spec::parse_duration;
    use std::time::Duration;
    assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
    assert_eq!(parse_duration("1.5s"), Some(Duration::from_secs_f64(1.5)));
    assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
    assert_eq!(parse_duration("bad"), None);
    assert_eq!(parse_duration(""), None);
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
        BindAction::SetInput {
            name: "x".into(),
            value: "upper".into()
        }
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
        BindAction::SetInput {
            name: "x".into(),
            value: "field 2".into()
        }
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
    let s = build(&parse("input .x\nexpect /ERROR/ set .x upper\nexpect /shutdown/ quit").unwrap())
        .unwrap();
    assert_eq!(s.expects.len(), 2);
    // The pattern is a real regex fired against stream lines.
    assert!(s.expects[0].pattern.is_match("2026 ERROR disk full"));
    assert!(!s.expects[0].pattern.is_match("all good"));
    assert_eq!(
        s.expects[0].action,
        BindAction::SetInput {
            name: "x".into(),
            value: "upper".into()
        }
    );
    assert!(s.expects[1].pattern.is_match("graceful shutdown"));
    assert_eq!(s.expects[1].action, BindAction::Quit);
}

#[test]
fn expect_rejects_unknown_action() {
    assert!(build(&parse("input .x\nexpect /re/ frobnicate .x").unwrap()).is_err());
}

#[test]
fn expect_block_two_clauses_two_expects() {
    use arb::spec::BindAction;
    // Tcl Expect's multi-pattern block: one `expect { }`, several /re/ ACTION
    // clauses, each becoming a live Expect.
    let s = build(&parse("tail .log\nexpect { /panic/ quit\n/5\\d\\d/ flash .log red }").unwrap())
        .unwrap();
    assert_eq!(s.expects.len(), 2);
    assert!(s.expects[0].pattern.is_match("kernel panic"));
    assert_eq!(s.expects[0].action, BindAction::Quit);
    assert!(s.expects[1].pattern.is_match("503 unavailable"));
    assert_eq!(
        s.expects[1].action,
        BindAction::Flash {
            widget: "log".into(),
            color: "red".into()
        }
    );
}

#[test]
fn expect_block_nested_action_seq() {
    use arb::spec::BindAction;
    // A clause action may itself be a `{ … }` block → a Seq of actions.
    let s = build(&parse("tail .l\nexpect { /err/ { alert boom; beep } }").unwrap()).unwrap();
    assert_eq!(s.expects.len(), 1);
    assert!(matches!(s.expects[0].action, BindAction::Seq(_)));
}

#[test]
fn expect_block_bad_regex_errors() {
    assert!(build(&parse("tail .l\nexpect { /(unterminated/ quit }").unwrap()).is_err());
}

#[test]
fn expect_empty_block_no_expects() {
    let s = build(&parse("tail .l\nexpect { }").unwrap()).unwrap();
    assert!(s.expects.is_empty());
}

#[test]
fn expect_single_clause_still_builds() {
    use arb::spec::BindAction;
    // Regression: the single-clause form is unchanged by the block branch.
    let s = build(&parse("tail .l\nexpect /panic/ quit").unwrap()).unwrap();
    assert_eq!(s.expects.len(), 1);
    assert_eq!(s.expects[0].action, BindAction::Quit);
    assert!(s.expects[0].pattern.is_match("panic!"));
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
    let s =
        build(&parse("select .f\nsource .f { in }\nsearch .f { in; field 1 }").unwrap()).unwrap();
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
    let s = build(
        &parse("select .files -prompt \"pick> \" -header files\nsource .files { in }").unwrap(),
    )
    .unwrap();
    assert_eq!(s.widgets.len(), 1);
    assert_eq!(s.widgets[0].kind, WidgetKind::Select);
    assert_eq!(
        s.widgets[0].opts.get("prompt").map(String::as_str),
        Some("pick> ")
    );
    assert_eq!(
        s.widgets[0].opts.get("header").map(String::as_str),
        Some("files")
    );
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

#[test]
fn spawn_sets_source() {
    // `spawn CMD` declares an input source; no widget source / stdin required.
    let s = build(&parse("tail .t\nspawn seq 1 5").unwrap()).unwrap();
    assert_eq!(s.spawn.as_deref(), Some("seq 1 5"));
}

#[test]
fn spawn_block_form() {
    let s = build(&parse("tail .t\nspawn { ps aux }").unwrap()).unwrap();
    assert_eq!(s.spawn.as_deref(), Some("ps aux"));
}

#[test]
fn spawn_block_multi_cmd() {
    // Several commands in the block join with `; ` into one shell string.
    let s = build(&parse("tail .t\nspawn { tail -f a.log; grep err }").unwrap()).unwrap();
    assert_eq!(s.spawn.as_deref(), Some("tail -f a.log; grep err"));
}

#[test]
fn spawn_empty_errors() {
    assert!(build(&parse("spawn").unwrap()).is_err());
}

#[test]
fn spawn_double_errors() {
    assert!(build(&parse("spawn a\nspawn b").unwrap()).is_err());
}

#[test]
fn spawn_coexists_with_source_pipeline() {
    // A `spawn` source and a widget `source { … }` pipeline are independent.
    let s = build(&parse("tail .t\nspawn seq 1 3\nsource .t { in; count }").unwrap()).unwrap();
    assert_eq!(s.spawn.as_deref(), Some("seq 1 3"));
    assert!(s
        .widgets
        .iter()
        .any(|w| w.path == ".t" && w.source.is_some()));
}

#[test]
fn spawn_from_import_merges() {
    // A module that declares `spawn` carries it across an aliased import merge.
    let dir = temp_lib("spawn-import");
    std::fs::create_dir_all(&dir).unwrap();
    let module = dir.join("src.arb");
    std::fs::write(&module, "spawn seq 1 2\n").unwrap();
    // The path is quoted so the lexer reads it as one string (an unquoted
    // absolute path collides with `/…/` regex-token lexing).
    let spec_src = format!("tail .t\nimport \"{}\" as m", module.display());
    let s = build(&parse(&spec_src).unwrap()).unwrap();
    assert_eq!(s.spawn.as_deref(), Some("seq 1 2"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn spawn_conflict_across_imports_errors() {
    // A local `spawn` plus an imported module that also spawns is a conflict.
    let dir = temp_lib("spawn-conflict");
    std::fs::create_dir_all(&dir).unwrap();
    let module = dir.join("src.arb");
    std::fs::write(&module, "spawn seq 1 2\n").unwrap();
    let spec_src = format!("spawn echo hi\nimport \"{}\" as m", module.display());
    assert!(build(&parse(&spec_src).unwrap()).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_sets_source() {
    let s = build(&parse("tail .t\n< file.log").unwrap()).unwrap();
    assert_eq!(s.source_file.as_deref(), Some("file.log"));
}

#[test]
fn file_empty_errors() {
    assert!(build(&parse("<").unwrap()).is_err());
}

#[test]
fn file_double_errors() {
    assert!(build(&parse("< a.log\n< b.log").unwrap()).is_err());
}

#[test]
fn file_conflicts_with_spawn() {
    // Only one stream source allowed, in either order.
    assert!(build(&parse("spawn seq 1 3\n< a.log").unwrap()).is_err());
    assert!(build(&parse("< a.log\nspawn seq 1 3").unwrap()).is_err());
}

#[test]
fn file_from_import_merges() {
    let dir = temp_lib("file-import");
    std::fs::create_dir_all(&dir).unwrap();
    let module = dir.join("src.arb");
    std::fs::write(&module, "< data.log\n").unwrap();
    // Quote the absolute path (unquoted `/a/b` collides with `/…/` regex lexing).
    let spec_src = format!("tail .t\nimport \"{}\" as m", module.display());
    let s = build(&parse(&spec_src).unwrap()).unwrap();
    assert_eq!(s.source_file.as_deref(), Some("data.log"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn poll_sets_source() {
    use std::time::Duration;
    let s = build(&parse("tail .t\n! vmstat 1 every 1s").unwrap()).unwrap();
    assert_eq!(
        s.poll.as_ref().unwrap(),
        &("vmstat 1".to_string(), Duration::from_secs(1))
    );
}

#[test]
fn poll_block_form() {
    use std::time::Duration;
    let s = build(&parse("tail .t\n! { ps aux } every 2s").unwrap()).unwrap();
    assert_eq!(
        s.poll.as_ref().unwrap(),
        &("ps aux".to_string(), Duration::from_secs(2))
    );
}

#[test]
fn poll_ms_interval() {
    use std::time::Duration;
    let s = build(&parse("tail .t\n! date every 500ms").unwrap()).unwrap();
    assert_eq!(s.poll.as_ref().unwrap().1, Duration::from_millis(500));
}

#[test]
fn poll_missing_every_errors() {
    assert!(build(&parse("! vmstat 1").unwrap()).is_err());
}

#[test]
fn poll_bad_duration_errors() {
    assert!(build(&parse("! date every soon").unwrap()).is_err());
}

#[test]
fn poll_empty_cmd_errors() {
    assert!(build(&parse("! every 1s").unwrap()).is_err());
}

#[test]
fn poll_conflicts_with_spawn() {
    assert!(build(&parse("spawn seq 1 3\n! date every 1s").unwrap()).is_err());
    assert!(build(&parse("! date every 1s\nspawn seq 1 3").unwrap()).is_err());
}

#[test]
fn poll_conflicts_with_file() {
    assert!(build(&parse("< a.log\n! date every 1s").unwrap()).is_err());
}

#[test]
fn poll_double_errors() {
    assert!(build(&parse("! a every 1s\n! b every 2s").unwrap()).is_err());
}

#[test]
fn test_blocks_parse_into_spec_tests() {
    let s = build(
        &parse("tail .l\ntest \"t1\" { given \"a\" \"b\"; run { in; count }; want \"2\" }")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(s.tests.len(), 1);
    assert_eq!(s.tests[0].name, "t1");
    assert_eq!(s.tests[0].given, vec!["a", "b"]);
    assert_eq!(s.tests[0].want, vec!["2"]);
    // A test with no `run` clause is a build error.
    assert!(build(&parse("test \"x\" { given \"a\"; want \"1\" }").unwrap()).is_err());
    // An unknown clause errors (fail-closed).
    assert!(build(&parse("test \"x\" { given \"a\"; run { in }; bogus \"y\" }").unwrap()).is_err());
}

#[test]
fn spawn_pty_flag_and_send_action() {
    use arb::spec::BindAction;
    // `spawn -pty CMD` sets spawn + the pty flag; plain spawn does not.
    let s = build(&parse("tail .t\nspawn -pty seq 1 3").unwrap()).unwrap();
    assert_eq!(s.spawn.as_deref(), Some("seq 1 3"));
    assert!(s.spawn_pty);
    let s2 = build(&parse("tail .t\nspawn seq 1 3").unwrap()).unwrap();
    assert!(!s2.spawn_pty);
    // `send "text"` parses to a Send action; the lexer already turns \n into a
    // real newline, and empty send errors.
    let s3 = build(&parse("tail .t\nbind C-y send \"yes\\n\"").unwrap()).unwrap();
    assert!(matches!(&s3.binds[0].action, BindAction::Send(t) if t == "yes\n"));
    assert!(build(&parse("bind C-y send").unwrap()).is_err());
}

// Adversarial-audit regression: the block parser recursed once per nested `{ }`
// with no depth guard, so a pathologically nested spec overflowed the stack and
// SIGABRT'd the process. It must now fail closed with an error (a passing run is
// the proof — an abort would kill the whole test binary).
#[test]
fn deeply_nested_blocks_error_not_abort() {
    let deep = format!("bind C-a {}quit{}", "{ ".repeat(6000), " }".repeat(6000));
    let err = parse(&deep).expect_err("deep block nesting must be rejected");
    assert!(
        err.msg.contains("too deeply nested"),
        "expected a depth-limit error, got: {}",
        err.msg
    );
}

// Adversarial-audit regression: the block lexer counted raw `{`/`}` without
// honoring string/regex literals, so a `}` inside a `/regex/` or `"string"` in a
// block body closed the block early and silently dropped the rest of the body.
#[test]
fn brace_inside_regex_or_string_does_not_truncate_block() {
    // `/x}/` — the `}` is inside a regex literal; the block must stay intact so
    // the `grep` op keeps the FULL pattern (a truncated block would either error
    // or drop to a shorter regex). `grep` compiles to a Match op.
    let s = build(&parse("tail .x\nsource .x { in; grep /x}/ }").unwrap()).unwrap();
    let ops = &s.widgets[0].source.as_ref().unwrap().pipeline;
    let dbg = format!("{ops:?}");
    assert!(dbg.contains("x}"), "regex brace survived: {dbg}");
    // `}` inside a "string" likewise must not truncate the block: the prepend op
    // must carry the whole `a}b`, brace included.
    let s2 = build(&parse("tail .x\nsource .x { in; prepend \"a}b\" }").unwrap()).unwrap();
    let dbg2 = format!("{:?}", s2.widgets[0].source.as_ref().unwrap().pipeline);
    assert!(dbg2.contains("a}b"), "string brace survived: {dbg2}");
}

// Round-4 audit regression: grid -row/-col/-rowspan/-colspan were parsed as usize
// with no upper bound, so `compute_rects` overflowed (row+rowspan near usize::MAX)
// or built a multi-million-cell layout (huge span) that hung the TUI. The parsed
// coords/spans must be clamped so the layout stays bounded.
#[test]
fn grid_coords_and_spans_are_clamped() {
    let s = build(&parse("gauge .g\ngrid .g -row 18446744073709551615 -colspan 50000000").unwrap())
        .unwrap();
    let w = s.widgets.iter().find(|w| w.path == ".g").unwrap();
    let (row, _col) = w.grid.expect("grid set");
    let (_rs, cs) = w.span;
    assert!(row <= 256, "row clamped: {row}");
    assert!(cs <= 256, "colspan clamped: {cs}");
}
