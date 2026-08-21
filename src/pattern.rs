//! A port of fzf's query language — `src/pattern.go` from fzf 0.74.3.
//!
//! fzf's default search mode is EXTENDED: a query is a space-separated list of
//! terms that must all match (AND), `|` between terms makes an OR set, and a
//! term can carry a sigil that changes how it matches:
//!
//! ```text
//! fuzzy      'exact      ^prefix      suffix$      ^equal$      'boundary'
//! !inverse   !'inverse-fuzzy          !^inverse-prefix          !inverse-suffix$
//! ```
//!
//! Everything here is a PORT, not an approximation — `parseTerms`, the term
//! type flips, `extendedMatch`'s OR/AND/inverse walk and the `sortable` /
//! `cacheable` flags all follow the Go source, so `arb --fzf` answers a query
//! the way `fzf` answers it. `-x`/`--extended` and `+x`/`--no-extended` select
//! the mode; extended is the default, as in fzf.
//!
//! A [`Pattern`] is built ONCE per query and scored against every line. That is
//! deliberate: the pattern's `Vec<char>` and case decision are per-QUERY work,
//! and doing them per line (as arb did before this module existed) cost more
//! than the match itself on a query that rejects most of the corpus.

use crate::algo::{self, Match, Text};
use crate::fzf::Case;

/// How one term matches — fzf's `termType` (pattern.go:22).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TermType {
    Fuzzy,
    Exact,
    ExactBoundary,
    Prefix,
    Suffix,
    Equal,
}

/// One term of a query: the text to look for, how to look for it, and whether
/// finding it DISQUALIFIES the line (`!`).
#[derive(Clone, Debug)]
pub struct Term {
    pub typ: TermType,
    pub inv: bool,
    pub text: Vec<char>,
    pub case_sensitive: bool,
}

impl Term {
    /// Run this term's matcher against one line.
    fn run(
        &self,
        scheme: &crate::algo::Scheme,
        forward: bool,
        algo_v1: bool,
        input: &Text,
        with_pos: bool,
    ) -> Option<(Match, Option<Vec<usize>>)> {
        let f = match self.typ {
            TermType::Fuzzy => match algo_v1 {
                true => algo::fuzzy_match_v1,
                false => algo::fuzzy_match_v2,
            },
            TermType::Exact => algo::exact_match_naive,
            TermType::ExactBoundary => algo::exact_match_boundary,
            TermType::Prefix => algo::prefix_match,
            TermType::Suffix => algo::suffix_match,
            TermType::Equal => algo::equal_match,
        };
        f(
            scheme,
            self.case_sensitive,
            forward,
            input,
            &self.text,
            with_pos,
        )
    }
}

/// A parsed query, ready to score any number of lines.
#[derive(Clone, Debug)]
pub struct Pattern {
    /// `false` under `-e`/`--exact`, which turns every bare term into a
    /// substring match (fzf's `fuzzy` flag, inverted from arb's `exact`).
    fuzzy: bool,
    extended: bool,
    /// Non-extended state: the whole query as one term.
    text: Vec<char>,
    case_sensitive: bool,
    /// Extended state: AND across the outer Vec, OR within each inner one.
    term_sets: Vec<Vec<Term>>,
    /// fzf's `sortable` (pattern.go:100): a query of nothing but inverse terms
    /// scores every survivor 0, so fzf leaves them in input order instead of
    /// letting the tiebreak reshuffle them.
    pub sortable: bool,
    /// fzf's `cacheable` (pattern.go:115): every term set is a single plain,
    /// non-inverse term, so extending the query can only ever NARROW the result
    /// set. This is what makes the picker's incremental re-filter sound — an
    /// inverse term or an `|` can WIDEN it, and then the old hit set is not a
    /// superset of the new one.
    pub cacheable: bool,
    /// `--scheme`: which characters count as boundaries and what a match just
    /// after one is worth. Carried on the pattern so the scorer never depends on
    /// a process-wide setting.
    pub scheme: crate::algo::Scheme,
    /// Scan direction, and whether the matchers must backtrack for an accurate
    /// begin offset. Neither is a user option: fzf derives both from the
    /// `--tiebreak` criteria (core.go:215), because `end` and `pathname` want
    /// the LAST best match and `chunk`/`pathname` need to know where it starts.
    forward: bool,
    with_pos: bool,
    /// `--algo=v1`: use fzf's older greedy matcher instead of the score matrix.
    algo_v1: bool,
}

