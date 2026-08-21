//! A port of fzf's matching algorithm — `src/algo/algo.go` from fzf 0.74.2.
//!
//! This is a PORT, not an approximation: the constants, the char classes, the
//! bonus matrix, the two-phase score matrix and the backtrace all follow the Go
//! source line for line, so `arb --fzf` ranks a query exactly the way `fzf`
//! ranks it. The previous hand-rolled scorer agreed with fzf on toy inputs and
//! diverged on real ones (a `find /` corpus put a different line on top for the
//! same query), which is the whole reason this exists.
//!
//! Ported: `FuzzyMatchV2` (with its `asciiFuzzyIndex` prefilter and position
//! backtrace), `ExactMatchNaive` + `calculateScore` (fzf's `--exact`), and the
//! `charClassOf`/`bonusFor` scoring tables. fzf's `--scheme=path|history`
//! variants and its single/two-character fast paths are not ported; the fast
//! paths are optimizations of the general path this file implements, and the
//! schemes are options arb does not accept yet.

use std::sync::OnceLock;

// ── Scoring constants (algo.go:112-153) ─────────────────────────────────────
const SCORE_MATCH: i16 = 16;
const SCORE_GAP_START: i16 = -3;
const SCORE_GAP_EXTENSION: i16 = -1;
const BONUS_BOUNDARY: i16 = SCORE_MATCH / 2;
const BONUS_NON_WORD: i16 = SCORE_MATCH / 2;
const BONUS_CAMEL123: i16 = BONUS_BOUNDARY + SCORE_GAP_EXTENSION;
const BONUS_CONSECUTIVE: i16 = -(SCORE_GAP_START + SCORE_GAP_EXTENSION);
const BONUS_FIRST_CHAR_MULTIPLIER: i16 = 2;
/// `--scheme=default`: extra bonus after whitespace or at the start of a line.
const BONUS_BOUNDARY_WHITE: i16 = BONUS_BOUNDARY + 2;
/// `--scheme=default`: extra bonus after `/`, `,`, `:`, `;` or `|`.
const BONUS_BOUNDARY_DELIMITER: i16 = BONUS_BOUNDARY + 1;

const DELIMITER_CHARS: &str = "/,:;|";
const WHITE_CHARS: &str = " \t\n\u{b}\u{c}\r\u{85}\u{a0}";

/// The class of a character, in fzf's order — `bonusFor` compares against it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
enum CharClass {
    White = 0,
    NonWord = 1,
    Delimiter = 2,
    Lower = 3,
    Upper = 4,
    Letter = 5,
    Number = 6,
}

/// `--scheme=default` starts as if the line were preceded by whitespace.
const INITIAL_CHAR_CLASS: CharClass = CharClass::White;

fn char_class_of_ascii(c: u8) -> CharClass {
    if c.is_ascii_lowercase() {
        CharClass::Lower
    } else if c.is_ascii_uppercase() {
        CharClass::Upper
    } else if c.is_ascii_digit() {
        CharClass::Number
    } else if WHITE_CHARS.contains(c as char) {
        CharClass::White
    } else if DELIMITER_CHARS.contains(c as char) {
        CharClass::Delimiter
    } else {
        CharClass::NonWord
    }
}

fn char_class_of_non_ascii(c: char) -> CharClass {
    if c.is_lowercase() {
        CharClass::Lower
    } else if c.is_uppercase() {
        CharClass::Upper
    } else if c.is_numeric() {
        CharClass::Number
    } else if c.is_alphabetic() {
        CharClass::Letter
    } else if c.is_whitespace() {
        CharClass::White
    } else if DELIMITER_CHARS.contains(c) {
        CharClass::Delimiter
    } else {
        CharClass::NonWord
    }
}

fn char_class_of(c: char) -> CharClass {
    match u8::try_from(c as u32) {
        Ok(b) if c.is_ascii() => char_class_of_ascii(b),
        _ => char_class_of_non_ascii(c),
    }
}

