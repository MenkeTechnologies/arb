//! Query-pipeline evaluation tests, driven through the spec parser so the whole
//! `source { … }` -> ops -> eval path is exercised. Headless, CI-safe.

use arb::parser::parse;
use arb::query::{eval, QueryOp, QueryResult};
use arb::spec::build;

fn pipeline(spec_src: &str) -> Vec<QueryOp> {
    let s = build(&parse(spec_src).unwrap()).unwrap();
    s.widgets[0].source.as_ref().unwrap().pipeline.clone()
}

fn lines(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn match_then_count() {
    let ops = pipeline("tail .x\nsource .x { in; match /err/; count }");
    assert_eq!(
        eval(&ops, &lines(&["err a", "ok", "err b"]), 1.0),
        QueryResult::Scalar(2.0)
    );
}

#[test]
fn field_extracts_column() {
    let ops = pipeline("tail .x\nsource .x { in; field 2 }");
    assert_eq!(
        eval(&ops, &lines(&["a b c", "x y z"]), 1.0),
        QueryResult::Lines(lines(&["b", "y"]))
    );
}

#[test]
fn reject_drops_matches() {
    let ops = pipeline("tail .x\nsource .x { in; reject /skip/ }");
    assert_eq!(
        eval(&ops, &lines(&["keep", "skip me", "keep2"]), 1.0),
        QueryResult::Lines(lines(&["keep", "keep2"]))
    );
}

#[test]
fn rate_uses_elapsed() {
    let ops = pipeline("tail .x\nsource .x { in; rate }");
    assert_eq!(
        eval(&ops, &lines(&["a", "b", "c", "d"]), 2.0),
        QueryResult::Scalar(2.0)
    );
}

#[test]
fn field_then_tally_groups_sorted() {
    let ops = pipeline("bars .x\nsource .x { in; field 1; tally }");
    assert_eq!(
        eval(&ops, &lines(&["a x", "a y", "b z", "c w", "a q"]), 1.0),
        QueryResult::Pairs(vec![("a".into(), 3), ("b".into(), 1), ("c".into(), 1)])
    );
}

#[test]
fn sel_extracts_element_text() {
    let ops = pipeline("tail .x\nsource .x { in.html; sel h1 }");
    let html = lines(&["<html><body><h1>Hello</h1><h1>World</h1></body></html>"]);
    assert_eq!(
        eval(&ops, &html, 1.0),
        QueryResult::Lines(lines(&["Hello", "World"]))
    );
}

#[test]
fn sel_class_then_tally() {
    let ops = pipeline("bars .x\nsource .x { in.html; sel .tag; tally }");
    let html = lines(&[
        r#"<div><span class="tag">a</span><span class="tag">a</span><span class="tag">b</span></div>"#,
    ]);
    assert_eq!(
        eval(&ops, &html, 1.0),
        QueryResult::Pairs(vec![("a".into(), 2), ("b".into(), 1)])
    );
}

#[test]
fn numeric_aggregates() {
    let data = lines(&["10", "20", "30", "40"]);
    let run = |verb: &str| {
        eval(
            &pipeline(&format!("gauge .x\nsource .x {{ in; {verb} }}")),
            &data,
            1.0,
        )
    };
    assert_eq!(run("sum"), QueryResult::Scalar(100.0));
    assert_eq!(run("min"), QueryResult::Scalar(10.0));
    assert_eq!(run("max"), QueryResult::Scalar(40.0));
    assert_eq!(run("avg"), QueryResult::Scalar(25.0));
}

#[test]
fn avg_over_json_field() {
    let ops = pipeline("gauge .a\nsource .a { in.json; field ms; avg }");
    let data = lines(&[r#"{"ms":100}"#, r#"{"ms":200}"#, r#"{"ms":300}"#]);
    assert_eq!(eval(&ops, &data, 1.0), QueryResult::Scalar(200.0));
}

#[test]
fn keys_flattens_object_keys() {
    let ops = pipeline("tail .x\nsource .x { in.json; keys }");
    assert_eq!(
        eval(&ops, &lines(&[r#"{"a":1,"b":2}"#]), 1.0),
        QueryResult::Lines(lines(&["a", "b"]))
    );
}

#[test]
fn each_flattens_json_array_jq_style() {
    // jq `.items[].name`  ==  field items; each; field name
    let ops = pipeline("tail .x\nsource .x { in.json; field items; each; field name }");
    let data = lines(&[r#"{"items":[{"name":"a"},{"name":"b"},{"name":"c"}]}"#]);
    assert_eq!(
        eval(&ops, &data, 1.0),
        QueryResult::Lines(lines(&["a", "b", "c"]))
    );
}

#[test]
fn each_passes_non_array_through() {
    let ops = pipeline("tail .x\nsource .x { in; each }");
    assert_eq!(
        eval(&ops, &lines(&["hello", "world"]), 1.0),
        QueryResult::Lines(lines(&["hello", "world"]))
    );
}

#[test]
fn json_field_by_name_then_tally() {
    let ops = pipeline("bars .x\nsource .x { in.json; field level; tally }");
    let data = lines(&[
        r#"{"level":"INFO","msg":"a"}"#,
        r#"{"level":"ERROR","msg":"b"}"#,
        r#"{"level":"INFO","msg":"c"}"#,
    ]);
    assert_eq!(
        eval(&ops, &data, 1.0),
        QueryResult::Pairs(vec![("INFO".into(), 2), ("ERROR".into(), 1)])
    );
}

#[test]
fn json_nested_key_path() {
    let ops = pipeline("text .x\nsource .x { in.json; field a b }");
    assert_eq!(
        eval(&ops, &lines(&[r#"{"a":{"b":42}}"#]), 1.0),
        QueryResult::Lines(lines(&["42"]))
    );
}

#[test]
fn json_missing_key_is_empty() {
    let ops = pipeline("text .x\nsource .x { in.json; field nope }");
    assert_eq!(
        eval(&ops, &lines(&[r#"{"a":1}"#]), 1.0),
        QueryResult::Lines(lines(&[""]))
    );
}

#[test]
fn map_transforms_each_line_via_fusevm() {
    let ops = pipeline("tail .x\nsource .x { in; field 2; map x / 1024 }");
    assert_eq!(
        eval(&ops, &lines(&["a 2048", "b 1024"]), 1.0),
        QueryResult::Lines(lines(&["2", "1"]))
    );
}

#[test]
fn map_json_field_via_fusevm() {
    let ops = pipeline("tail .x\nsource .x { in.json; map bytes / 1024 }");
    let data = lines(&[r#"{"bytes":2048}"#, r#"{"bytes":512}"#]);
    assert_eq!(
        eval(&ops, &data, 1.0),
        QueryResult::Lines(lines(&["2", "0.5"]))
    );
}

#[test]
fn where_by_json_field_via_fusevm() {
    let ops = pipeline("tail .x\nsource .x { in.json; where amount > 100 }");
    let data = lines(&[r#"{"amount":50}"#, r#"{"amount":150}"#, r#"{"amount":200}"#]);
    assert_eq!(
        eval(&ops, &data, 1.0),
        QueryResult::Lines(lines(&[r#"{"amount":150}"#, r#"{"amount":200}"#]))
    );
}

#[test]
fn where_filters_numeric_lines_via_fusevm() {
    let ops = pipeline("tail .x\nsource .x { in; field 2; where x > 100 }");
    assert_eq!(
        eval(&ops, &lines(&["a 50", "b 150", "c 200", "d 30"]), 1.0),
        QueryResult::Lines(lines(&["150", "200"]))
    );
}

#[test]
fn calc_transforms_count_via_fusevm() {
    let ops = pipeline("gauge .x\nsource .x { in; match /err/; calc x / 2 }");
    assert_eq!(
        eval(&ops, &lines(&["err", "err", "ok", "err", "err"]), 1.0),
        QueryResult::Scalar(2.0)
    );
}

#[test]
fn empty_pipeline_passes_lines_through() {
    let ops = pipeline("tail .x\nsource .x { in }");
    assert_eq!(
        eval(&ops, &lines(&["a", "b"]), 1.0),
        QueryResult::Lines(lines(&["a", "b"]))
    );
}