impl Pattern {
    /// fzf's `BuildPattern` (pattern.go:79). `exact` is `-e`/`--exact`,
    /// `extended` is `-x`/`+x`, `case` is `-i`/`+i`/`--smart-case`.
    pub fn build(query: &str, exact: bool, extended: bool, case: Case) -> Pattern {
        Pattern::build_with(query, exact, extended, case, crate::algo::Scheme::DEFAULT)
    }

    /// [`Pattern::build`] under an explicit `--scheme`.
    pub fn build_with(
        query: &str,
        exact: bool,
        extended: bool,
        case: Case,
        scheme: crate::algo::Scheme,
    ) -> Pattern {
        Pattern::build_ranked(
            query,
            exact,
            extended,
            case,
            scheme,
            &scheme_criteria(&scheme),
        )
    }

    /// [`Pattern::build_with`] told which `--tiebreak` criteria the ranking will
    /// use, so the matchers can scan in the direction those criteria need.
    pub fn build_ranked(
        query: &str,
        exact: bool,
        extended: bool,
        case: Case,
        scheme: crate::algo::Scheme,
        criteria: &[Criterion],
    ) -> Pattern {
        Pattern::build_full(query, exact, extended, case, scheme, criteria, false)
    }

    /// [`Pattern::build_ranked`] with `--algo` chosen too.
    #[allow(clippy::too_many_arguments)]
    pub fn build_full(
        query: &str,
        exact: bool,
        extended: bool,
        case: Case,
        scheme: crate::algo::Scheme,
        criteria: &[Criterion],
        algo_v1: bool,
    ) -> Pattern {
        let (forward, with_pos) = scan_direction(criteria);
        let fuzzy = !exact;
        // Extended mode trims the query's outer spaces (they are term
        // separators, not text) — but an escaped `\ ` at the end is real.
        let as_string = match extended {
            true => {
                let mut s = query.trim_start_matches(' ');
                while s.ends_with(' ') && !s.ends_with("\\ ") {
                    s = &s[..s.len() - 1];
                }
                s.to_string()
            }
            false => query.to_string(),
        };

        if !extended {
            let lower = as_string.to_lowercase();
            let case_sensitive = match case {
                Case::Respect => true,
                Case::Ignore => false,
                Case::Smart => lower != as_string,
            };
            let text = match case_sensitive {
                true => as_string.chars().collect(),
                false => lower.chars().collect(),
            };
            return Pattern {
                fuzzy,
                extended,
                text,
                case_sensitive,
                term_sets: Vec::new(),
                sortable: true,
                cacheable: true,
                scheme,
                forward,
                with_pos,
                algo_v1,
            };
        }

        let term_sets = parse_terms(fuzzy, case, &as_string);
        // A query of only inverse terms is not sortable; one plain term per set
        // (and nothing inverted) is cacheable.
        let mut sortable = false;
        let mut cacheable = true;
        for set in &term_sets {
            for (idx, term) in set.iter().enumerate() {
                if !term.inv {
                    sortable = true;
                }
                let plain = match fuzzy {
                    true => term.typ == TermType::Fuzzy,
                    false => term.typ == TermType::Exact,
                };
                if idx > 0 || term.inv || !plain {
                    cacheable = false;
                }
            }
        }
        Pattern {
            fuzzy,
            extended,
            text: Vec::new(),
            case_sensitive: true,
            term_sets,
            sortable,
            cacheable,
            scheme,
            forward,
            with_pos,
            algo_v1,
        }
    }

    /// fzf's `Pattern.IsEmpty` (pattern.go:252) — an empty query matches
    /// everything, and the picker shows the stream untouched.
    pub fn is_empty(&self) -> bool {
        match self.extended {
            true => self.term_sets.is_empty(),
            false => self.text.is_empty(),
        }
    }

