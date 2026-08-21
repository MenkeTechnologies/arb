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
    let order = rank(&lines, "ma", false, false, false, &look);
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
    let unsorted = rank(&lines, "ma", false, true, false, &look);
    let mut expected = unsorted.clone();
    expected.sort_unstable();
    assert_eq!(unsorted, expected);
    // `--tac` walks the same matches backwards.
    let tac = rank(&lines, "ma", false, true, true, &look);
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

// ── fzf's extended search (`-x`, the default) ───────────────────────────────
// Every expectation below is the verbatim output of `fzf --filter QUERY` on
// `CORPUS`, captured from fzf 0.74.3 — order included, since the ranking is
// half of what a picker is.

/// The corpus these cases rank, in input order.
const CORPUS: [&str; 7] = [
    "src/main.rs",
    "src/algo.rs",
    "tests/main.rs",
    "docs/main.md",
    "Makefile",
    "main.rs",
    "src/main_test.rs",
];

/// Rank `CORPUS` with fzf's default flags (extended on, smart-case,
/// `--tiebreak=length`) and return the lines, best first.
fn ranked(query: &str) -> Vec<&'static str> {
    let order = arb::tui::rank(
        &CORPUS,
        query,
        false,
        false,
        false,
        &arb::fzf::Look::default(),
    );
    order.into_iter().map(|i| CORPUS[i]).collect()
}

/// The whole point of extended mode: a space is an AND of two terms, not a
/// literal character. Getting this wrong silently returns a near-empty set for
/// the most ordinary query a person types.
#[test]
fn spaces_and_terms_together() {
    assert_eq!(
        ranked("src rs"),
        vec!["src/main.rs", "src/algo.rs", "src/main_test.rs"]
    );
    // `+x` turns the same query back into one literal term — the space has to
    // appear in the line, so nothing here matches.
    let plain = arb::fzf::Look {
        extended: false,
        ..arb::fzf::Look::default()
    };
    assert!(arb::tui::rank(&CORPUS, "src rs", false, false, false, &plain).is_empty());
}

/// `|` ORs the terms on either side of it into one set, and the set is scored
/// by whichever term matched.
#[test]
fn bar_ors_the_terms_around_it() {
    assert_eq!(
        ranked("src | docs"),
        vec![
            "docs/main.md",
            "src/main.rs",
            "src/algo.rs",
            "src/main_test.rs"
        ]
    );
}

/// `^`/`$` anchor, and together they demand the whole line.
#[test]
fn anchors_restrict_where_a_term_may_match() {
    assert_eq!(
        ranked("^src"),
        vec!["src/main.rs", "src/algo.rs", "src/main_test.rs"]
    );
    assert_eq!(
        ranked("rs$"),
        vec![
            "main.rs",
            "src/main.rs",
            "src/algo.rs",
            "tests/main.rs",
            "src/main_test.rs"
        ]
    );
    assert_eq!(ranked("^main.rs$"), vec!["main.rs"]);
}

/// `!` inverts a term. `!'` inverts a FUZZY one — the quote flips exactness in
/// the opposite direction once the term is already inverse.
#[test]
fn inverse_terms_subtract() {
    assert_eq!(ranked("!src rs"), vec!["main.rs", "tests/main.rs"]);
    assert_eq!(
        ranked("main !test"),
        vec!["main.rs", "src/main.rs", "docs/main.md"]
    );
    // Inverse-fuzzy: `src/main_test.rs` has s-r-c as a subsequence, so it goes
    // too — an inverse EXACT term (`!src`) would have kept it.
    assert_eq!(
        ranked("!'src"),
        vec!["tests/main.rs", "docs/main.md", "Makefile", "main.rs"]
    );
}

/// A query of nothing but inverse terms scores every survivor 0, so fzf leaves
/// them in INPUT order instead of letting the length tiebreak reorder them
/// (`sortable`, pattern.go:100). Sorting here would put `Makefile` first.
#[test]
fn inverse_only_query_keeps_input_order() {
    assert_eq!(
        ranked("!'src"),
        vec!["tests/main.rs", "docs/main.md", "Makefile", "main.rs"]
    );
}

/// `'term'` (quoted both sides) matches only at a word boundary, unlike `'term`
/// which matches any substring.
#[test]
fn quoted_term_matches_on_boundaries() {
    assert_eq!(
        ranked("'main'"),
        vec![
            "main.rs",
            "src/main.rs",
            "docs/main.md",
            "tests/main.rs",
            "src/main_test.rs"
        ]
    );
}