/// algo.go:268 — the bonus for `class` when it follows `prev`.
fn bonus_for(prev: CharClass, class: CharClass) -> i16 {
    if class >= CharClass::NonWord {
        match prev {
            CharClass::White => return BONUS_BOUNDARY_WHITE,
            CharClass::Delimiter => return BONUS_BOUNDARY_DELIMITER,
            CharClass::NonWord => return BONUS_BOUNDARY,
            _ => {}
        }
    }
    if prev == CharClass::Lower && class == CharClass::Upper
        || prev != CharClass::Number && class == CharClass::Number
    {
        return BONUS_CAMEL123;
    }
    match class {
        CharClass::NonWord | CharClass::Delimiter => BONUS_NON_WORD,
        CharClass::White => BONUS_BOUNDARY_WHITE,
        _ => 0,
    }
}

/// The 7×7 bonus lookup fzf builds in `Init` (algo.go:212).
fn bonus_matrix() -> &'static [[i16; 7]; 7] {
    static M: OnceLock<[[i16; 7]; 7]> = OnceLock::new();
    M.get_or_init(|| {
        const CLASSES: [CharClass; 7] = [
            CharClass::White,
            CharClass::NonWord,
            CharClass::Delimiter,
            CharClass::Lower,
            CharClass::Upper,
            CharClass::Letter,
            CharClass::Number,
        ];
        let mut m = [[0i16; 7]; 7];
        for (i, prev) in CLASSES.iter().enumerate() {
            for (j, class) in CLASSES.iter().enumerate() {
                m[i][j] = bonus_for(*prev, *class);
            }
        }
        m
    })
}

fn bonus_of(prev: CharClass, class: CharClass) -> i16 {
    bonus_matrix()[prev as usize][class as usize]
}

/// The haystack, indexed by CHARACTER like fzf's `util.Chars`. An all-ASCII
/// line borrows its bytes (the common case — no per-keystroke allocation);
/// anything else is decoded once into runes.
pub enum Text<'a> {
    Ascii(&'a [u8]),
    Runes(Vec<char>),
}

impl<'a> Text<'a> {
    pub fn new(s: &'a str) -> Self {
        match s.is_ascii() {
            true => Text::Ascii(s.as_bytes()),
            false => Text::Runes(s.chars().collect()),
        }
    }
    fn len(&self) -> usize {
        match self {
            Text::Ascii(b) => b.len(),
            Text::Runes(r) => r.len(),
        }
    }
    fn get(&self, i: usize) -> char {
        match self {
            Text::Ascii(b) => b[i] as char,
            Text::Runes(r) => r[i],
        }
    }
    /// fzf's `Chars.IsBytes()` — whether the byte-level prefilter can run.
    fn is_bytes(&self) -> bool {
        matches!(self, Text::Ascii(_))
    }
    fn bytes(&self) -> &[u8] {
        match self {
            Text::Ascii(b) => b,
            Text::Runes(_) => &[],
        }
    }
}

/// One match, in fzf's terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub start: usize,
    pub end: usize,
    pub score: i32,
}

/// algo.go:322 — the next index at or after `from` holding `b` (either case
/// when the search is case-insensitive).
fn try_skip(input: &[u8], case_sensitive: bool, b: u8, from: usize) -> Option<usize> {
    let hay = &input[from..];
    if !case_sensitive && b.is_ascii_lowercase() {
        let up = b - 32;
        return hay
            .iter()
            .position(|c| *c == b || *c == up)
            .map(|i| from + i);
    }
    hay.iter().position(|c| *c == b).map(|i| from + i)
}