    /// The score for one line, or `None` when the line does not match. This is
    /// fzf's `MatchItem` (pattern.go:388) reduced to the score — the offsets it
    /// also returns drive highlighting, which [`Pattern::positions`] handles.
    pub fn score(&self, line: &str) -> Option<i32> {
        if self.is_empty() {
            return Some(0);
        }
        let text = Text::new(line);
        match self.extended {
            true => self.extended_match(&text, false).map(|(s, _)| s),
            false => self.basic_match(&text, false).map(|(m, _)| m.score),
        }
    }

    /// The character indices this pattern matched, for fzf's highlight. Sorted
    /// and deduplicated: an OR set can match overlapping runs.
    pub fn positions(&self, line: &str) -> Vec<usize> {
        if self.is_empty() {
            return Vec::new();
        }
        let text = Text::new(line);
        let mut pos = match self.extended {
            true => self.extended_match(&text, true).and_then(|(_, p)| p),
            false => self.basic_match(&text, true).and_then(|(_, p)| p),
        }
        .unwrap_or_default();
        pos.sort_unstable();
        pos.dedup();
        pos
    }

    /// The score AND the match bounds for one line — fzf's `MatchItem` in full.
    /// [`Pattern::score`] is this with the bounds dropped; the tiebreak criteria
    /// need them, because `begin`, `end`, `chunk` and `pathname` all rank by
    /// WHERE the match landed rather than how well it scored.
    pub fn match_line(&self, line: &str) -> Option<Bounds> {
        if self.is_empty() {
            return Some(Bounds::empty(0));
        }
        let text = Text::new(line);
        match self.extended {
            true => self.extended_bounds(&text),
            false => self.basic_match(&text, self.with_pos).map(|(m, _)| {
                let mut b = Bounds::empty(m.score);
                b.add(m.start, m.end);
                b
            }),
        }
    }

    /// fzf's `basicMatch` (pattern.go:403) — `+x`: the whole query is one term.
    fn basic_match(&self, text: &Text, with_pos: bool) -> Option<(Match, Option<Vec<usize>>)> {
        match self.fuzzy {
            true if self.algo_v1 => algo::fuzzy_match_v1(
                &self.scheme,
                self.case_sensitive,
                self.forward,
                text,
                &self.text,
                with_pos,
            ),
            true => algo::fuzzy_match_v2(
                &self.scheme,
                self.case_sensitive,
                self.forward,
                text,
                &self.text,
                with_pos,
            ),
            false => algo::exact_match_naive(
                &self.scheme,
                self.case_sensitive,
                self.forward,
                text,
                &self.text,
                with_pos,
            ),
        }
    }

    /// [`Pattern::extended_match`] keeping the offsets instead of the positions,
    /// so the tiebreak criteria can see where each term set matched.
    fn extended_bounds(&self, text: &Text) -> Option<Bounds> {
        let mut bounds = Bounds::empty(0);
        for set in &self.term_sets {
            let mut matched = false;
            let mut current = 0i32;
            let mut offset = (0usize, 0usize);
            for term in set {
                match term.run(
                    &self.scheme,
                    self.forward,
                    self.algo_v1,
                    text,
                    self.with_pos,
                ) {
                    Some((m, _)) => {
                        if term.inv {
                            continue;
                        }
                        current = m.score;
                        offset = (m.start, m.end);
                        matched = true;
                        break;
                    }
                    None if term.inv => {
                        // A satisfied inverse term contributes an EMPTY offset,
                        // which `Bounds::add` ignores — it locates nothing.
                        current = 0;
                        offset = (0, 0);
                        matched = true;
                        continue;
                    }
                    None => {}
                }
            }
            if !matched {
                return None;
            }
            bounds.score += current;
            bounds.add(offset.0, offset.1);
        }
        Some(bounds)
    }

