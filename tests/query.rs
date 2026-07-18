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
fn empty_pipeline_passes_lines_through() {
    let ops = pipeline("tail .x\nsource .x { in }");
    assert_eq!(
        eval(&ops, &lines(&["a", "b"]), 1.0),
        QueryResult::Lines(lines(&["a", "b"]))
    );
}