/// algo.go:348 — a cheap byte-level prefilter that both rejects impossible
/// matches and narrows the score matrix to `[min, max)`. `min` steps one
/// character BACK from the first hit so the boundary bonus sees what precedes it.
fn ascii_fuzzy_index(
    input: &Text,
    pattern: &[char],
    case_sensitive: bool,
) -> Option<(usize, usize)> {
    if !input.is_bytes() {
        return Some((0, input.len()));
    }
    if !pattern.iter().all(char::is_ascii) {
        return None;
    }
    let bytes = input.bytes();
    let (mut first_idx, mut idx, mut last_idx) = (0usize, 0usize, 0usize);
    let mut b = 0u8;
    for (pidx, pc) in pattern.iter().enumerate() {
        b = *pc as u8;
        idx = try_skip(bytes, case_sensitive, b, idx)?;
        if pidx == 0 && idx > 0 {
            first_idx = idx - 1;
        }
        last_idx = idx;
        idx += 1;
    }
    // Limit the scope to the last appearance of the pattern's last character.
    let scope = &bytes[last_idx..];
    if scope.len() > 1 {
        let tail = &scope[1..];
        let end = if !case_sensitive && b.is_ascii_lowercase() {
            let up = b - 32;
            tail.iter().rposition(|c| *c == b || *c == up)
        } else {
            tail.iter().rposition(|c| *c == b)
        };
        if let Some(end) = end {
            return Some((first_idx, last_idx + 1 + end + 1));
        }
    }
    Some((first_idx, last_idx + 1))
}

/// fzf's `FuzzyMatchV2` (algo.go:639). `pattern` must already be lowercase when
/// `case_sensitive` is false, exactly as fzf requires of its callers. Returns
/// the match and, when asked, the matched character positions for highlighting.
pub fn fuzzy_match_v2(
    case_sensitive: bool,
    input: &Text,
    pattern: &[char],
    with_pos: bool,
) -> Option<(Match, Option<Vec<usize>>)> {
    let m = pattern.len();
    if m == 0 {
        return Some((
            Match {
                start: 0,
                end: 0,
                score: 0,
            },
            with_pos.then(Vec::new),
        ));
    }
    if m > input.len() {
        return None;
    }
    let (min_idx, max_idx) = ascii_fuzzy_index(input, pattern, case_sensitive)?;
    let n = max_idx - min_idx;
    if n == 0 {
        return None;
    }

    // Phase 2: per-position bonus, the first row of the score matrix, and the
    // first occurrence of each pattern character. The scratch buffers are
    // per-thread and reused across calls — fzf allocates them from a `util.Slab`
    // for the same reason: a million-line scan would otherwise make five
    // allocations per line and spend more time in the allocator than in scoring.
    SCRATCH.with(|s| {
        let s = &mut *s.borrow_mut();
        s.resize(n, m);
        let (h0, c0, b, f, t, hm, cm) = s.buffers();
        fuzzy_match_v2_inner(
            case_sensitive,
            input,
            pattern,
            with_pos,
            min_idx,
            n,
            m,
            h0,
            c0,
            b,
            f,
            t,
            hm,
            cm,
        )
    })
}

/// Per-thread scratch for the score matrices (fzf's `util.Slab`).
struct Scratch {
    h0: Vec<i16>,
    c0: Vec<i16>,
    b: Vec<i16>,
    f: Vec<usize>,
    t: Vec<char>,
    h: Vec<i16>,
    c: Vec<i16>,
}

impl Scratch {
    fn new() -> Self {
        Scratch {
            h0: Vec::new(),
            c0: Vec::new(),
            b: Vec::new(),
            f: Vec::new(),
            t: Vec::new(),
            h: Vec::new(),
            c: Vec::new(),
        }
    }
    /// Grow (never shrink) to hold one match of an `n`-character window against
    /// an `m`-character pattern, and clear what phase 2 reads.
    fn resize(&mut self, n: usize, m: usize) {
        self.h0.clear();
        self.h0.resize(n, 0);
        self.c0.clear();
        self.c0.resize(n, 0);
        self.b.clear();
        self.b.resize(n, 0);
        self.f.clear();
        self.f.resize(m, 0);
        self.t.clear();
    }
    /// All seven buffers at once — the borrow checker needs them split in one
    /// call, and phase 3 sizes `h`/`c` itself once the matrix width is known.
    #[allow(clippy::type_complexity)]
    fn buffers(
        &mut self,
    ) -> (
        &mut Vec<i16>,
        &mut Vec<i16>,
        &mut Vec<i16>,
        &mut Vec<usize>,
        &mut Vec<char>,
        &mut Vec<i16>,
        &mut Vec<i16>,
    ) {
        (
            &mut self.h0,
            &mut self.c0,
            &mut self.b,
            &mut self.f,
            &mut self.t,
            &mut self.h,
            &mut self.c,
        )
    }
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::new(Scratch::new());
}

