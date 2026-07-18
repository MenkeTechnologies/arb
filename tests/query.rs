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

#[test]
fn find_attr_text_xpath_leg() {
    let html = vec![
        r#"<div class="card"><a href="/a">Alpha</a></div>"#.to_string(),
        r#"<div class="card"><a href="/b">Beta</a></div>"#.to_string(),
    ];
    // //a/@href
    let hrefs = pipeline("tail .x\nsource .x { in.html; find a; attr href }");
    match arb::query::eval(&hrefs, &html, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, vec!["/a", "/b"]),
        other => panic!("expected Lines, got {other:?}"),
    }
    // find a; text
    let texts = pipeline("tail .x\nsource .x { in.html; find a; text }");
    match arb::query::eval(&texts, &html, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, vec!["Alpha", "Beta"]),
        other => panic!("expected Lines, got {other:?}"),
    }
}

#[test]
fn attr_drops_missing() {
    let html = vec![r#"<a href="/x">y</a><a>no</a>"#.to_string()];
    let ops = pipeline("tail .x\nsource .x { in.html; find a; attr href }");
    match arb::query::eval(&ops, &html, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, vec!["/x"]),
        other => panic!("expected Lines, got {other:?}"),
    }
}

#[test]
fn sort_by_orders_json_records_numerically_and_stably() {
    let ops = pipeline("tail .x\nsource .x { in.json; sort_by age }");
    let lines = vec![
        r#"{"name":"a","age":30}"#.to_string(),
        r#"{"name":"b","age":5}"#.to_string(),
        "not-json".to_string(),
        r#"{"name":"c","age":5}"#.to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(
            ls,
            vec![
                r#"{"name":"b","age":5}"#,  // numeric (not lexical: "30" would precede "5")
                r#"{"name":"c","age":5}"#,  // stable: b before c on equal keys
                r#"{"name":"a","age":30}"#,
                "not-json",                 // non-object sinks after, input order
            ]
        ),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn unique_by_keeps_first_record_per_field_value() {
    let ops = pipeline("tail .x\nsource .x { in.json; unique_by name }");
    let lines = vec![
        r#"{"name":"a","v":1}"#.to_string(),
        r#"{"name":"b","v":2}"#.to_string(),
        r#"{"name":"a","v":3}"#.to_string(),
        r#"{"name":"b","v":4}"#.to_string(),
        r#"{"name":"c","v":5}"#.to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(
            ls,
            vec![
                r#"{"name":"a","v":1}"#,
                r#"{"name":"b","v":2}"#,
                r#"{"name":"c","v":5}"#,
            ]
        ),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn count_by_groups_json_records_by_field() {
    let ops = pipeline("tail .x\nsource .x { in.json; count_by dept }");
    let lines = vec![
        r#"{"dept":"eng","name":"a"}"#.to_string(),
        r#"{"dept":"eng","name":"b"}"#.to_string(),
        r#"{"dept":"sales","name":"c"}"#.to_string(),
        "plain".to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Pairs(ps) => assert_eq!(
            ps,
            vec![
                ("eng".to_string(), 2u64),
                ("plain".to_string(), 1u64),
                ("sales".to_string(), 1u64),
            ]
        ),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn min_by_returns_record_with_smallest_field() {
    let ops = pipeline("tail .x\nsource .x { in.json; min_by age }");
    let lines = vec![
        r#"{"name":"a","age":30}"#.to_string(),
        r#"{"name":"b","age":10}"#.to_string(),
        r#"{"name":"c","age":20}"#.to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec![r#"{"name":"b","age":10}"#]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn max_by_returns_record_with_largest_field() {
    let ops = pipeline("tail .x\nsource .x { in.json; max_by age }");
    let lines = vec![
        r#"{"name":"a","age":30}"#.to_string(),
        r#"{"name":"b","age":45}"#.to_string(),
        r#"{"name":"c","age":12}"#.to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        // Must return the FULL record for the max field, not just the value,
        // and must pick 45 (max) rather than 12 (min) — catches a min/max swap.
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec![r#"{"name":"b","age":45}"#]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn has_retains_only_objects_with_key() {
    let ops = pipeline("tail .x\nsource .x { in.json; has age }");
    let lines = vec![
        r#"{"name":"a","age":30}"#.to_string(),
        r#"{"name":"b"}"#.to_string(),
        "not json".to_string(),
        r#"["age"]"#.to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec![r#"{"name":"a","age":30}"#]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn entries_expands_json_object_to_key_value_lines() {
    let ops = pipeline("tail .x\nsource .x { in.json; entries }");
    let lines = vec![
        r#"{"name":"a","age":30}"#.to_string(),
        "not json".to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(
            ls,
            vec![
                r#"{"key":"age","value":30}"#.to_string(),
                r#"{"key":"name","value":"a"}"#.to_string(),
                "not json".to_string(),
            ]
        ),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn flatten_expands_nested_json_arrays_one_level() {
    // [1,[2,3],4] -> 1, 2, 3, 4  (nested array [2,3] is expanded, unlike `each`)
    let ops = pipeline("tail .x\nsource .x { in.json; flatten }");
    let lines = vec!["[1,[2,3],4]".to_string()];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec!["1", "2", "3", "4"]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn add_reduces_json_arrays() {
    let ops = pipeline("tail .x\nsource .x { in.json; add }");
    let lines = vec![
        "[1, 2, 3]".to_string(),
        r#"["a", "b", "c"]"#.to_string(),
        "[]".to_string(),
        r#"{"name":"keep"}"#.to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(
            ls,
            vec![
                "6".to_string(),
                "abc".to_string(),
                String::new(),
                r#"{"name":"keep"}"#.to_string(),
            ]
        ),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn over_keeps_lines_strictly_greater_than_threshold() {
    let ops = pipeline("tail .x\nsource .x { in; over 10 }");
    let lines = vec![
        "5".to_string(),
        "10".to_string(),
        "10.5".to_string(),
        "42".to_string(),
        "nan-ish".to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec!["10.5".to_string(), "42".to_string()]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn under_keeps_lines_strictly_below_threshold() {
    let ops = pipeline("tail .x\nsource .x { in; under 10 }");
    let lines = vec![
        "5".to_string(),
        "10".to_string(),
        "15".to_string(),
        "abc".to_string(),
        "9.5".to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec!["5".to_string(), "9.5".to_string()]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn between_keeps_inclusive_numeric_range() {
    let ops = pipeline("tail .x\nsource .x { in; between 10 20 }");
    let lines = vec![
        "5".to_string(),
        "10".to_string(),
        "15".to_string(),
        "20".to_string(),
        "25".to_string(),
        "abc".to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec!["10", "15", "20"]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn enumerate_prefixes_one_based_index_and_tab() {
    let ops = pipeline("tail .x\nsource .x { in; enumerate }");
    let lines = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(
            ls,
            vec![
                "1\talpha".to_string(),
                "2\tbeta".to_string(),
                "3\tgamma".to_string(),
            ]
        ),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn words_splits_lines_on_whitespace_and_drops_empties() {
    let ops = pipeline("tail .x\nsource .x { in; words }");
    let lines = vec![
        "the  quick brown".to_string(),
        "".to_string(),
        "   ".to_string(),
        "fox".to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec!["the", "quick", "brown", "fox"]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn dedup_collapses_only_adjacent_duplicates() {
    let ops = pipeline("tail .x\nsource .x { in; dedup }");
    let lines = vec![
        "a".to_string(),
        "a".to_string(),
        "b".to_string(),
        "a".to_string(),
        "a".to_string(),
        "a".to_string(),
        "c".to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            // runs of "a" collapse, but the later "a" survives because it is not adjacent to the first run
            assert_eq!(ls, vec!["a", "b", "a", "c"]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn tailn_keeps_last_n_lines() {
    let ops = pipeline("tail .x\nsource .x { in; tailn 2 }");
    let input = vec![
        "a".to_string(),
        "b".to_string(),
        "c".to_string(),
        "d".to_string(),
    ];
    match arb::query::eval(&ops, &input, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, vec!["c".to_string(), "d".to_string()]),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn pad_right_pads_to_min_width_without_truncating() {
    let ops = pipeline("tail .x\nsource .x { in; pad 5 }");
    let lines = vec!["ab".to_string(), "abcdef".to_string()];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec!["ab   ".to_string(), "abcdef".to_string()]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn lpad_pads_short_lines_and_leaves_long_ones() {
    let ops = pipeline("tail .x\nsource .x { in; lpad 4 }");
    let lines = vec!["a".to_string(), "bb".to_string(), "toolong".to_string()];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec!["   a".to_string(), "  bb".to_string(), "toolong".to_string()]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn grepf_filters_by_json_field() {
    let ops = pipeline("tail .x\nsource .x { in.json; grepf name /^a/ }");
    let lines = vec![
        r#"{"name":"alice","age":30}"#.to_string(),
        r#"{"name":"bob","age":25}"#.to_string(),
        r#"{"name":"amir","age":40}"#.to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(
            ls,
            vec![
                r#"{"name":"alice","age":30}"#.to_string(),
                r#"{"name":"amir","age":40}"#.to_string(),
            ]
        ),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn flip_reverses_characters_per_line() {
    let ops = pipeline("tail .x\nsource .x { in; flip }");
    let lines = vec![
        "abc".to_string(),
        "héllo".to_string(),
        String::new(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec!["cba".to_string(), "olléh".to_string(), String::new()]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn in_yaml_parses_to_json_records() {
    let ops = pipeline("tail .x\nsource .x { in.yaml; field name }");
    let lines = vec!["name: alice".to_string(), "age: 30".to_string()];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, vec!["alice"]),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn in_yaml_multidoc_counts() {
    let ops = pipeline("tail .x\nsource .x { in.yaml; count }");
    let lines = vec!["a: 1".to_string(), "---".to_string(), "b: 2".to_string()];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Scalar(n) => assert_eq!(n, 2.0),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn in_toml_parses_nested_field() {
    let ops = pipeline("tail .x\nsource .x { in.toml; field nested port }");
    let lines = vec!["[nested]".to_string(), "port = 8080".to_string()];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, vec!["8080"]),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn b64_encodes_each_line_standard() {
    let ops = pipeline("tail .x\nsource .x { in; b64 }");
    let lines = vec!["hello".to_string(), "foo".to_string()];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec!["aGVsbG8=".to_string(), "Zm9v".to_string()]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn b64d_decodes_and_passes_invalid_through() {
    let ops = pipeline("tail .x\nsource .x { in; b64d }");
    // "aGVsbG8=" -> "hello"; "###" is not valid base64 -> unchanged;
    // "kg==" decodes to bytes [0x92] which is not valid UTF-8 -> unchanged.
    let lines = vec![
        "aGVsbG8=".to_string(),
        "###".to_string(),
        "kg==".to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec!["hello".to_string(), "###".to_string(), "kg==".to_string()]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn hex_encodes_lines_bytewise() {
    let ops = pipeline("tail .x\nsource .x { in; hex }");
    // "Hi!" -> 48 69 21 ; "é" is UTF-8 0xC3 0xA9 -> proves byte-wise, not char-wise.
    let lines = vec!["Hi!".to_string(), "é".to_string()];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec!["486921".to_string(), "c3a9".to_string()]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn unhex_decodes_valid_and_passes_through_invalid() {
    let ops = pipeline("tail .x\nsource .x { in; unhex }");
    let lines = vec![
        "48656c6c6f".to_string(), // "Hello"
        "6869".to_string(),       // "hi"
        "zz".to_string(),         // non-hex digit -> unchanged
        "abc".to_string(),        // odd length -> unchanged
        "".to_string(),           // empty -> unchanged
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(
            ls,
            vec![
                "Hello".to_string(),
                "hi".to_string(),
                "zz".to_string(),
                "abc".to_string(),
                "".to_string(),
            ]
        ),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn urlenc_escapes_non_alphanumerics() {
    let ops = pipeline("tail .x\nsource .x { in; urlenc }");
    let lines = vec!["hello world!".to_string(), "a/b?c=1&d".to_string()];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(
            ls,
            vec!["hello%20world%21".to_string(), "a%2Fb%3Fc%3D1%26d".to_string()]
        ),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn urldec_decodes_and_passes_invalid_utf8_through() {
    let ops = pipeline("tail .x\nsource .x { in; urldec }");
    let lines = vec![
        "caf%C3%A9%20%2F".to_string(), // multibyte UTF-8 + space + slash
        "%FF".to_string(),             // 0xFF alone is invalid UTF-8 -> unchanged
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec!["café /".to_string(), "%FF".to_string()]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn extract_emits_capture_group_and_drops_nonmatching() {
    // Pattern has a group -> emit group 1, not the whole match; unmatched line is dropped.
    let ops = pipeline("tail .x\nsource .x { in; extract /id=(\\d+)/ }");
    let lines = vec![
        "id=42 name=a".to_string(),
        "no match here".to_string(),
        "id=99".to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, vec!["42", "99"]),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn split_explodes_each_line_by_literal_multichar_delim() {
    // Multi-char literal delim "::" over two lines exercises both properties:
    // (1) literal-string split (not per-char / not regex), (2) one->many rebuild across lines.
    let ops = pipeline("tail .x\nsource .x { in; split :: }");
    let lines = vec!["a::b::c".to_string(), "d::e".to_string()];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, vec!["a", "b", "c", "d", "e"]),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn substr_extracts_char_range_and_clamps_end() {
    // A>0 exercises skip; B far past len exercises take + clamp; multibyte confirms
    // char-indexing (not byte-indexing) since 'é' is 2 bytes.
    let ops = pipeline("tail .x\nsource .x { in; substr 2 100 }");
    let lines = vec!["héllo".to_string()];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, vec!["llo"]),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn chars_explodes_each_line_into_one_char_per_line() {
    let ops = pipeline("tail .x\nsource .x { in; chars }");
    let lines = vec!["ab".to_string(), "cé".to_string(), "".to_string()];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec!["a", "b", "c", "é"]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn title_titlecases_and_normalizes_whitespace() {
    // mixed case in first + rest, multiple/tab whitespace collapsed to single spaces
    let ops = pipeline("tail .x\nsource .x { in; title }");
    let lines = vec!["hELLo   WORLD\tfoo bAR".to_string()];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec!["Hello World Foo Bar".to_string()]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn repeat_concatenates_line_n_times() {
    let ops = pipeline("tail .x\nsource .x { in; repeat 3 }");
    let lines = vec!["ab".to_string(), "".to_string()];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, vec!["ababab".to_string(), "".to_string()]),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn set_overwrites_existing_and_adds_key_passing_through_non_json() {
    let ops = pipeline("tail .x\nsource .x { in.json; set name x }");
    let lines = vec![
        r#"{"age":30}"#.to_string(),          // key added
        r#"{"name":"old","age":1}"#.to_string(), // existing overwritten
        "hello".to_string(),                   // non-object passes through
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, vec![
            r#"{"age":30,"name":"x"}"#.to_string(),
            r#"{"age":1,"name":"x"}"#.to_string(),
            "hello".to_string(),
        ]),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn del_removes_key_and_passes_non_objects() {
    let ops = pipeline("tail .x\nsource .x { in.json; del age }");
    let lines = vec![
        r#"{"name":"a","age":30,"city":"z"}"#.to_string(),
        "not json".to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(
            ls,
            vec![
                r#"{"city":"z","name":"a"}"#.to_string(),
                "not json".to_string(),
            ]
        ),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn rename_renames_key_preserving_value_and_noops_when_absent() {
    let ops = pipeline("tail .x\nsource .x { in.json; rename name id }");
    let lines = vec![
        r#"{"name":"a","age":30}"#.to_string(),
        r#"{"age":40}"#.to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(
            ls,
            vec![
                r#"{"age":30,"id":"a"}"#.to_string(),
                r#"{"age":40}"#.to_string(),
            ]
        ),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn default_fills_missing_but_never_overwrites() {
    let ops = pipeline("tail .x\nsource .x { in.json; default city z }");
    let lines = vec![
        r#"{"name":"a"}"#.to_string(),
        r#"{"name":"b","city":"existing"}"#.to_string(),
        "not json".to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(
            ls,
            vec![
                r#"{"city":"z","name":"a"}"#.to_string(),
                r#"{"city":"existing","name":"b"}"#.to_string(),
                "not json".to_string(),
            ]
        ),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn merge_reduces_objects_last_key_wins() {
    let ops = pipeline("tail .x\nsource .x { in.json; merge }");
    let lines = vec![
        r#"{"a":1,"b":2}"#.to_string(),
        "not json".to_string(),
        r#"{"b":3,"c":4}"#.to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            assert_eq!(ls, vec![r#"{"a":1,"b":3,"c":4}"#.to_string()]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn floor_rounds_down_including_negatives() {
    let ops = pipeline("tail .x\nsource .x { in; floor }");
    let lines = vec![
        "3.7".to_string(),
        "-1.2".to_string(),
        "5".to_string(),
        "abc".to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => {
            // -1.2 -> -2 catches a trunc-instead-of-floor bug; "abc" passes through.
            assert_eq!(ls, vec!["3", "-2", "5", "abc"]);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn ceil_rounds_up_and_leaves_non_numeric_untouched() {
    let ops = pipeline("tail .x\nsource .x { in; ceil }");
    let lines = vec![
        "1.2".to_string(),
        "3.0".to_string(),
        "-1.5".to_string(),
        "abc".to_string(),
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(
            ls,
            vec![
                "2".to_string(),
                "3".to_string(),
                "-1".to_string(),
                "abc".to_string(),
            ]
        ),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn clamp_bounds_numeric_lines_and_passes_others() {
    let ops = pipeline("tail .x\nsource .x { in; clamp 0 10 }");
    let lines = vec![
        "-5".to_string(),   // below LO -> 0
        "3".to_string(),    // in range -> 3
        "42".to_string(),   // above HI -> 10
        "abc".to_string(),  // non-numeric -> unchanged
    ];
    match arb::query::eval(&ops, &lines, 0.0) {
        arb::query::QueryResult::Lines(ls) => assert_eq!(ls, vec!["0", "3", "10", "abc"]),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn table_data_splits_cells_and_names_headers() {
    use arb::query::{table_data, table_ncols};
    let lines = vec!["alice 100 vim".to_string(), "bob 200 bash".to_string()];
    let (headers, rows) = table_data(&lines, Some("user, pid, cmd"));
    assert_eq!(headers, vec!["user", "pid", "cmd"]);
    assert_eq!(rows, vec![vec!["alice", "100", "vim"], vec!["bob", "200", "bash"]]);
    assert_eq!(table_ncols(&headers, &rows), 3);

    // No cols header: headers empty, ncols from the widest row.
    let ragged = vec!["a b c d".to_string(), "x y".to_string()];
    let (h2, r2) = table_data(&ragged, None);
    assert!(h2.is_empty());
    assert_eq!(table_ncols(&h2, &r2), 4);
}

#[test]
fn sparkline_and_numeric_series() {
    use arb::query::{numeric_series, sparkline};
    // Non-numeric lines skipped; first token parsed.
    let lines = vec!["10".to_string(), "20 x".to_string(), "nope".to_string(), "30".to_string()];
    assert_eq!(numeric_series(&lines), vec![10.0, 20.0, 30.0]);
    // Evenly spaced 0..7 maps to each of the 8 ticks in order.
    assert_eq!(sparkline(&[0., 1., 2., 3., 4., 5., 6., 7.]), "▁▂▃▄▅▆▇█");
    // Flat series → lowest tick; empty → empty string.
    assert_eq!(sparkline(&[5.0, 5.0, 5.0]), "▁▁▁");
    assert_eq!(sparkline(&[]), "");
}

#[test]
fn percentile_nearest_rank_and_sugar() {
    let f = |v: &str, d: &[&str]| eval(&pipeline(&format!("gauge .x\nsource .x {{ in; {v} }}")), &lines(d), 1.0);
    let ten = &["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"];
    // Nearest-rank: ceil(frac * n), 1-indexed.
    assert_eq!(f("percentile 90", ten), QueryResult::Scalar(9.0));
    assert_eq!(f("percentile 100", ten), QueryResult::Scalar(10.0));
    assert_eq!(f("percentile 0", ten), QueryResult::Scalar(1.0));
    // Sugar aliases agree with the explicit form; p95 keeps its prior value.
    assert_eq!(f("p50", ten), f("percentile 50", ten));
    assert_eq!(f("p99", ten), QueryResult::Scalar(10.0));
    assert_eq!(f("p95", ten), QueryResult::Scalar(10.0));
    // Empty input → 0.
    assert_eq!(f("percentile 99", &[]), QueryResult::Scalar(0.0));
}

#[test]
fn delta_and_cumsum() {
    let f = |v: &str, d: &[&str]| eval(&pipeline(&format!("list .x\nsource .x {{ in; {v} }}")), &lines(d), 1.0);
    // Consecutive differences: n values → n-1 deltas.
    assert_eq!(f("delta", &["10", "15", "15", "40"]), QueryResult::Lines(lines(&["5", "0", "25"])));
    // Running total.
    assert_eq!(f("cumsum", &["1", "2", "3", "4"]), QueryResult::Lines(lines(&["1", "3", "6", "10"])));
    // Edge cases: single value → empty delta; empty input → empty.
    assert_eq!(f("delta", &["5"]), QueryResult::Lines(lines(&[])));
    assert_eq!(f("cumsum", &[]), QueryResult::Lines(lines(&[])));
    // Composes: delta then sum recovers the net change (40-10=30).
    let net = eval(&pipeline("gauge .x\nsource .x { in; delta; sum }"), &lines(&["10", "15", "15", "40"]), 1.0);
    assert_eq!(net, QueryResult::Scalar(30.0));
}

#[test]
fn sma_and_ewma_smooth() {
    let f = |v: &str, d: &[&str]| eval(&pipeline(&format!("list .x\nsource .x {{ in; {v} }}")), &lines(d), 1.0);
    // SMA is length-preserving; the first points average a shorter window.
    assert_eq!(
        f("sma 3", &["1", "2", "3", "4", "5", "6"]),
        QueryResult::Lines(lines(&["1", "1.5", "2", "3", "4", "5"]))
    );
    // EWMA: s0=x0, then alpha*x + (1-alpha)*prev.
    assert_eq!(
        f("ewma 0.5", &["0", "10", "10", "10"]),
        QueryResult::Lines(lines(&["0", "5", "7.5", "8.75"]))
    );
    // Empty input → empty; window >= len averages everything seen so far.
    assert_eq!(f("sma 10", &[]), QueryResult::Lines(lines(&[])));
    assert_eq!(f("sma 10", &["2", "4"]), QueryResult::Lines(lines(&["2", "3"])));
}

#[test]
fn bytes_and_duration_humanize() {
    let f = |v: &str, d: &[&str]| eval(&pipeline(&format!("list .x\nsource .x {{ in; {v} }}")), &lines(d), 1.0);
    // 1024-based byte sizes, one decimal (trailing .0 trimmed).
    assert_eq!(
        f("bytes", &["500", "1024", "1536", "1048576", "1610612736"]),
        QueryResult::Lines(lines(&["500 B", "1 KB", "1.5 KB", "1 MB", "1.5 GB"]))
    );
    // Durations as the two largest non-zero units.
    assert_eq!(
        f("duration", &["45", "90", "3661", "90061", "0"]),
        QueryResult::Lines(lines(&["45s", "1m 30s", "1h 1m", "1d 1h", "0s"]))
    );
    // Non-numeric passes through unchanged.
    assert_eq!(f("bytes", &["n/a"]), QueryResult::Lines(lines(&["n/a"])));
}
