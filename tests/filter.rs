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
    let kept: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| filter_matches(l, "/api"))
        .collect();
    assert_eq!(kept, vec!["GET /api 200", "POST /api 500"]);
}

#[test]
fn fuzzy_matches_subsequence_not_just_substring() {
    use arb::tui::fuzzy_score;
    // out-of-order-but-in-sequence chars match (substring would fail)
    assert!(fuzzy_score("src/main.rs", "smain").is_some());
    assert!(fuzzy_score("alphabetic", "abc").is_some());
    // not a subsequence
    assert!(fuzzy_score("hello", "xyz").is_none());
    assert!(fuzzy_score("abc", "cba").is_none());
    // empty pattern matches all
    assert_eq!(fuzzy_score("anything", ""), Some(0));
}

#[test]
fn fuzzy_ranks_contiguous_and_boundary_higher() {
    use arb::tui::fuzzy_score;
    // contiguous "main" scores higher than scattered m..a..i..n
    let contig = fuzzy_score("main.rs", "main").unwrap();
    let scattered = fuzzy_score("m_a_i_n", "main").unwrap();
    assert!(
        contig > scattered,
        "contig {contig} should beat scattered {scattered}"
    );
    // word-boundary start beats mid-word
    let boundary = fuzzy_score("the config file", "config").unwrap();
    let midword = fuzzy_score("reconfigure", "config").unwrap();
    assert!(
        boundary > midword,
        "boundary {boundary} should beat midword {midword}"
    );
}

#[test]
fn fuzzy_smart_case() {
    use arb::tui::fuzzy_score;
    // lowercase pattern is case-insensitive
    assert!(fuzzy_score("README", "read").is_some());
    // uppercase in pattern forces case-sensitivity
    assert!(fuzzy_score("README", "READ").is_some());
    assert!(fuzzy_score("readme", "READ").is_none());
}

#[test]
fn match_positions_marks_matched_chars() {
    use arb::tui::match_positions;
    // "abc" over "alphabetic": a(0) b(5) c(9)
    assert_eq!(match_positions("alphabetic", "abc"), vec![0, 5, 9]);
    // no match → empty
    assert_eq!(match_positions("hello", "xyz"), Vec::<usize>::new());
    // empty pattern → nothing highlighted
    assert_eq!(match_positions("anything", ""), Vec::<usize>::new());
}