/// The body of [`fuzzy_match_v2`], with the scratch buffers handed in.
#[allow(clippy::too_many_arguments)]
fn fuzzy_match_v2_inner(
    case_sensitive: bool,
    input: &Text,
    pattern: &[char],
    with_pos: bool,
    min_idx: usize,
    n: usize,
    m: usize,
    h0: &mut [i16],
    c0: &mut [i16],
    b: &mut [i16],
    f: &mut [usize],
    t: &mut Vec<char>,
    h: &mut Vec<i16>,
    c: &mut Vec<i16>,
) -> Option<(Match, Option<Vec<usize>>)> {
    t.extend((min_idx..min_idx + n).map(|i| input.get(i)));

    let (mut max_score, mut max_score_pos) = (0i16, 0usize);
    let (mut pidx, mut last_idx) = (0usize, 0usize);
    let pchar0 = pattern[0];
    let mut pchar = pattern[0];
    let mut prev_h0 = 0i16;
    let mut prev_class = INITIAL_CHAR_CLASS;
    let mut in_gap = false;
    for off in 0..n {
        let mut ch = t[off];
        let class = if ch.is_ascii() {
            let cl = char_class_of_ascii(ch as u8);
            if !case_sensitive && cl == CharClass::Upper {
                ch = ((ch as u8) + 32) as char;
                t[off] = ch;
            }
            cl
        } else {
            let cl = char_class_of_non_ascii(ch);
            if !case_sensitive && cl == CharClass::Upper {
                ch = ch.to_lowercase().next().unwrap_or(ch);
            }
            t[off] = ch;
            cl
        };

        let bonus = bonus_of(prev_class, class);
        b[off] = bonus;
        prev_class = class;

        if ch == pchar {
            if pidx < m {
                f[pidx] = off;
                pidx += 1;
                pchar = pattern[pidx.min(m - 1)];
            }
            last_idx = off;
        }

        if ch == pchar0 {
            let score = SCORE_MATCH + bonus * BONUS_FIRST_CHAR_MULTIPLIER;
            h0[off] = score;
            c0[off] = 1;
            if m == 1 && score > max_score {
                max_score = score;
                max_score_pos = off;
                if bonus >= BONUS_BOUNDARY {
                    break;
                }
            }
            in_gap = false;
        } else {
            h0[off] = match in_gap {
                true => (prev_h0 + SCORE_GAP_EXTENSION).max(0),
                false => (prev_h0 + SCORE_GAP_START).max(0),
            };
            c0[off] = 0;
            in_gap = true;
        }
        prev_h0 = h0[off];
    }
    if pidx != m {
        return None;
    }
    if m == 1 {
        let start = min_idx + max_score_pos;
        return Some((
            Match {
                start,
                end: start + 1,
                score: max_score as i32,
            },
            with_pos.then(|| vec![start]),
        ));
    }

    // Phase 3: fill the score matrix.
    let f0 = f[0];
    let width = last_idx - f0 + 1;
    h.clear();
    h.resize(width * m, 0);
    h[..width].copy_from_slice(&h0[f0..=last_idx]);
    c.clear();
    c.resize(width * m, 0);
    c[..width].copy_from_slice(&c0[f0..=last_idx]);

    for (off, &fv) in f[1..].iter().enumerate() {
        let pchar = pattern[off + 1];
        let pidx = off + 1;
        let row = pidx * width;
        let mut in_gap = false;
        // Row `pidx` starts at the first place its pattern char can appear.
        h[row + fv - f0 - 1] = 0;
        for (k, &ch) in t[fv..=last_idx].iter().enumerate() {
            let col = k + fv;
            let (mut s1, s2, mut consecutive) = (
                0i16,
                {
                    let left = h[row + fv - f0 - 1 + k];
                    match in_gap {
                        true => left + SCORE_GAP_EXTENSION,
                        false => left + SCORE_GAP_START,
                    }
                },
                0i16,
            );

            if pchar == ch {
                s1 = h[row + fv - f0 - 1 - width + k] + SCORE_MATCH;
                let mut bo = b[col];
                consecutive = c[row + fv - f0 - 1 - width + k] + 1;
                if consecutive > 1 {
                    // `col + 1 - consecutive` in Go's signed arithmetic: the start of the
                    // consecutive chunk. Grouped this way so the usize math never dips
                    // below zero on the way there.
                    let fb = b[col + 1 - consecutive as usize];
                    // Break the consecutive chunk at a stronger boundary.
                    if bo >= BONUS_BOUNDARY && bo > fb {
                        consecutive = 1;
                    } else {
                        bo = bo.max(BONUS_CONSECUTIVE.max(fb));
                    }
                }
                if s1 + bo < s2 {
                    s1 += b[col];
                    consecutive = 0;
                } else {
                    s1 += bo;
                }
            }
            c[row + fv - f0 + k] = consecutive;
            in_gap = s1 < s2;
            let score = s1.max(s2).max(0);
            if pidx == m - 1 && score > max_score {
                max_score = score;
                max_score_pos = col;
            }
            h[row + fv - f0 + k] = score;
        }
    }

    // Phase 4: backtrace for the matched positions (only when highlighting).
    let mut pos = with_pos.then(|| Vec::with_capacity(m));
    let mut j = f0;
    if let Some(pos) = pos.as_mut() {
        let mut i = m - 1;
        j = max_score_pos;
        let mut prefer_match = true;
        loop {
            let idx = i * width;
            let j0 = j - f0;
            let s = h[idx + j0];
            let mut s1 = 0i16;
            let mut s2 = 0i16;
            if i > 0 && j >= f[i] {
                s1 = h[idx - width + j0 - 1];
            }
            if j > f[i] {
                s2 = h[idx + j0 - 1];
            }
            if s > s1 && (s > s2 || s == s2 && prefer_match) {
                pos.push(j + min_idx);
                if i == 0 {
                    break;
                }
                i -= 1;
            }
            prefer_match =
                c[idx + j0] > 1 || idx + width + j0 + 1 < c.len() && c[idx + width + j0 + 1] > 0;
            if j == 0 {
                break;
            }
            j -= 1;
        }
    }
    Some((
        Match {
            start: min_idx + j,
            end: min_idx + max_score_pos + 1,
            score: max_score as i32,
        },
        pos,
    ))
}