    /// fzf's `extendedMatch` (pattern.go:416). Every term set must produce a
    /// match; within a set the FIRST matching term wins and the rest are
    /// skipped, which is what makes `|` an OR. An inverse term matching kills
    /// the set unless a later term in it matches; an inverse term NOT matching
    /// satisfies the set with a score of zero.
    fn extended_match(&self, text: &Text, with_pos: bool) -> Option<(i32, Option<Vec<usize>>)> {
        let mut total = 0i32;
        let mut all_pos = with_pos.then(Vec::new);
        for set in &self.term_sets {
            let mut matched = false;
            let mut current = 0i32;
            for term in set {
                match term.run(&self.scheme, self.forward, self.algo_v1, text, with_pos) {
                    Some((m, pos)) => {
                        if term.inv {
                            continue;
                        }
                        current = m.score;
                        matched = true;
                        if let Some(all) = all_pos.as_mut() {
                            match pos {
                                Some(p) => all.extend(p),
                                None => all.extend(m.start..m.end),
                            }
                        }
                        break;
                    }
                    None if term.inv => {
                        current = 0;
                        matched = true;
                        continue;
                    }
                    None => {}
                }
            }
            // One unsatisfied set is enough to reject the line (fzf compares
            // the offset count against the term-set count).
            if !matched {
                return None;
            }
            total += current;
        }
        Some((total, all_pos))
    }
}

