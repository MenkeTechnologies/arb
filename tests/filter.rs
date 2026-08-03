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

/// The ranking `--filter` prints (and the picker's list) follows fzf's order:
/// score first, then `--tiebreak=length` on the TRIMMED length, then input
/// order. These are the three rules that make arb's output match `fzf`'s
/// line for line on a real corpus.
#[test]
fn rank_orders_by_score_then_trimmed_length_then_input() {
    use arb::tui::rank;
    let lines = vec![
        "src/main.rs",      // 'm' mid-path
        "man/arb.1",        // 'm' at a boundary after '/'
        "Makefile",         // 'm' at the start of the line
        "docs/manual.md  ", // boundary too, but longer — and padded
    ];
    let look = arb::fzf::Look::default();
    let order = rank(&lines, "ma", false, false, true, false, &look);
    // This is `fzf --filter ma` on the same four lines, verbatim: a
    // start-of-line match first, then the boundary match after `/`, then the
    // mid-word one, then the longer boundary match.
    let ranked: Vec<&str> = order.iter().map(|i| lines[*i]).collect();
    assert_eq!(
        ranked,
        vec!["Makefile", "man/arb.1", "src/main.rs", "docs/manual.md  "]
    );
    // Trailing whitespace is not counted by the length tiebreak (fzf compares
    // `TrimLength`), so the padded line ties on its trimmed width of 14.
    assert_eq!(arb::tui::trim_length("docs/manual.md  "), 14);
    // `--no-sort` keeps input order for the same match set.
    let unsorted = rank(&lines, "ma", false, true, true, false, &look);
    let mut expected = unsorted.clone();
    expected.sort_unstable();
    assert_eq!(unsorted, expected);
    // `--tac` walks the same matches backwards.
    let tac = rank(&lines, "ma", false, true, true, true, &look);
    let mut rev = unsorted.clone();
    rev.reverse();
    assert_eq!(tac, rev);
}

/// `--exact` uses fzf's `ExactMatchNaive`: substring only, but still scored, so
/// a boundary occurrence outranks one buried inside a word.
#[test]
fn exact_mode_is_substring_but_still_ranked() {
    use arb::tui::rank;
    let lines = vec!["unbarred", "foo/bar", "bar"];
    let order = rank(
        &lines,
        "bar",
        true,
        false,
        true,
        false,
        &arb::fzf::Look::default(),
    );
    assert_eq!(lines[order[0]], "bar");
    assert_eq!(lines[order[order.len() - 1]], "unbarred");
    // A subsequence that isn't a substring doesn't match at all in exact mode.
    assert!(rank(
        &["b-a-r"],
        "bar",
        true,
        false,
        true,
        false,
        &arb::fzf::Look::default()
    )
    .is_empty());
    assert_eq!(
        rank(
            &["b-a-r"],
            "bar",
            false,
            false,
            true,
            false,
            &arb::fzf::Look::default()
        ),
        vec![0]
    );
}

/// fzf's field selection: `--nth` restricts what the query matches (the row
/// still shows the whole line), `--with-nth` restricts what it shows, and the
/// field text follows fzf's tokenizer — with an explicit delimiter a single
/// field excludes its trailing delimiter, while a range keeps the internal ones.
/// Verified against `fzf --filter` on the same fixtures.
#[test]
fn nth_selects_the_fields_fzf_selects() {
    use arb::fzf::{tokenize, transform, Nth};
    // Explicit delimiter: tokens keep their trailing delimiter…
    let toks = tokenize("alpha:beta:gamma", Some(":"));
    assert_eq!(toks, vec!["alpha:", "beta:", "gamma"]);
    // …but a selected range drops the one that trails the whole range.
    assert_eq!(transform(&toks, &Nth::parse_list("1"), Some(":")), "alpha");
    assert_eq!(
        transform(&toks, &Nth::parse_list("1..2"), Some(":")),
        "alpha:beta"
    );
    assert_eq!(transform(&toks, &Nth::parse_list("-1"), Some(":")), "gamma");
    assert_eq!(
        transform(&toks, &Nth::parse_list("2.."), Some(":")),
        "beta:gamma"
    );
    // AWK-style: a token is a word plus the whitespace after it, leading
    // whitespace belongs to no token, and nothing is stripped.
    let toks = tokenize("  aa bb  cc", None);
    assert_eq!(toks, vec!["aa ", "bb  ", "cc"]);
    assert_eq!(transform(&toks, &Nth::parse_list("1"), None), "aa ");
    assert_eq!(transform(&toks, &Nth::parse_list("2.."), None), "bb  cc");
    // Range syntax fzf rejects yields no ranges (the option is ignored).
    assert!(Nth::parse_list("0").is_empty());
    assert!(Nth::parse_list("-1..2").is_empty());
}

/// `--ansi`: the colour codes are metadata. What the query matches, and what a
/// selection emits, is the text without them.
#[test]
fn ansi_codes_are_colour_not_content() {
    use arb::fzf::{strip_ansi, Look};
    let line = "\x1b[32malpha\x1b[0m:beta";
    assert_eq!(strip_ansi(line), "alpha:beta");
    let ansi = Look {
        ansi: true,
        ..Look::default()
    };
    assert_eq!(arb::tui::search_key(line, &ansi), "alpha:beta");
    assert_eq!(arb::tui::item_text(line, &ansi), "alpha:beta");
    // Without `--ansi` the escapes are part of the text, as in fzf.
    let plain = Look::default();
    assert_eq!(arb::tui::item_text(line, &plain), line);
    // `--nth` composes with it: field 1 of the stripped line.
    let both = Look {
        ansi: true,
        delimiter: Some(":".into()),
        nth: arb::fzf::Nth::parse_list("1"),
        ..Look::default()
    };
    assert_eq!(arb::tui::search_key(line, &both), "alpha");
}