/// fzf's `calculateScore` (algo.go) — scores an already-located exact match so
/// `--exact` ranks on the same scale as fuzzy matching.
fn calculate_score(
    case_sensitive: bool,
    text: &Text,
    pattern: &[char],
    sidx: usize,
    eidx: usize,
    with_pos: bool,
) -> (i32, Option<Vec<usize>>) {
    let (mut pidx, mut score, mut in_gap, mut consecutive, mut first_bonus) =
        (0usize, 0i32, false, 0i32, 0i16);
    let mut pos = with_pos.then(|| Vec::with_capacity(pattern.len()));
    let mut prev_class = INITIAL_CHAR_CLASS;
    if sidx > 0 {
        prev_class = char_class_of(text.get(sidx - 1));
    }
    for idx in sidx..eidx {
        let mut ch = text.get(idx);
        let class = char_class_of(ch);
        if !case_sensitive {
            ch = ch.to_lowercase().next().unwrap_or(ch);
        }
        if pidx < pattern.len() && ch == pattern[pidx] {
            if let Some(p) = pos.as_mut() {
                p.push(idx);
            }
            score += SCORE_MATCH as i32;
            let mut bonus = bonus_of(prev_class, class);
            if consecutive == 0 {
                first_bonus = bonus;
            } else {
                if bonus >= BONUS_BOUNDARY && bonus > first_bonus {
                    first_bonus = bonus;
                }
                bonus = bonus.max(first_bonus).max(BONUS_CONSECUTIVE);
            }
            score += if pidx == 0 {
                (bonus * BONUS_FIRST_CHAR_MULTIPLIER) as i32
            } else {
                bonus as i32
            };
            in_gap = false;
            consecutive += 1;
            pidx += 1;
        } else {
            score += match in_gap {
                true => SCORE_GAP_EXTENSION as i32,
                false => SCORE_GAP_START as i32,
            };
            in_gap = true;
            consecutive = 0;
            first_bonus = 0;
        }
        prev_class = class;
    }
    (score, pos)
}