/// fzf's `parseTerms` (pattern.go:166). `\ ` escapes a space into the term
/// rather than splitting on it, so it is swapped for a tab across the split and
/// swapped back after.
fn parse_terms(fuzzy: bool, case: Case, query: &str) -> Vec<Vec<Term>> {
    let escaped = query.replace("\\ ", "\t");
    let mut sets: Vec<Vec<Term>> = Vec::new();
    let mut set: Vec<Term> = Vec::new();
    let mut switch_set = false;
    let mut after_bar = false;

    for token in escaped.split(' ').filter(|t| !t.is_empty()) {
        let mut typ = TermType::Fuzzy;
        let mut inv = false;
        let mut text = token.replace('\t', " ");
        let lower = text.to_lowercase();
        let case_sensitive = match case {
            Case::Respect => true,
            Case::Ignore => false,
            Case::Smart => text != lower,
        };
        if !case_sensitive {
            text = lower;
        }
        if !fuzzy {
            typ = TermType::Exact;
        }

        // A bare `|` joins the previous term into an OR set instead of starting
        // a new one. Two in a row is not an operator, it is a term.
        if !set.is_empty() && !after_bar && text == "|" {
            switch_set = false;
            after_bar = true;
            continue;
        }
        after_bar = false;

        if let Some(rest) = text.strip_prefix('!') {
            inv = true;
            typ = TermType::Exact;
            text = rest.to_string();
        }

        if text != "$" {
            if let Some(rest) = text.strip_suffix('$') {
                typ = TermType::Suffix;
                text = rest.to_string();
            }
        }

        if text.len() > 2 && text.starts_with('\'') && text.ends_with('\'') {
            typ = TermType::ExactBoundary;
            text = text[1..text.len() - 1].to_string();
        } else if let Some(rest) = text.strip_prefix('\'') {
            // `'` flips exactness: it makes a fuzzy search exact, and an exact
            // search (`--exact`, or an inverse term) fuzzy.
            typ = match fuzzy && !inv {
                true => TermType::Exact,
                false => TermType::Fuzzy,
            };
            text = rest.to_string();
        } else if let Some(rest) = text.strip_prefix('^') {
            typ = match typ == TermType::Suffix {
                true => TermType::Equal,
                false => TermType::Prefix,
            };
            text = rest.to_string();
        }

        if text.is_empty() {
            continue;
        }
        if switch_set {
            sets.push(std::mem::take(&mut set));
        }
        set.push(Term {
            typ,
            inv,
            text: text.chars().collect(),
            case_sensitive,
        });
        switch_set = true;
    }
    if !set.is_empty() {
        sets.push(set);
    }
    sets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(query: &str) -> Vec<Vec<Term>> {
        parse_terms(true, Case::Smart, query)
    }

    #[test]
    fn space_separated_terms_are_anded() {
        let p = Pattern::build("usr bin", false, true, Case::Smart);
        assert_eq!(p.term_sets.len(), 2);
        assert!(p.score("/usr/local/bin").is_some());
        assert!(p.score("/usr/local/lib").is_none());
        assert!(p.score("/opt/bin").is_none());
    }

    #[test]
    fn bar_makes_an_or_set() {
        let p = Pattern::build("^/usr | ^/opt", false, true, Case::Smart);
        assert_eq!(p.term_sets.len(), 1);
        assert_eq!(p.term_sets[0].len(), 2);
        assert!(p.score("/usr/bin").is_some());
        assert!(p.score("/opt/bin").is_some());
        assert!(p.score("/var/bin").is_none());
    }

    #[test]
    fn sigils_select_the_term_type() {
        assert_eq!(terms("'abc")[0][0].typ, TermType::Exact);
        assert_eq!(terms("^abc")[0][0].typ, TermType::Prefix);
        assert_eq!(terms("abc$")[0][0].typ, TermType::Suffix);
        assert_eq!(terms("^abc$")[0][0].typ, TermType::Equal);
        assert_eq!(terms("'abc'")[0][0].typ, TermType::ExactBoundary);
        // `!` alone is an exact inverse; `!'` is a FUZZY inverse.
        assert!(terms("!abc")[0][0].inv);
        assert_eq!(terms("!abc")[0][0].typ, TermType::Exact);
        assert_eq!(terms("!'abc")[0][0].typ, TermType::Fuzzy);
        assert_eq!(terms("!^abc")[0][0].typ, TermType::Prefix);
        // A lone `$` is a term, not a suffix marker.
        assert_eq!(terms("$")[0][0].typ, TermType::Fuzzy);
    }

    #[test]
    fn inverse_term_rejects_only_what_it_matches() {
        let p = Pattern::build("!log", false, true, Case::Smart);
        assert!(p.score("main.rs").is_some());
        assert!(p.score("main.log").is_none());
        // Only inverse terms → fzf keeps input order rather than ranking.
        assert!(!p.sortable);
        assert!(!p.cacheable);
    }

    #[test]
    fn prefix_and_suffix_anchor() {
        let p = Pattern::build("^src", false, true, Case::Smart);
        assert!(p.score("src/main.rs").is_some());
        assert!(p.score("lib/src/main.rs").is_none());
        let p = Pattern::build("rs$", false, true, Case::Smart);
        assert!(p.score("src/main.rs").is_some());
        assert!(p.score("src/main.rs.bak").is_none());
    }

    #[test]
    fn equal_matches_the_whole_line_only() {
        let p = Pattern::build("^main.rs$", false, true, Case::Smart);
        assert!(p.score("main.rs").is_some());
        assert!(p.score("  main.rs  ").is_some()); // surrounding space is trimmed
        assert!(p.score("src/main.rs").is_none());
    }

    #[test]
    fn escaped_space_stays_in_the_term() {
        let p = Pattern::build("my\\ file", false, true, Case::Smart);
        assert_eq!(p.term_sets.len(), 1);
        assert_eq!(p.term_sets[0][0].text.iter().collect::<String>(), "my file");
    }

    #[test]
    fn cacheable_only_for_plain_and_terms() {
        // Plain terms can only narrow as the query grows — the picker's
        // incremental re-filter depends on this being right.
        assert!(Pattern::build("abc", false, true, Case::Smart).cacheable);
        assert!(Pattern::build("abc def", false, true, Case::Smart).cacheable);
        assert!(!Pattern::build("abc | def", false, true, Case::Smart).cacheable);
        assert!(!Pattern::build("!abc", false, true, Case::Smart).cacheable);
        assert!(!Pattern::build("^abc", false, true, Case::Smart).cacheable);
        assert!(!Pattern::build("'abc", false, true, Case::Smart).cacheable);
    }

    #[test]
    fn non_extended_treats_the_query_as_one_term() {
        let p = Pattern::build("usr bin", false, false, Case::Smart);
        assert!(p.term_sets.is_empty());
        // The space is part of the pattern, so it must appear in the line.
        assert!(p.score("usr x bin").is_some());
        assert!(p.score("/usr/local/bin").is_none());
    }

    #[test]
    fn smart_case_is_per_term() {
        let p = Pattern::build("readme Cargo", false, true, Case::Smart);
        assert!(!p.term_sets[0][0].case_sensitive);
        assert!(p.term_sets[1][0].case_sensitive);
        assert!(p.score("README / Cargo.toml").is_some());
        assert!(p.score("README / cargo.toml").is_none());
    }

    #[test]
    fn empty_query_matches_everything() {
        let p = Pattern::build("", false, true, Case::Smart);
        assert!(p.is_empty());
        assert_eq!(p.score("anything"), Some(0));
    }
}