/// The incremental re-filter in the picker may only reuse the previous hit set
/// when growing the query can only NARROW it. An `|` or a `!` breaks that, and
/// reusing the old hits there drops lines that should have appeared.
#[test]
fn only_plain_queries_are_safe_to_narrow_incrementally() {
    use arb::fzf::Case;
    use arb::pattern::Pattern;
    let cacheable = |q: &str| Pattern::build(q, false, true, Case::Smart).cacheable;
    assert!(cacheable("src"));
    assert!(cacheable("src rs"));
    assert!(!cacheable("src | docs"));
    assert!(!cacheable("!src"));
    assert!(!cacheable("^src"));
    // Growing `src` into `src | docs` really does widen the set, which is what
    // makes the gate necessary rather than merely cautious.
    assert!(ranked("src | docs").len() > ranked("src").len());
}

// ── Scoring schemes and tiebreak criteria ──────────────────────────────────
// Expectations captured from `fzf --scheme=… --filter …` and
// `fzf --tiebreak=… --filter …` on fzf 0.74.3.

/// Rank `lines` under an explicit `Look`.
fn ranked_with(lines: &[&'static str], query: &str, look: &arb::fzf::Look) -> Vec<&'static str> {
    let order = arb::tui::rank(lines, query, false, false, false, look);
    order.into_iter().map(|i| lines[i]).collect()
}

/// `--tiebreak=pathname` prefers a match inside the LAST path segment. It needs
/// both of the settings fzf derives from the criteria rather than from a flag:
/// a backward scan, and the backtrace that makes the begin offset accurate.
/// Without either, both lines score the same and input order survives.
#[test]
fn pathname_tiebreak_prefers_the_last_segment() {
    let lines: [&str; 2] = ["/conf/x/y", "/usr/local/etc/conf"];
    let look = arb::fzf::Look {
        tiebreak: arb::pattern::parse_tiebreak("pathname").unwrap(),
        ..arb::fzf::Look::default()
    };
    assert_eq!(
        ranked_with(&lines, "conf", &look),
        vec!["/usr/local/etc/conf", "/conf/x/y"]
    );
    // The two score identically, so `index` leaves them in input order — which
    // is what makes the case above a real test of the criterion.
    let by_index = arb::fzf::Look {
        tiebreak: arb::pattern::parse_tiebreak("index").unwrap(),
        ..arb::fzf::Look::default()
    };
    assert_eq!(
        ranked_with(&lines, "conf", &by_index),
        vec!["/conf/x/y", "/usr/local/etc/conf"]
    );
}

/// fzf derives the scan direction and the need for a backtrace from the
/// criteria (core.go:215) — neither is a user-facing option, and getting them
/// wrong silently changes the order for four of the six tiebreaks.
#[test]
fn scan_direction_follows_the_criteria() {
    use arb::pattern::{parse_tiebreak, scan_direction};
    let dir = |s: &str| scan_direction(&parse_tiebreak(s).unwrap());
    assert_eq!(dir("length"), (true, false));
    assert_eq!(dir("begin"), (true, false));
    assert_eq!(dir("index"), (true, false));
    assert_eq!(dir("end"), (false, false)); // last best match
    assert_eq!(dir("chunk"), (true, true)); // needs an accurate begin
    assert_eq!(dir("pathname"), (false, true)); // both
                                                // An earlier criterion wins: the walk runs last-to-first.
    assert_eq!(dir("begin,end"), (true, false));
    assert_eq!(dir("end,begin"), (false, false));
}

/// A `--tiebreak` spec fzf rejects must not silently reorder — duplicates, an
/// `index` that isn't last, unknown names and over-long lists all keep the
/// caller's criteria.
#[test]
fn invalid_tiebreak_specs_are_rejected() {
    use arb::pattern::parse_tiebreak;
    assert!(parse_tiebreak("length,length").is_none());
    assert!(parse_tiebreak("index,length").is_none());
    assert!(parse_tiebreak("nonsense").is_none());
    assert!(parse_tiebreak("begin,end,chunk,length").is_none());
    assert!(parse_tiebreak("begin,end,chunk").is_some());
}

/// `--scheme=path` stops treating a whitespace boundary as special and makes
/// `/` the only delimiter, so a path ranks by its segments. It also carries its
/// own tiebreak order.
#[test]
fn path_scheme_brings_its_own_criteria() {
    use arb::pattern::{scheme_criteria, Criterion};
    assert_eq!(
        scheme_criteria(&arb::algo::Scheme::PATH),
        vec![Criterion::Score, Criterion::Pathname, Criterion::Length]
    );
    assert_eq!(
        scheme_criteria(&arb::algo::Scheme::HISTORY),
        vec![Criterion::Score]
    );
    assert_eq!(
        scheme_criteria(&arb::algo::Scheme::DEFAULT),
        vec![Criterion::Score, Criterion::Length]
    );
}

/// `--accept-nth` prints fields, not lines. A field carries the delimiter that
/// followed it, so joining fields 2 and 3 of `a:b:c` is `b:c` — only the very
/// last delimiter is stripped.
#[test]
fn accept_nth_keeps_inner_delimiters() {
    use arb::fzf::{AcceptNth, Nth};
    let by = |s: &str| AcceptNth::parse(s).unwrap().render("a:b:c", Some(":"), 7);
    assert_eq!(by("2,3"), "b:c");
    assert_eq!(by("2"), "b");
    assert_eq!(by("2.."), "b:c");
    assert_eq!(by("-1"), "c");
    // A template splices literal text and `{n}`, the line's input index.
    assert_eq!(by("[{n}] {2}"), "[7] b");
    assert_eq!(by("{1}-{3}"), "a-c");
    // A bare field list is not a template, and a template needs a placeholder.
    assert!(matches!(
        AcceptNth::parse("2,3"),
        Some(AcceptNth::Fields(_))
    ));
    assert!(AcceptNth::parse("no placeholder here").is_none());
    assert_eq!(Nth::parse_list("2,3").len(), 2);
}

// ── `--nth` vs `--with-nth` ────────────────────────────────────────────────
// The two look alike and behave differently in three ways. Each expectation
// below was read off `fzf -d / … --filter …` on fzf 0.74.3 with the line
// `/a/bb/ccc`, whose `/`-tokens are ["/", "a/", "bb/", "ccc"].

/// `--with-nth` rebuilds the item, so its fields keep the delimiters that
/// followed them and a query can match across them. `--nth` only redirects the
/// query at a field, which is trimmed.
#[test]
fn with_nth_keeps_delimiters_and_nth_does_not() {
    use arb::fzf::{Look, Nth};
    let look = |with: &str, nth: &str| Look {
        delimiter: Some("/".into()),
        with_nth: Nth::parse_list(with),
        nth: Nth::parse_list(nth),
        ..Look::default()
    };
    // `--with-nth 2` is `a/`, delimiter included.
    assert_eq!(arb::tui::search_key("/a/bb/ccc", &look("2", "")), "a/");
    assert_eq!(arb::tui::search_key("/a/bb/ccc", &look("2,3", "")), "a/bb/");
    // `--nth 2` is `a`, trimmed.
    assert_eq!(arb::tui::search_key("/a/bb/ccc", &look("", "2")), "a");
}

/// They COMPOSE, in one order: `--with-nth` replaces the item, then `--nth`
/// selects from that. Applying both to the original line searches a field that
/// no longer exists in the item fzf would have searched.
#[test]
fn nth_selects_from_the_with_nth_projection() {
    use arb::fzf::{Look, Nth};
    let look = Look {
        delimiter: Some("/".into()),
        with_nth: Nth::parse_list("2,3"),
        nth: Nth::parse_list("2"),
        ..Look::default()
    };
    // item = "a/bb/" → its fields are ["a/", "bb/"] → nth 2 is "bb".
    assert_eq!(arb::tui::search_key("/a/bb/ccc", &look), "bb");
}

/// The `length` tiebreak measures the ITEM. `--with-nth` changes the item, so it
/// changes the length; `--nth` does not. Two lines with an identical field 2 and
/// different overall lengths therefore rank one way under `--nth` and stay in
/// input order under `--with-nth`.
#[test]
fn length_tiebreak_measures_the_item_not_the_searched_field() {
    use arb::fzf::{Look, Nth};
    let lines: [&str; 2] = ["/a/xxxxxxxxxxxx", "/a/y"];
    let ranked = |look: &Look| -> Vec<&'static str> {
        arb::tui::rank(&lines, "a", false, false, false, look)
            .into_iter()
            .map(|i| lines[i])
            .collect()
    };
    let by_nth = Look {
        delimiter: Some("/".into()),
        nth: Nth::parse_list("2"),
        ..Look::default()
    };
    // The item is the whole line, so the shorter line wins.
    assert_eq!(ranked(&by_nth), vec!["/a/y", "/a/xxxxxxxxxxxx"]);
    let by_with_nth = Look {
        delimiter: Some("/".into()),
        with_nth: Nth::parse_list("2"),
        ..Look::default()
    };
    // The item is `a/` for both, so they tie and keep input order.
    assert_eq!(ranked(&by_with_nth), vec!["/a/xxxxxxxxxxxx", "/a/y"]);
}