/// fzf's `ExactMatchNaive` (algo.go:808) — `--exact`: the substring occurrence
/// with the best boundary bonus, scored like a fuzzy match.
pub fn exact_match_naive(
    case_sensitive: bool,
    input: &Text,
    pattern: &[char],
    with_pos: bool,
) -> Option<(Match, Option<Vec<usize>>)> {
    if pattern.is_empty() {
        return Some((
            Match {
                start: 0,
                end: 0,
                score: 0,
            },
            with_pos.then(Vec::new),
        ));
    }
    let len_runes = input.len();
    let len_pattern = pattern.len();
    if len_runes < len_pattern {
        return None;
    }
    ascii_fuzzy_index(input, pattern, case_sensitive)?;

    // `index` walks back on a failed partial match (fzf rewinds by `pidx`), so
    // it has to be signed — the Go loop relies on it dipping to -1.
    let mut pidx = 0usize;
    let mut best_pos: Option<usize> = None;
    let mut bonus = 0i16;
    let mut best_bonus = -1i16;
    let mut index: isize = 0;
    while index < len_runes as isize {
        let mut ch = input.get(index as usize);
        if !case_sensitive {
            ch = ch.to_lowercase().next().unwrap_or(ch);
        }
        if ch == pattern[pidx] {
            if pidx == 0 {
                bonus = bonus_at(input, index as usize);
            }
            pidx += 1;
            if pidx == len_pattern {
                if bonus > best_bonus {
                    best_pos = Some(index as usize);
                    best_bonus = bonus;
                }
                if bonus >= BONUS_BOUNDARY {
                    break;
                }
                index -= pidx as isize - 1;
                pidx = 0;
                bonus = 0;
            }
        } else {
            index -= pidx as isize;
            pidx = 0;
            bonus = 0;
        }
        index += 1;
    }
    let best_pos = best_pos?;
    let sidx = best_pos + 1 - len_pattern;
    let eidx = best_pos + 1;
    let (score, pos) = calculate_score(case_sensitive, input, pattern, sidx, eidx, with_pos);
    Some((
        Match {
            start: sidx,
            end: eidx,
            score,
        },
        pos,
    ))
}

/// algo.go:298 — the bonus at one position, with the start of the line treated
/// as if it followed whitespace.
fn bonus_at(input: &Text, idx: usize) -> i16 {
    if idx == 0 {
        return BONUS_BOUNDARY_WHITE;
    }
    bonus_of(
        char_class_of(input.get(idx - 1)),
        char_class_of(input.get(idx)),
    )
}

/// fzf's smart case: a pattern with any uppercase character matches case
/// sensitively; otherwise it is lowercased and the match ignores case.
pub fn prepare_pattern(pattern: &str) -> (Vec<char>, bool) {
    prepare_pattern_case(pattern, crate::fzf::Case::Smart)
}