/// Where a pattern matched a line, folded across every term set — fzf's
/// `buildResult` inputs (result.go:32). `min_begin`/`min_end`/`max_end` are
/// character indices; a term that located nothing (a satisfied `!term`)
/// contributes an empty offset and is skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub score: i32,
    pub min_begin: usize,
    pub min_end: usize,
    pub max_end: usize,
    /// False when nothing located a position — every position-based criterion
    /// then scores the worst possible value, as in fzf.
    pub valid: bool,
}

impl Bounds {
    fn empty(score: i32) -> Bounds {
        Bounds {
            score,
            min_begin: u16::MAX as usize,
            min_end: u16::MAX as usize,
            max_end: 0,
            valid: false,
        }
    }
    fn add(&mut self, begin: usize, end: usize) {
        if begin >= end {
            return;
        }
        self.min_begin = self.min_begin.min(begin);
        self.min_end = self.min_end.min(end);
        self.max_end = self.max_end.max(end);
        self.valid = true;
    }
}

/// One ranking criterion — fzf's `criterion` (options.go:268). `Score` is
/// always first; the rest are the `--tiebreak` list, applied in order, with the
/// item's input index as the final, implicit tiebreak.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Criterion {
    Score,
    Chunk,
    Length,
    Begin,
    End,
    Pathname,
}

/// fzf's `parseTiebreak` (options.go:1349). `index` adds no criterion — it IS
/// the implicit final one — but it must come last, and no name may repeat.
/// `None` for a spec fzf would reject, which keeps the caller's default.
pub fn parse_tiebreak(spec: &str) -> Option<Vec<Criterion>> {
    let lowered = spec.to_lowercase();
    let mut out = vec![Criterion::Score];
    let mut seen: Vec<&str> = Vec::new();
    let mut has_index = false;
    for name in lowered.split(',') {
        // A repeat is an error, and `index` must be last — it is the implicit
        // final criterion, so nothing may follow it.
        if seen.contains(&name) || has_index {
            return None;
        }
        seen.push(name);
        match name {
            "index" => has_index = true,
            "chunk" => out.push(Criterion::Chunk),
            "pathname" => out.push(Criterion::Pathname),
            "length" => out.push(Criterion::Length),
            "begin" => out.push(Criterion::Begin),
            "end" => out.push(Criterion::End),
            _ => return None,
        }
    }
    // fzf's points vector is four slots wide: score plus at most three tiebreaks.
    match out.len() > 4 {
        true => None,
        false => Some(out),
    }
}

/// The criteria a `--scheme` implies when `--tiebreak` is not given — fzf's
/// `parseScheme` (options.go:1336).
pub fn scheme_criteria(scheme: &crate::algo::Scheme) -> Vec<Criterion> {
    if *scheme == crate::algo::Scheme::PATH {
        return vec![Criterion::Score, Criterion::Pathname, Criterion::Length];
    }
    if *scheme == crate::algo::Scheme::HISTORY {
        return vec![Criterion::Score];
    }
    vec![Criterion::Score, Criterion::Length]
}

