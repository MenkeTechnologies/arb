//! Tests for the bundled `examples/*.arb` dashboards. Two layers:
//!
//!   1. Every example parses and builds into a valid spec (a broken example
//!      shipped in the repo is a broken tutorial — this is the guard).
//!   2. Named examples are exercised end-to-end: their `source { … }` pipelines
//!      are evaluated against representative input and the `QueryResult` is
//!      asserted, so the dashboards are proven to compute what their header
//!      comment claims, not merely to parse.
//!
//! Headless and CI-safe: no terminal is touched (the TUI is never started).

use arb::parser::parse;
use arb::query::{eval, QueryOp, QueryResult};
use arb::spec::{build, Spec};
use std::path::PathBuf;

/// Absolute path to the repo's `examples/` directory (resolved at compile time).
fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples")
}

/// Every `examples/*.arb` file, sorted, as (name, source) pairs.
fn all_examples() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::fs::read_dir(examples_dir())
        .expect("examples/ dir exists")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("arb"))
        .map(|p| {
            let name = p.file_stem().unwrap().to_string_lossy().into_owned();
            let src = std::fs::read_to_string(&p).expect("readable example");
            (name, src)
        })
        .collect();
    out.sort();
    out
}

/// Build one example by file name (without extension) into a `Spec`.
fn build_example(name: &str) -> Spec {
    let src = std::fs::read_to_string(examples_dir().join(format!("{name}.arb")))
        .unwrap_or_else(|_| panic!("example `{name}` exists"));
    build(&parse(&src).unwrap_or_else(|e| panic!("`{name}` parses: {e:?}")))
        .unwrap_or_else(|e| panic!("`{name}` builds: {e:?}"))
}

/// The evaluated `source { … }` pipeline for `path` in example `name`.
fn eval_source(name: &str, path: &str, input: &[&str], elapsed: f64) -> QueryResult {
    let spec = build_example(name);
    let w = spec
        .widgets
        .iter()
        .find(|w| w.path == path)
        .unwrap_or_else(|| panic!("example `{name}` has widget `{path}`"));
    let ops: &[QueryOp] = &w
        .source
        .as_ref()
        .unwrap_or_else(|| panic!("`{path}` has a source"))
        .pipeline;
    let lines: Vec<String> = input.iter().map(|s| s.to_string()).collect();
    eval(ops, &lines, elapsed)
}

fn pairs(v: &[(&str, u64)]) -> QueryResult {
    QueryResult::Pairs(v.iter().map(|(k, n)| (k.to_string(), *n)).collect())
}

fn lines_result(v: &[&str]) -> QueryResult {
    QueryResult::Lines(v.iter().map(|s| s.to_string()).collect())
}

// --- Layer 1: everything ships in a buildable state --------------------------

#[test]
fn examples_dir_is_not_empty() {
    assert!(!all_examples().is_empty(), "no examples/*.arb found");
}

#[test]
fn all_examples_parse_and_build() {
    for (name, src) in all_examples() {
        let ast = parse(&src).unwrap_or_else(|e| panic!("example `{name}` failed to parse: {e:?}"));
        let spec =
            build(&ast).unwrap_or_else(|e| panic!("example `{name}` failed to build: {e:?}"));
        assert!(
            !spec.widgets.is_empty(),
            "example `{name}` built zero widgets"
        );
    }
}

#[test]
fn every_widget_in_every_example_has_a_source() {
    // Each example is a complete dashboard: no dangling widget without a feed.
    for (name, src) in all_examples() {
        let spec = build(&parse(&src).unwrap()).unwrap();
        for w in &spec.widgets {
            assert!(
                w.source.is_some(),
                "example `{name}` widget `{}` has no source block",
                w.path
            );
        }
    }
}

// --- Layer 2: named examples compute the right thing -------------------------

#[test]
fn error_rate_counts_and_streams_error_lines() {
    let input = [
        "INFO started",
        "ERROR boom",
        "just fine",
        "fatal: crash",
        "PANIC!!",
    ];
    // The gauge counts anything matching error|fatal|panic (case-insensitive).
    assert_eq!(
        eval_source("error-rate", ".errors", &input, 1.0),
        QueryResult::Scalar(3.0)
    );
    // The tail keeps exactly those lines, in order.
    assert_eq!(
        eval_source("error-rate", ".errlog", &input, 1.0),
        lines_result(&["ERROR boom", "fatal: crash", "PANIC!!"])
    );
}

#[test]
fn http_status_tallies_codes_and_counts_5xx() {
    let input = [
        "code=200 path=/a dur=5",
        "code=200 path=/b dur=9",
        "code=404 path=/c dur=2",
        "code=500 path=/d dur=50",
        "code=503 path=/e dur=80",
    ];
    assert_eq!(
        eval_source("http-status", ".codes", &input, 1.0),
        pairs(&[("200", 2), ("404", 1), ("500", 1), ("503", 1)])
    );
    // Only 5xx feeds the error gauge.
    assert_eq!(
        eval_source("http-status", ".errors", &input, 1.0),
        QueryResult::Scalar(2.0)
    );
}

#[test]
fn top_talkers_ranks_clients_by_request_count() {
    let input = [
        "10.0.0.1 /home",
        "10.0.0.1 /api",
        "10.0.0.2 /home",
        "10.0.0.1 /login",
        "10.0.0.3 /api",
    ];
    // tally sorts by count desc, then key asc.
    assert_eq!(
        eval_source("top-talkers", ".ips", &input, 1.0),
        pairs(&[("10.0.0.1", 3), ("10.0.0.2", 1), ("10.0.0.3", 1)])
    );
    assert_eq!(
        eval_source("top-talkers", ".total", &input, 1.0),
        QueryResult::Scalar(5.0)
    );
}

#[test]
fn json_latency_avg_peak_and_slow_filter() {
    let input = [
        r#"{"path":"/a","ms":100}"#,
        r#"{"path":"/b","ms":200}"#,
        r#"{"path":"/c","ms":300}"#,
    ];
    assert_eq!(
        eval_source("json-latency", ".avg", &input, 1.0),
        QueryResult::Scalar(200.0)
    );
    assert_eq!(
        eval_source("json-latency", ".peak", &input, 1.0),
        QueryResult::Scalar(300.0)
    );
    // `where ms > 100` keeps the original JSON lines strictly above 100.
    assert_eq!(
        eval_source("json-latency", ".slow", &input, 1.0),
        lines_result(&[r#"{"path":"/b","ms":200}"#, r#"{"path":"/c","ms":300}"#])
    );
}

#[test]
fn word_freq_lowercases_and_ranks_tokens() {
    let input = ["The quick brown Fox", "the QUICK dog"];
    assert_eq!(
        eval_source("word-freq", ".words", &input, 1.0),
        pairs(&[
            ("quick", 2),
            ("the", 2),
            ("brown", 1),
            ("dog", 1),
            ("fox", 1)
        ])
    );
    assert_eq!(
        eval_source("word-freq", ".count", &input, 1.0),
        QueryResult::Scalar(7.0)
    );
}

#[test]
fn disk_usage_drops_header_and_counts_mounts() {
    let input = [
        "Filesystem 1024-blocks Used Available Capacity Mounted",
        "/dev/disk1s1 500000 200000 300000 40% /",
        "/dev/disk1s2 100000 90000 10000 90% /System",
    ];
    assert_eq!(
        eval_source("disk-usage", ".mounts", &input, 1.0),
        QueryResult::Scalar(2.0)
    );
}
