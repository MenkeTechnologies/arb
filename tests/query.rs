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
fn sel_attr_extracts_attribute() {
    let ops = pipeline("tail .x\nsource .x { in.html; sel a -attr href }");
    let html = lines(&[r#"<div><a href="/1">one</a><a href="/2">two</a><a>none</a></div>"#]);
    assert_eq!(
        eval(&ops, &html, 1.0),
        QueryResult::Lines(lines(&["/1", "/2"]))
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
fn replace_join_nth() {
    assert_eq!(
        eval(
            &pipeline("tail .x\nsource .x { in; replace /o/ 0 }"),
            &lines(&["foo", "bob"]),
            1.0
        ),
        QueryResult::Lines(lines(&["f00", "b0b"]))
    );
    assert_eq!(
        eval(
            &pipeline("tail .x\nsource .x { in; join , }"),
            &lines(&["a", "b", "c"]),
            1.0
        ),
        QueryResult::Lines(lines(&["a,b,c"]))
    );
    assert_eq!(
        eval(
            &pipeline("tail .x\nsource .x { in; nth 2 }"),
            &lines(&["a", "b", "c"]),
            1.0
        ),
        QueryResult::Lines(lines(&["b"]))
    );
}

#[test]
fn string_transforms_upper_lower_trim() {
    assert_eq!(
        eval(
            &pipeline("tail .x\nsource .x { in; upper }"),
            &lines(&["abc", "De"]),
            1.0
        ),
        QueryResult::Lines(lines(&["ABC", "DE"]))
    );
    assert_eq!(
        eval(
            &pipeline("tail .x\nsource .x { in; trim }"),
            &lines(&["  hi  ", " yo"]),
            1.0
        ),
        QueryResult::Lines(lines(&["hi", "yo"]))
    );
}

#[test]
fn sort_then_uniq() {
    let ops = pipeline("tail .x\nsource .x { in; sort; uniq }");
    assert_eq!(
        eval(&ops, &lines(&["b", "a", "b", "c", "a"]), 1.0),
        QueryResult::Lines(lines(&["a", "b", "c"]))
    );
}

#[test]
fn numeric_sort_ascending() {
    let ops = pipeline("tail .x\nsource .x { in; sort -n }");
    assert_eq!(
        eval(&ops, &lines(&["40", "1200", "90", "2500", "300"]), 1.0),
        QueryResult::Lines(lines(&["40", "90", "300", "1200", "2500"]))
    );
}

#[test]
fn numeric_sort_reverse_top_n() {
    let ops = pipeline("tail .x\nsource .x { in; sort -n -r; take 3 }");
    assert_eq!(
        eval(&ops, &lines(&["40", "1200", "90", "2500", "300"]), 1.0),
        QueryResult::Lines(lines(&["2500", "1200", "300"]))
    );
}

#[test]
fn take_and_drop() {
    assert_eq!(
        eval(
            &pipeline("tail .x\nsource .x { in; take 2 }"),
            &lines(&["1", "2", "3", "4"]),
            1.0
        ),
        QueryResult::Lines(lines(&["1", "2"]))
    );
    assert_eq!(
        eval(
            &pipeline("tail .x\nsource .x { in; drop 2 }"),
            &lines(&["1", "2", "3", "4"]),
            1.0
        ),
        QueryResult::Lines(lines(&["3", "4"]))
    );
}

#[test]
fn last_and_rev() {
    assert_eq!(
        eval(
            &pipeline("tail .x\nsource .x { in; last }"),
            &lines(&["a", "b", "c"]),
            1.0
        ),
        QueryResult::Lines(lines(&["c"]))
    );
    assert_eq!(
        eval(
            &pipeline("tail .x\nsource .x { in; rev }"),
            &lines(&["a", "b", "c"]),
            1.0
        ),
        QueryResult::Lines(lines(&["c", "b", "a"]))
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
fn csv_field_by_header_name_tally() {
    let ops = pipeline("bars .x\nsource .x { in.csv; field status; tally }");
    let data = lines(&["name,status", "a,ok", "b,err", "c,ok"]);
    assert_eq!(
        eval(&ops, &data, 1.0),
        QueryResult::Pairs(vec![("ok".into(), 2), ("err".into(), 1)])
    );
}

#[test]
fn csv_where_numeric_column_count() {
    let ops = pipeline("gauge .x\nsource .x { in.csv; where ms > 100; count }");
    let data = lines(&["path,ms", "/a,50", "/b,150", "/c,200"]);
    assert_eq!(eval(&ops, &data, 1.0), QueryResult::Scalar(2.0));
}

#[test]
fn logfmt_field_by_name_tally() {
    let ops = pipeline("bars .x\nsource .x { in.logfmt; field level; tally }");
    let data = lines(&["level=INFO msg=a", "level=ERROR msg=b", "level=INFO msg=c"]);
    assert_eq!(
        eval(&ops, &data, 1.0),
        QueryResult::Pairs(vec![("INFO".into(), 2), ("ERROR".into(), 1)])
    );
}

#[test]
fn logfmt_where_numeric_field() {
    let ops = pipeline("tail .x\nsource .x { in.logfmt; where dur > 100 }");
    let data = lines(&["dur=50 x=a", "dur=150 x=b", "dur=200 x=c"]);
    assert_eq!(
        eval(&ops, &data, 1.0),
        QueryResult::Lines(lines(&["dur=150 x=b", "dur=200 x=c"]))
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

#[test]
fn v20_filters() {
    let f = |v: &str, d: &[&str]| eval(&pipeline(&format!("tail .x\nsource .x {{ in; {v} }}")), &lines(d), 1.0);
    assert_eq!(f("contains err", &["err a", "ok", "c err"]), QueryResult::Lines(lines(&["err a", "c err"])));
    assert_eq!(f("starts GET", &["GET /a", "POST /b", "GET /c"]), QueryResult::Lines(lines(&["GET /a", "GET /c"])));
    assert_eq!(f("ends .json", &["a.json", "b.txt"]), QueryResult::Lines(lines(&["a.json"])));
    assert_eq!(f("nonempty", &["a", "", "  ", "b"]), QueryResult::Lines(lines(&["a", "b"])));
    assert_eq!(f("numeric", &["1", "foo", "2.5"]), QueryResult::Lines(lines(&["1", "2.5"])));
    assert_eq!(f("sample 2", &["a", "b", "c", "d"]), QueryResult::Lines(lines(&["b", "d"])));
    assert_eq!(f("slice 2 3", &["a", "b", "c", "d"]), QueryResult::Lines(lines(&["b", "c"])));
}

#[test]
fn v20_transforms() {
    let f = |v: &str, d: &[&str]| eval(&pipeline(&format!("tail .x\nsource .x {{ in; {v} }}")), &lines(d), 1.0);
    assert_eq!(f("len", &["a", "abc"]), QueryResult::Lines(lines(&["1", "3"])));
    assert_eq!(f("wc", &["a b c", "one"]), QueryResult::Lines(lines(&["3", "1"])));
    assert_eq!(f("abs", &["-3", "2", "foo"]), QueryResult::Lines(lines(&["3", "2", "foo"])));
    assert_eq!(f("round", &["1.4", "2.5"]), QueryResult::Lines(lines(&["1", "3"])));
    assert_eq!(f("prepend >>", &["a"]), QueryResult::Lines(lines(&[">>a"])));
    assert_eq!(f("append !", &["a"]), QueryResult::Lines(lines(&["a!"])));
    assert_eq!(f("cut , 2", &["a,b,c"]), QueryResult::Lines(lines(&["b"])));
}

#[test]
fn v20_reduces() {
    let f = |v: &str, d: &[&str]| eval(&pipeline(&format!("gauge .x\nsource .x {{ in; {v} }}")), &lines(d), 1.0);
    let d = &["1", "2", "3", "4"];
    assert_eq!(f("median", d), QueryResult::Scalar(2.5));
    assert_eq!(f("range", d), QueryResult::Scalar(3.0));
    assert_eq!(f("product", d), QueryResult::Scalar(24.0));
    assert_eq!(f("distinct", &["a", "a", "b"]), QueryResult::Scalar(2.0));
    assert_eq!(f("p95", &["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"]), QueryResult::Scalar(10.0));
    match f("stddev", d) {
        QueryResult::Scalar(v) => assert!((v - 1.118_033_988_749_895).abs() < 1e-9),
        _ => panic!("stddev not scalar"),
    }
}

#[test]
fn bins_numeric_histogram() {
    let ops = pipeline("histo .x\nsource .x { in; bins 2 }");
    match eval(&ops, &lines(&["0", "10", "20", "30", "40"]), 1.0) {
        QueryResult::Pairs(p) => {
            assert_eq!(p.len(), 2);
            assert_eq!(p[0].1, 2);
            assert_eq!(p[1].1, 3);
        }
        other => panic!("expected pairs, got {other:?}"),
    }
}

#[test]
fn tsv_field_by_header() {
    let ops = pipeline("bars .x\nsource .x { in.tsv; field status; tally }");
    let data = lines(&["name\tstatus", "a\tok", "b\terr", "c\tok"]);
    assert_eq!(
        eval(&ops, &data, 1.0),
        QueryResult::Pairs(vec![("ok".into(), 2), ("err".into(), 1)])
    );
}

#[test]
fn streamable_detection() {
    use arb::query::is_line_streamable;
    assert!(is_line_streamable(&pipeline("tail .x\nsource .x { in; match /a/; field 1; where x > 1 }")));
    assert!(!is_line_streamable(&pipeline("tail .x\nsource .x { in; sort; count }")));
    assert!(!is_line_streamable(&pipeline("tail .x\nsource .x { in; tally }")));
}

#[test]
fn pick_projects_json_objects() {
    let ops = pipeline("tail .x\nsource .x { in.json; pick name age }");
    let lines = vec![
        r#"{"name":"a","age":30,"city":"nyc"}"#.to_string(),
        r#"{"name":"b","age":25,"city":"sf"}"#.to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec![r#"{"name":"a","age":30}"#, r#"{"name":"b","age":25}"#]);
        }
        other => panic!("expected Lines, got {other:?}"),
    }
}

#[test]
fn pick_drops_missing_keys_and_passes_non_objects() {
    let ops = pipeline("tail .x\nsource .x { in.json; pick id nope }");
    let lines = vec![r#"{"id":7}"#.to_string(), "not json".to_string()];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec![r#"{"id":7}"#, "not json"]);
        }
        other => panic!("expected Lines, got {other:?}"),
    }
}

#[test]
fn pick_is_streamable() {
    assert!(arb::query::is_line_streamable(&pipeline(
        "tail .x\nsource .x { in.json; pick a b }"
    )));
}