/// fzf's `buildResultFromBounds` (result.go:54): pack the criteria into four
/// `u16` slots, best-first, so ordering is one integer comparison per pair.
///
/// The slots fill from the END (`points[3 - idx]`), which is what makes the
/// packed comparison in [`compare_points`] read criteria in declaration order.
pub fn points(line: &str, b: &Bounds, criteria: &[Criterion]) -> [u16; 4] {
    let text: Vec<char> = line.chars().collect();
    let num_chars = text.len();
    let as_u16 = |v: i64| v.clamp(0, u16::MAX as i64) as u16;
    let mut pts = [0u16; 4];
    for (idx, criterion) in criteria.iter().enumerate().take(4) {
        let val = match criterion {
            // Higher score sorts first, so it is stored inverted.
            Criterion::Score => u16::MAX - as_u16(b.score as i64),
            Criterion::Chunk if b.valid => {
                // Widen the match to the whitespace-delimited chunk holding it;
                // the shorter that chunk, the better the match fits.
                let mut s = b.min_begin;
                while s >= 1 && !text[s - 1].is_whitespace() {
                    s -= 1;
                }
                let mut e = b.max_end;
                while e < num_chars && !text[e].is_whitespace() {
                    e += 1;
                }
                as_u16((e - s) as i64)
            }
            Criterion::Length => as_u16(trim_length(line) as i64),
            Criterion::Pathname if b.valid => {
                // Prefer a match in the last path segment. fzf scans BYTES here
                // while `min_begin` counts characters; the two agree on the
                // ASCII paths this criterion exists for, and the port keeps the
                // comparison as fzf makes it.
                let bytes = line.as_bytes();
                let last_delim = bytes
                    .iter()
                    .rposition(|c| *c == b'/' || *c == b'\\')
                    .map(|i| i as i64)
                    .unwrap_or(-1);
                match last_delim <= b.min_begin as i64 {
                    true => as_u16(b.min_begin as i64 - last_delim),
                    false => u16::MAX,
                }
            }
            Criterion::Begin | Criterion::End if b.valid => {
                let mut white_prefix = 0usize;
                for (i, r) in text.iter().enumerate() {
                    white_prefix = i;
                    if i == b.min_begin || !r.is_whitespace() {
                        break;
                    }
                }
                let trim = trim_length(line) as i64;
                match criterion {
                    // Earlier match first.
                    Criterion::Begin => as_u16(b.min_end as i64 - white_prefix as i64),
                    // Later END first — the match reaching further into the line
                    // is the better one, normalized by the line's own length.
                    _ => as_u16(
                        u16::MAX as i64
                            - (u16::MAX as i64 * (b.max_end as i64 - white_prefix as i64))
                                / (trim + 1),
                    ),
                }
            }
            // A criterion with nothing to measure scores the worst value.
            _ => u16::MAX,
        };
        pts[3 - idx] = val;
    }
    pts
}

/// fzf's `compareRanks` (result_x86.go): the four slots read as one integer,
/// most significant first, then the item's input index breaks a full tie.
pub fn compare_points(a: &[u16; 4], b: &[u16; 4]) -> std::cmp::Ordering {
    for i in (0..4).rev() {
        match a[i].cmp(&b[i]) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

/// The length fzf's `length` criterion compares — `Chars.TrimLength`, the
/// character count with surrounding whitespace removed.
fn trim_length(s: &str) -> usize {
    s.trim().chars().count()
}

/// fzf's `core.go:215`: the `--tiebreak` criteria decide how the matchers scan.
/// Returns `(forward, with_pos)`.
///
/// `end` and `pathname` want the LAST best match rather than the first, so they
/// scan backwards. `chunk` and `pathname` rank by WHERE the match begins, and
/// the begin offset is only accurate when the matcher backtracks — fzf skips
/// that work otherwise, and says so at the bottom of `FuzzyMatchV2`.
///
/// The walk starts at the LAST criterion, so an earlier one wins a conflict.
/// `Score` (always first) is skipped, as in fzf.
pub fn scan_direction(criteria: &[Criterion]) -> (bool, bool) {
    let mut forward = true;
    let mut with_pos = false;
    for c in criteria.iter().skip(1).rev() {
        match c {
            Criterion::Chunk => with_pos = true,
            Criterion::End => forward = false,
            Criterion::Begin => forward = true,
            Criterion::Pathname => {
                with_pos = true;
                forward = false;
            }
            _ => {}
        }
    }
    (forward, with_pos)
}