/// [`prepare_pattern`] under an explicit case mode: smart-case is fzf's default
/// (and what `prepare_pattern` uses), `-i`/`--ignore-case` forces the
/// insensitive path, `+i`/`--no-ignore-case` the sensitive one. Only the
/// insensitive path folds the pattern — a sensitive match compares it as typed.
pub fn prepare_pattern_case(pattern: &str, case: crate::fzf::Case) -> (Vec<char>, bool) {
    let case_sensitive = match case {
        crate::fzf::Case::Smart => pattern.chars().any(char::is_uppercase),
        crate::fzf::Case::Ignore => false,
        crate::fzf::Case::Respect => true,
    };
    let chars = match case_sensitive {
        true => pattern.chars().collect(),
        false => pattern.chars().flat_map(char::to_lowercase).collect(),
    };
    (chars, case_sensitive)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(text: &str, pat: &str) -> Option<i32> {
        let (p, cs) = prepare_pattern(pat);
        fuzzy_match_v2(cs, &Text::new(text), &p, false).map(|(m, _)| m.score)
    }

    fn positions(text: &str, pat: &str) -> Option<Vec<usize>> {
        let (p, cs) = prepare_pattern(pat);
        fuzzy_match_v2(cs, &Text::new(text), &p, true).map(|(_, pos)| {
            let mut v = pos.unwrap();
            v.sort_unstable();
            v
        })
    }

    // The expectations below come from fzf's own algo_test.go (same corpus of
    // cases, scored with the constants ported here).
    #[test]
    fn scores_match_fzfs_own_test_vectors() {
        // "fooBarbaz1" on "obz" (case-insensitive, as fzf lowercases the
        // pattern): match at 2..9, scored from three matches, a camelCase bonus
        // and the gaps between them.
        // (`start` is only backtraced when positions are requested, exactly as
        // in fzf — without them it stays at the first candidate offset.)
        let p: Vec<char> = "obz".chars().collect();
        let (m, _) = fuzzy_match_v2(false, &Text::new("fooBarbaz1"), &p, true).unwrap();
        assert_eq!((m.start, m.end), (2, 9));
        assert_eq!(
            m.score,
            (SCORE_MATCH * 3 + BONUS_CAMEL123 + SCORE_GAP_START + SCORE_GAP_EXTENSION * 3) as i32
        );
        // A word-boundary start earns the doubled first-char bonus.
        let (p, cs) = prepare_pattern("fbb");
        let (m, _) = fuzzy_match_v2(cs, &Text::new("foo bar baz"), &p, true).unwrap();
        assert_eq!((m.start, m.end), (0, 9));
        assert_eq!(
            m.score,
            (SCORE_MATCH * 3
                + BONUS_BOUNDARY_WHITE * BONUS_FIRST_CHAR_MULTIPLIER
                + BONUS_BOUNDARY_WHITE * 2
                + 2 * SCORE_GAP_START
                + 4 * SCORE_GAP_EXTENSION) as i32
        );
    }

    #[test]
    fn consecutive_chunks_beat_scattered_ones() {
        // fzf's documented anomaly fix: "foobar" must outrank "foo-bar" on
        // "foob", because every character sits in one consecutive chunk.
        assert!(score("foobar", "foob").unwrap() > score("foo-bar", "foob").unwrap());
        // …and a boundary match beats a mid-word one on the same length.
        assert!(score("foo/bar", "bar").unwrap() > score("foobar", "bar").unwrap());
    }

    #[test]
    fn smart_case_and_non_matches() {
        assert!(score("Foo Bar", "foo").is_some());
        assert!(score("foo bar", "Foo").is_none()); // uppercase = case sensitive
        assert!(score("foo", "bar").is_none());
        assert!(score("ab", "abc").is_none()); // pattern longer than the line
    }

    #[test]
    fn positions_cover_every_pattern_char() {
        assert_eq!(positions("fooBarbaz", "oBz"), Some(vec![2, 3, 8]));
        assert_eq!(positions("/usr/bin/env", "usbe"), Some(vec![1, 2, 5, 9]));
    }

    #[test]
    fn exact_prefers_the_boundary_occurrence() {
        let (p, cs) = prepare_pattern("bar");
        let (m, _) = exact_match_naive(cs, &Text::new("foobar bar"), &p, false).unwrap();
        // The stand-alone "bar" wins over the one glued to "foo".
        assert_eq!((m.start, m.end), (7, 10));
        assert!(exact_match_naive(cs, &Text::new("br"), &p, false).is_none());
    }

    #[test]
    fn non_ascii_lines_still_match() {
        // The byte prefilter can't run here; the rune path must still work.
        // No `z` anywhere, so no fuzzy alignment exists.
        assert!(score("caffè latte", "cafez").is_none());
        assert!(score("caffè latte", "cffl").is_some());
        assert_eq!(positions("αβγ delta", "dl"), Some(vec![4, 6]));
    }
}
