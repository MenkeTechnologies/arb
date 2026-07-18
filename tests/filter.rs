//! Megafilter predicate + line-clip tests (the interactive filter's core logic;
//! the /dev/tty render loop itself needs a real terminal, but this is testable).
use arb::tui::filter_matches;

#[test]
fn filter_empty_keeps_all() {
    assert!(filter_matches("anything", ""));
    assert!(filter_matches("", ""));
}

#[test]
fn filter_case_insensitive_substring() {
    assert!(filter_matches("/var/log/SYSTEM.log", "system"));
    assert!(filter_matches("ERROR: disk full", "error"));
    assert!(!filter_matches("all good here", "error"));
}

#[test]
fn filter_narrows_a_line_set() {
    let lines = ["GET /api 200", "GET /health 200", "POST /api 500"];
    let kept: Vec<&str> = lines.iter().copied().filter(|l| filter_matches(l, "/api")).collect();
    assert_eq!(kept, vec!["GET /api 200", "POST /api 500"]);
}
