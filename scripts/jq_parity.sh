#!/usr/bin/env bash
# jq_parity.sh — differential harness: arb's query engine vs the reference tools.
#
# arb's README/SPEC claim the query engine is a `jq`/`xpath`/`css`/`yq` SUPERSET.
# A superset claim is a contract: every construct the docs claim must produce the
# same answer as the reference tool on the same input. This script is how that
# contract is checked, in the shape of ../tclrs/scripts/fuzz_parity.sh — run one
# corpus of probes through BOTH engines and byte-diff stdout.
#
#   bash scripts/jq_parity.sh          # run every probe, print divergences
#   bash scripts/jq_parity.sh -q       # summary only
#
# References
#   jq       — /opt/homebrew/bin/jq, invoked as `jq -rc`. That is the invocation
#              that matches arb's output MODEL: arb's query engine is a line
#              stream, so a string renders raw (jq -r) and a compound value
#              renders compact on one line (jq -c). Comparing against bare `jq`
#              would diff on pretty-printing alone and hide real gaps.
#   xmllint  — /usr/bin/xmllint --html --xpath. Its `--xpath` prints attribute
#              NODES (` href="/x"`), whereas arb emits attribute VALUES (`/x`).
#              For `@attr` probes the reference output is normalized to the value
#              so the comparison is about node SELECTION, not xmllint's node
#              serialization. That normalization is applied to the reference only,
#              is printed in the report, and never hides a selection difference.
#   yq       — mikefarah/yq v4, invoked as `yq -o=json -I=0`. That is the
#              invocation that compares the QUERY rather than the serializer:
#              arb's YAML source emits one compact JSON line per document. A
#              reference result that is a bare JSON STRING is unwrapped, because
#              arb renders a top-level string RAW the way `jq -r` does; the
#              normalization is applied to the reference only, is printed in the
#              report, and cannot mask a selection difference. When yq is absent
#              its probes SKIP and the run says so, never passing silently.
#
# The contract has TWO halves and this harness probes both. SPEC §8 lists the
# supported jq/xpath subset, and then says anything OUTSIDE it "is a hard error
# (`jq: …` / `xpath: …`) … never silently reinterpreted". So:
#   jq_probe / xp_probe  — an IN-subset construct must byte-match the reference.
#   css_probe            — the css leg, against the xmllint XPath that selects
#                          the same elements (there is no css tool here; the
#                          translation is stated per probe and kept trivial).
#   err_probe            — an OUT-of-subset construct must EXIT NON-ZERO. Passing
#                          one silently is the worse failure of the two: a wrong
#                          answer that looks like an answer. (The earlier
#                          `select(.status == "ok")` bug was exactly this — the
#                          quotes were dropped in reconstruction and the filter
#                          matched nothing instead of erroring.)
#   type_probe           — an IN-subset construct on the wrong TYPE. Both engines
#                          must refuse, and the probe CHECKS that jq refuses, so
#                          a refusal arb invented alone is never scored as parity.
#
# Exit status is the number of diverging probes (0 = parity). Divergences are
# only ever reported, never suppressed: there is no allowlist in this script.
#
# Recorded measurement, this corpus, both binaries built from the same tree:
#   cec1d985a2 (before the parity work)   41 pass / 19 diverged / 1 skipped
#   c952bfce57 (after that wave)          59 pass /  1 diverged / 1 skipped
#   879e61a823 (the previous corpus, before)  128 pass / 26 diverged / 1 skipped
#   77d4244243 (the previous corpus, after)   153 pass /  1 diverged / 1 skipped
#   aac6d4eefa (that corpus, before)          158 pass / 15 diverged / 1 skipped
#   5ccdf9de36 (that corpus, after)           172 pass /  1 diverged / 1 skipped
#   3e19965cd4 (the previous corpus)          173 pass / 11 diverged / 1 skipped
#   3e19965cd4 (the previous corpus, before) 250 pass / 94 diverged / 1 skipped
#   81c0d07485 (the previous corpus, after)  343 pass /  1 diverged / 1 skipped
#   81c0d07485 (THIS corpus, before)         544 pass / 20 diverged / 1 skipped
#   HEAD       (THIS corpus, after)          559 pass /  5 diverged / 1 skipped
#   HEAD~      (THE JQ ENGINE corpus, before) 503 pass / 61 diverged / 1 skipped
#   HEAD       (THE JQ ENGINE corpus, after)  672 pass /  4 diverged / 0 skipped
#   5998c8e3ae (round 3, before)              672 pass /  4 diverged / 0 skipped
#   4152602d51 (`sel { #main }` fixed)        673 pass /  3 diverged / 0 skipped
#   e85497e008 (`keys` given back to jq)      674 pass /  2 diverged / 0 skipped
#   9075ed5f57 (the YAML literal fixed)       676 pass /  0 diverged / 0 skipped
#   HEAD       (THIS corpus, with the three
#               containment probes below)     676 pass / 10 diverged / 0 skipped
#
# ── the jq-engine wave ──────────────────────────────────────────────────────
# The 99 `err_probe`s are gone, and that is the measurement, not a change to it.
# Each of them asserted that arb REFUSES a jq construct — true only because a
# `Vec<QueryOp>` is a stage list and jq is a language of generators, so `.a, .b`
# and `reduce` could not be expressed at all. arb now runs a real jq engine, so
# every one of those 99 moved to a STRICTER kind on the same input: `jq_probe`
# (stdout must equal the reference byte for byte) for the 56 that answer,
# `type_probe` (both engines must refuse, and jq's refusal is CHECKED) for the 40
# that raise, and `ext_probe`/`superset_probe` for the two with no oracle.
#
# "arb exits non-zero" is satisfied by any error at all, including the wrong one.
# "arb's stdout equals jq's" is not. Every converted line asserts more than the
# line it replaced.
#
# Two probe kinds are new:
#
#   superset_probe — the containment the word SUPERSET actually names: every
#                    `name/arity` in jq's own `builtins` must exist in arb's. It
#                    is the only probe here that tests the claim as a whole
#                    rather than one construct at a time, and it found 44 missing
#                    builtins on its first run (`JOIN`, `bsearch`, `skip`,
#                    `toboolean`, `trimstr`, `format`, and the libm surface from
#                    `acosh` to `yn`), all of which are now implemented.
#   ext_probe      — an arb builtin jq 1.8 does NOT have (`leaf_paths`, dropped
#                    upstream after 1.7). A superset may define more; the probe
#                    CHECKS that this jq really lacks the name, so it can never
#                    quietly pin arb against a live reference.
#
# The four divergences this round left behind are ALL CLOSED now; see the
# round-3 note at the end of this header for what each one turned out to be.
#
# ── round 2 ─────────────────────────────────────────────────────────────────
# Same move as round 1, applied to what round 1 still could not see. The corpus
# went 344 -> 564 by diffing SPEC §8's CLAIMED subset against the probe list
# rather than by inventing new cases, and the 19 it exposed on the same binary
# that scored 343/344 were again mostly one root cause.
#
# `.` is the FIRST construct SPEC §8 names, and every probe of it fed an object,
# a number, a boolean or a null — never a STRING. For those four types arb's raw
# line and jq's rendering coincide exactly, so identity emitting NO OPS at all
# (`src/jq.rs`, `"." => Ok(())`) looked correct for the whole life of the corpus.
# It is not correct for a string: `jq -r` strips the quotes, so a line reading
# `"hello"` must print `hello`, and arb printed `"hello"`. arb also disagreed with
# ITSELF — `.[1:3]` on that same input already rendered `el` raw, because a slice
# RENDERS while identity passed the line through. `select(…)` and `values` re-emit
# the input line for the same reason, so all three carried it. See
# `QueryOp::JqRawString`, appended once at the END of a jq pipeline and only when
# the pipeline ends by emitting its input verbatim: mid-pipeline it would hand the
# next stage a non-JSON line and turn `"abc" | keys` from a hard error into a
# silent answer, and after an already-rendered stage it would unquote a second
# time and eat quotes that are DATA. Both are pinned as probes.
#
# The remaining 5 were recorded, not hidden, and are closed as of round 3 —
# `sel { #main }` needed the comment rule scoped to braces that hold COMMANDS,
# not a raw source span, and `keys` needed the native verb renamed off jq's
# spelling. The 3 whitespace probes are the measured COST of the passthrough
# that makes the string case above worth fixing rather than papering over:
# compacting a container would re-sort `serde_json`'s BTreeMap keys and reprint
# jq's preserved `1.50` as `1.5`, two deeper divergences traded for one.
#
# Round 2 also added a FIFTH probe kind. `text_probe` covers SPEC §8's non-JSON
# line carve-out — a path yields `null`, an iterate/slice passes through, an
# expression sees jq's string — which is the half of the value model that makes
# arb a line stream instead of a jq clone, was stated in three clauses, and was
# scored by NOTHING. jq refuses such a line outright, so there is no oracle; the
# expected values are transcribed from the SPEC prose and the probe asserts that
# jq really does refuse, so it can never quietly pin arb against a live reference.
#
# The corpus nearly doubled (184 -> 344 probes) and most of the 94 it exposed
# were one root cause with many faces: every `select(…)`/`map(…)` body and every bare
# arithmetic stage ran on `crate::expr`, arb's f64 evaluator, where a value can
# only be a double. jq's model is a JSON value, so the whole type lattice was
# missing: a compare rendered `1`/`0` instead of `true`/`false`, `select`'s falsy
# set was `0`/NaN instead of `false`/`null`, `==` was numeric instead of
# type-strict, `"x" + "y"` was NaN, and every TYPE ERROR answered `null` (or the
# raw line, or the line's character count) with exit 0 where jq raises. See
# `src/jqval.rs`, which replaces that path with a jq-value evaluator, and
# `QueryResult::Error`, the per-line refusal channel several comments in
# `src/query.rs` used to record as missing.
#
# The rest were two separate defects the widened corpus reached for the first
# time. `sel { div.card h2 }` — the css spelling SPEC §8 and the README both
# print — did not compile at all: a braced argument lexes to a command BLOCK,
# whose text `sel` dropped, so the DOCUMENTED form failed with "expected a CSS
# selector" while only the undocumented `sel div.card h2` worked. And `//*`
# answered with a node set that is not xmllint's: arb parses with html5ever,
# which synthesizes the `<head>` an HTML fragment omits, so the wildcard returned
# an element libxml2's set does not contain. It is not in the documented subset,
# so it refuses now.
#
# The 11 the previous corpus reported were 5 `%`-on-a-fraction (jq truncates both
# operands to integers first; arb took the f64 remainder — the JQ CONTEXT now
# follows jq, while arb's own `map x % 3` keeps SPEC §6's f64 rule, the same
# context gating `keys`/`flatten`/`to_entries` already used), 5 arithmetic
# against a whole object (now refused), and the bare `keys` spelling collision,
# which is the ONE that remains and is expected to.
#
# The 14 this corpus exposed and this tree closed were one blind spot: every
# numeric probe used a small value, so the corpus only ever exercised the single
# decade band where any number formatter agrees with jq's. `1e308 * 1` printed
# 309 digits, `1e18 / 3` printed `333333333333333312` against jq's
# `333333333333333300`, and `map(…)` rebuilt its array through a SECOND
# formatter, so `1e-06` came back out as `1e-6` while the scalar beside it was
# right. See `fmt_num` and `jq_array_json` in `src/query.rs`.
#
# The 25 closed before that were: every `map(…)` probe (jq returns `[…]`; arb dropped the
# rewrap and also merged two input lines into one flat stream), `keys` and
# `flatten` reached through the jq front-end, the ordered STRING compares
# (`select(.s < "abd")` silently matched NOTHING rather than erroring),
# `[@attr="v"]` with double quotes (as legal as `'v'` in XPath, but the lexer
# split the token at the quote), and the four jq FORMAT STRINGS
# `@base64`/`@csv`/`@tsv`/`@json`, which the body dispatcher handed to the xpath
# front-end as attribute steps — so they selected an attribute nobody has and
# exited ZERO with empty output, a jq construct answering "nothing" instead of
# refusing.
#
# The one remaining divergence is the bare `keys` SPELLING COLLISION and is
# expected to stay:
# `keys` is simultaneously a native verb (line-per-key — `stdlib/json.arb` pipes
# it into `tally`, and its own in-language test pins that) and a jq builtin (one
# sorted array). In a body a bare alphanumeric word is a NATIVE verb, so the probe
# below reaches the native one. The jq-context half IS resolved — `. | keys`
# routes through the jq literal front-end and now answers as jq — but the bare
# spelling cannot without breaking a shipped preset, so the probe stays red rather
# than being reworded into a pass. SPEC §8 documents it.
#
# ── round 3: the other three legs ───────────────────────────────────────────
# The four divergences round 2 left are closed, so the jq leg is at ZERO. Each
# turned out to be smaller than its note claimed:
#   * `sel { #main }` — scope the comment rule to braces that hold COMMANDS.
#   * `keys` — the native verb table is matched BEFORE the jq fall-through, so a
#     native verb spelled like a jq builtin SHADOWS it. Renamed to `names`.
#   * the YAML `1.50` literal, on `.` and `.ratio` — serde has nowhere to put a
#     number's source text, so the YAML reader composes from the parser's EVENT
#     stream now and keeps the literal the way the JSON reader always did.
#
# That left a harder question. `superset_probe` measures CONTAINMENT — every name
# the reference defines must exist in arb — and it existed for the jq leg only.
# The README claims a "jq/xpath/css/yq superset", so three legs had none, and the
# behavioural corpora could not stand in for one: every xp/css/yq probe here was
# written from a construct already known to work, and a corpus of things that
# pass measures nothing about what is missing.
#
# `xpath_superset_probe` and `yq_superset_probe` below fill that in, and the
# answer is that TWO OF THE FOUR LEGS ARE NOT SUPERSETS, by a wide margin:
#
#   xpath  46 of the 48 enumerated XPath 1.0 constructs are missing — all 13
#          axes in explicit syntax, all 27 core functions, `*`, `@*`, `..`,
#          `node()`/`text()`/`comment()`, positional predicates, `!=`. What arb
#          has is `//`, `/`, `@`, `[@a]`, `[@a='v']` and `[contains(@a,'v')]`:
#          an XPath-shaped syntax over a CSS engine, not an XPath engine.
#   yq     61 yq operators are missing, including every one that reads YAML NODE
#          METADATA — `anchor`, `alias`, `tag`, `style`, `kind`, `line`,
#          `column`, the three comment accessors, `key`, `parent`,
#          `documentIndex`, `splitDoc` — plus the whole encode/decode and
#          file/env families. Everything that DOES pass is a name arb already had
#          from jq. Metadata is the reason yq exists over jq, and arb's value
#          model is jq's, which has no slot for a comment or an anchor name.
#
# Eight `xp_probe`s were added with them, for the constructs the enumeration
# turned up, and two of those are worse than a refusal: `or` and a chained
# predicate answer with exit 0 and an EMPTY selection where XPath selects nodes,
# and a ROOTED path (`/li/text()`) answers with a non-empty node set where XPath
# selects nothing. SPEC §8 says anything outside the subset is "a hard error …
# never silently reinterpreted"; for those it is not, and a wrong answer that
# looks like an answer is the failure this harness exists to catch.
#
# None of the ten is allowlisted. They are the measurement.

set -u
cd "$(dirname "$0")/.." || exit 2

ARB=./target/debug/arb
# The reference's merge-key mode. `<<` has TWO readings and yq ships both: its
# default lets a merged key override an explicit one, and
# `--yaml-fix-merge-anchor-to-spec` follows the YAML spec instead. yq's own
# warning calls the default "isn't to the yaml spec" and says the flag will
# become the default.
#
# arb implements the SPEC rule (`src/yaml.rs` documents it, SPEC §8 states it), so
# the reference is asked in that mode. Asking it in the other one would report a
# divergence that says arb is right, which is not what a divergence should mean.
# The two modes differ ONLY on merge keys — every other probe is unaffected.
YQ_MERGE=--yaml-fix-merge-anchor-to-spec
QUIET=0
[ "${1:-}" = "-q" ] && QUIET=1

# The pinned references. Every recorded number below was measured against them.
# `command -v jq` alone is not enough: this machine carries TWO jq binaries
# (/opt/homebrew/bin/jq is 1.8.2, /usr/bin/jq is 1.7.1-apple) and they do not
# agree, so whichever one PATH resolved silently moved the score.
REF_JQ=jq-1.8.2
JQ=${JQ:-jq}
# The floor the probe count must clear. `xp_probe`/`css_probe` SKIP silently when
# xmllint is missing, so without this a machine with no xmllint drops 46 probes
# and still reports a clean run. Raise it when the corpus grows; never lower it.
MIN_PROBES=857

[ -x "$ARB" ] || { echo "jq_parity: $ARB not built — run 'cargo build'" >&2; exit 2; }
command -v "$JQ" >/dev/null || {
    echo "jq_parity: no '$JQ' on PATH — every jq probe would compare arb against" >&2
    echo "           a missing tool. Install $REF_JQ or set JQ=." >&2
    exit 3
}
have_jq=$("$JQ" --version 2>&1)
[ "$have_jq" = "$REF_JQ" ] || {
    echo "jq_parity: reference is pinned to $REF_JQ but '$JQ' is $have_jq —" >&2
    echo "           the recorded numbers are not reproducible against it." >&2
    exit 3
}
command -v xmllint >/dev/null || {
    echo "jq_parity: no xmllint — the 46 xpath/css probes would SKIP and the run" >&2
    echo "           would still report clean. Install libxml2." >&2
    exit 3
}

pass=0; fail=0; skip=0
fails=()

# jq_probe INPUT FILTER — feed INPUT to both engines with the same filter.
jq_probe() {
    local input="$1" filter="$2" a b
    a=$(printf '%s\n' "$input" | "$ARB" -e "out { in.json; $filter }" 2>&1)
    b=$(printf '%s\n' "$input" | "$JQ" -rc "$filter" 2>&1)
    if [ "$a" = "$b" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   jq   %-32s %s\n' "$filter" "$input"
    else
        fail=$((fail + 1))
        fails+=("jq   $filter <= $input"$'\n'"       arb: $(printf %s "$a" | tr '\n' '|')"$'\n'"       jq : $(printf %s "$b" | tr '\n' '|')")
        [ "$QUIET" = 1 ] || printf 'DIFF jq   %-32s %s\n       arb: %s\n       jq : %s\n' \
            "$filter" "$input" "$(printf %s "$a" | tr '\n' '|')" "$(printf %s "$b" | tr '\n' '|')"
    fi
}

# err_probe FILTER — a construct arb does not implement, which must therefore
# exit NON-ZERO rather than guess.
#
# This kind has NO CALL SITES any more, and that is the point of the wave that
# removed them: every construct it used to guard is now implemented, so each of
# its 99 probes moved to a STRICTER kind — `jq_probe` (byte-match the reference)
# for the 56 that answer, `type_probe` (both engines must refuse, and jq's
# refusal is checked) for the 40 that raise on this input, plus `ext_probe` and
# `superset_probe` for the two that have no oracle. Asserting "arb refuses this"
# would now assert something FALSE, and a probe that encodes a retired contract
# measures nothing.
#
# It is kept, not deleted: the next construct arb declines to implement needs it,
# and its shape is the record of how the earlier gaps were held.
err_probe() {
    local filter="$1" out rc
    out=$(printf '%s\n' '{"a":1,"b":2,"foo":null}' | "$ARB" -e "out { in.json; $filter }" 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   err  %-32s (refused)\n' "$filter"
    else
        fail=$((fail + 1))
        fails+=("err  $filter — OUT OF SUBSET but accepted silently"$'\n'"       arb: $(printf %s "$out" | tr '\n' '|')")
        [ "$QUIET" = 1 ] || printf 'DIFF err  %-32s accepted silently: %s\n' \
            "$filter" "$(printf %s "$out" | tr '\n' '|')"
    fi
}

# type_probe INPUT FILTER — the DATA half of the hard-error contract.
#
# `err_probe` covers constructs arb never implemented. This covers the other
# refusal: a construct that IS in the subset applied to a value of the wrong
# TYPE (`null | .[]`, `true | length`, `{"a":1} | . + 3`). jq raises on all of
# them, and SPEC §8 forbids answering where jq refuses — which is exactly what
# arb used to do, one `null` / raw line / character count at a time.
#
# Unlike `err_probe` there IS a reference here and it is CHECKED: a probe whose
# jq run exits zero is reported as a divergence rather than as a pass, so a
# refusal arb invented on its own can never be scored as parity. Only the exit
# statuses are compared — arb anchors its message as `arb: jq: …` while jq prints
# `jq: error (at <stdin>:N): …`, so the text differs by construction.
type_probe() {
    local input="$1" filter="$2" arc jrc out
    out=$(printf '%s\n' "$input" | "$ARB" -e "out { in.json; $filter }" 2>&1); arc=$?
    printf '%s\n' "$input" | "$JQ" -rc "$filter" >/dev/null 2>&1; jrc=$?
    if [ "$jrc" -eq 0 ]; then
        fail=$((fail + 1))
        fails+=("type $filter <= $input — MISCLASSIFIED: jq ACCEPTS this, so it is not a type error")
        [ "$QUIET" = 1 ] || printf 'DIFF type %-30s %s (jq accepts it — probe is wrong)\n' "$filter" "$input"
    elif [ "$arc" -ne 0 ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   type %-30s %s (both refused)\n' "$filter" "$input"
    else
        fail=$((fail + 1))
        fails+=("type $filter <= $input — jq REFUSES, arb answered"$'\n'"       arb: $(printf %s "$out" | tr '\n' '|')")
        [ "$QUIET" = 1 ] || printf 'DIFF type %-30s %s\n       arb answered: %s\n' \
            "$filter" "$input" "$(printf %s "$out" | tr '\n' '|')"
    fi
}

# text_probe INPUT FILTER EXPECTED — the NON-JSON line, where there is no oracle.
#
# SPEC §8 carves this case out in prose: arb's stream is TEXT and `jq` has no
# reading of a non-JSON line at all (it refuses the whole input), so there is
# nothing to byte-diff against. That makes it the one region of the contract the
# other four probe kinds structurally cannot reach — and it was scored by NOTHING,
# which is the same "too small to fail" state the css leg was in before round 1.
#
# The three rules SPEC states are the contract instead: a path yields `null`, an
# iterate/slice passes the line through, and an EXPRESSION sees the line as jq's
# string (`. * 2` over `abc` is `abcabc`). EXPECTED is transcribed from that prose
# and NEVER from a run of arb — a probe recording what arb happens to do would
# assert nothing and would ratify a regression as the new truth.
#
# The probe also CHECKS that jq refuses the input. That keeps it honest in the
# other direction: it may only claim to be oracle-free where the oracle really is
# absent, so a line jq CAN read is reported as a misclassification rather than
# being quietly pinned to arb's answer.
text_probe() {
    local input="$1" filter="$2" want="$3" a jrc
    printf '%s\n' "$input" | "$JQ" -rc "$filter" >/dev/null 2>&1; jrc=$?
    if [ "$jrc" -eq 0 ]; then
        fail=$((fail + 1))
        fails+=("text $filter <= $input — MISCLASSIFIED: jq READS this line, so it is not oracle-free")
        [ "$QUIET" = 1 ] || printf 'DIFF text %-26s %s (jq reads it — probe is wrong)\n' "$filter" "$input"
        return
    fi
    a=$(printf '%s\n' "$input" | "$ARB" -e "out { in.json; $filter }" 2>&1)
    if [ "$a" = "$want" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   text %-26s %-12s => %s\n' "$filter" "$input" "$(printf %s "$a" | tr '\n' '|')"
    else
        fail=$((fail + 1))
        fails+=("text $filter <= $input — SPEC §8 says \`$want\`"$'\n'"       arb: $(printf %s "$a" | tr '\n' '|')")
        [ "$QUIET" = 1 ] || printf 'DIFF text %-26s %s\n       SPEC: %s\n       arb : %s\n' \
            "$filter" "$input" "$(printf %s "$want" | tr '\n' '|')" "$(printf %s "$a" | tr '\n' '|')"
    fi
}

# ext_probe INPUT FILTER EXPECTED — an arb builtin jq 1.8 does NOT have.
#
# A superset is allowed to define MORE than its reference, and arb keeps two
# builtins jq had through 1.7 and dropped (`leaf_paths`, `toarray`). There is no
# oracle for those, so — exactly as `text_probe` does for the non-JSON line —
# EXPECTED is transcribed from jq's own historical definition and the probe
# CHECKS that this jq really does not define the name. That keeps it honest in
# both directions: it may only claim to be oracle-free where the oracle really is
# absent, and it can never quietly pin arb against a live reference.
ext_probe() {
    local input="$1" filter="$2" want="$3" a jrc
    printf '%s\n' "$input" | "$JQ" -rc "$filter" >/dev/null 2>&1; jrc=$?
    if [ "$jrc" -eq 0 ]; then
        fail=$((fail + 1))
        fails+=("ext  $filter — MISCLASSIFIED: this jq DEFINES it, so it is not an extension")
        [ "$QUIET" = 1 ] || printf 'DIFF ext  %-26s (jq defines it — probe is wrong)\n' "$filter"
        return
    fi
    a=$(printf '%s\n' "$input" | "$ARB" -e "out { in.json; $filter }" 2>&1)
    if [ "$a" = "$want" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   ext  %-26s => %s\n' "$filter" "$(printf %s "$a" | tr '\n' '|')"
    else
        fail=$((fail + 1))
        fails+=("ext  $filter — jq 1.7's definition gives \`$want\`"$'\n'"       arb: $(printf %s "$a" | tr '\n' '|')")
        [ "$QUIET" = 1 ] || printf 'DIFF ext  %-26s\n       want: %s\n       arb : %s\n' \
            "$filter" "$(printf %s "$want" | tr '\n' '|')" "$(printf %s "$a" | tr '\n' '|')"
    fi
}

# superset_probe — the containment check the word SUPERSET actually names.
#
# `builtins` is the one construct whose ANSWER cannot match jq's: the two engines
# define different sets, so a byte-diff would report a divergence that says
# nothing. What the claim requires is not equality but CONTAINMENT — every
# `name/arity` jq defines must exist in arb. That is a strictly stronger check
# than the byte-diff would have been for every other name in the list, and it is
# the only probe here that tests the superset claim as a whole rather than one
# construct at a time.
superset_probe() {
    local missing
    # arb is a LINE stream, so it needs a line to run the program over; feeding
    # it `null` is the closest thing to jq's `-n`.
    # Both sides go through `LC_ALL=C sort` AND `comm` itself runs under that
    # locale: `comm` compares with the locale's collation, and under a UTF-8
    # locale `_` and `/` sort differently from byte order — enough to report
    # `ascii_downcase/0` as missing from a list that contains it.
    missing=$(LC_ALL=C comm -23 \
        <("$JQ" -rn 'builtins | .[]' 2>/dev/null | LC_ALL=C sort) \
        <(printf 'null\n' | "$ARB" -e 'out { in.json; builtins | .[] }' 2>/dev/null | LC_ALL=C sort))
    if [ -z "$missing" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   sup  every jq builtin exists in arb\n'
    else
        fail=$((fail + 1))
        fails+=("sup  jq builtins MISSING from arb: $(printf %s "$missing" | tr '\n' ' ')")
        [ "$QUIET" = 1 ] || printf 'DIFF sup  jq builtins missing from arb: %s\n' \
            "$(printf %s "$missing" | tr '\n' ' ')"
    fi
}

# ── containment for the OTHER three legs ────────────────────────────────────
#
# `superset_probe` above is the only check that tests the word SUPERSET as a
# whole rather than one construct at a time, and until now it existed for the jq
# leg ONLY. The README claims a "jq/xpath/css/yq superset", so three of the four
# legs had no containment check at all — and the behavioural corpora below could
# not stand in for one, because each was written from constructs already known to
# work. A corpus of things that pass is not a containment measurement.
#
# The three probes below fix that, in `superset_probe`'s exact shape: enumerate
# the REFERENCE's surface, machine-check that the reference really defines each
# name (so arb is never charged for a name the reference does not have either),
# and report the set arb is MISSING.
#
# What they measure is NAMES, which is what containment means and what the jq
# probe measures. A name arb has with DIFFERENT semantics counts as present here
# and is caught by the behavioural probes instead — `explode` is both engines'
# spelling for two unrelated operations, and only a `yq_probe` can see that.

# Every yq name this script knows to ask about. Filtered through yq itself below,
# so a name yq does not define is dropped rather than charged to arb. Drawn from
# yq's operator index (mikefarah.gitbook.io/yq/operators) plus its `@encoder`s.
# Written as whole EXPRESSIONS, not bare names, and with no embedded spaces:
# the list is word-split, and asking about `select` rather than `select(.)` would
# charge arb for an ARITY spelling it does not accept rather than for a name it
# does not have. A wrong number in this direction is as useless as one in the
# other.
YQ_NAMES='anchor alias explode(.) tag style kind line column
head_comment line_comment foot_comment headComment lineComment footComment
key is_key parent path documentIndex di splitDoc split_doc comments
to_json from_json to_yaml from_yaml to_xml from_xml to_props from_props
to_csv from_csv to_tsv from_tsv
env(HOME) strenv(HOME) envsubst load("f") load_str("f") load_props("f")
load_xml("f") filename fileIndex
format_datetime("x") from_unix to_unix tz("UTC") with_dtf("x";.) now
pick(["a"]) omit(["a"]) with(.;.) sort_keys(.) sortKeys(.) shuffle pivot
ireduce(0;.) eval(".") ref
downcase upcase to_string to_number
select(.) map(.) map_values(.) has("a") length keys to_entries from_entries
with_entries(.) sort sort_by(.) reverse unique unique_by(.) group_by(.)
min max flatten contains(.) any all not split("a") join(",") sub("a";"b")
test("a") capture("(a)") tonumber trim error("x") type'

# XPath 1.0's own surface, from the REC (w3.org/TR/1999/REC-xpath-19991116):
# all 13 axes in §2.2, all 27 core functions in §4, and the node tests in §2.3.
# Each is written as a whole expression so xmllint can be asked whether IT
# accepts the expression before arb is charged for refusing it.
XP_AXES='child::p descendant::p parent::* ancestor::div following-sibling::p
preceding-sibling::p following::p preceding::p attribute::id namespace::*
self::p descendant-or-self::p ancestor-or-self::div'
XP_FUNCS='count(//p) id("main") local-name(//p) namespace-uri(//p) name(//p)
string(//p) concat("a","b") starts-with("ab","a") contains("ab","b")
substring-before("a-b","-") substring-after("a-b","-") substring("hello",2,3)
string-length("abcd") normalize-space("a") translate("abc","abc","xyz")
boolean(//p) not(false()) true() false() lang("en")
number("42") sum(//p) floor(1.7) ceiling(1.2) round(1.5)
//p[last()] //p[position()=2]'
XP_TESTS='//node() //text() //comment() //processing-instruction() //* //@id //p/.. //p[@id!="x"]'

# yq_superset_probe — every yq NAME must exist in arb.
#
# Existence oracles, both machine-checked rather than assumed:
#   yq  defines NAME unless `yq -n NAME` says `invalid input text` (its lexer
#       rejecting an unknown token). An ARITY complaint means the name exists.
#   arb defines NAME unless it answers `unknown verb` or `is not supported`.
#       A TYPE error means the name exists and refused this input, which is a
#       different thing and is not counted as missing.
yq_superset_probe() {
    local missing='' n out
    command -v yq >/dev/null || { skip=$((skip + 1)); return; }
    for n in $YQ_NAMES; do
        case "$(yq -n "$n" 2>&1)" in *'invalid input text'*) continue ;; esac
        out=$(printf 'null\n' | "$ARB" -e "out { in.yaml; $n }" 2>&1)
        case "$out" in
            *'unknown verb'* | *'is not supported'* | *'is not a valid format'*)
                missing="$missing $n" ;;
        esac
    done
    report_containment yqs "yq operator" "$missing"
}

# xpath_superset_probe — every XPath 1.0 axis, core function and node test must
# exist in arb. `xmllint --xpath` is asked first, so an expression libxml2 itself
# rejects is dropped rather than charged.
xpath_superset_probe() {
    local missing='' e out
    [ -x /usr/bin/xmllint ] || { skip=$((skip + 1)); return; }
    while IFS= read -r e; do
        [ -n "$e" ] || continue
        /usr/bin/xmllint --html --xpath "$e" "$XPF" >/dev/null 2>&1
        # 0 = a node set, 10 = a valid expression selecting nothing. Anything
        # else is libxml2 refusing the expression, so arb is not asked.
        case $? in 0 | 10) ;; *) continue ;; esac
        out=$("$ARB" -e "out { in.html; $e }" <"$XPF" 2>&1)
        case "$out" in *'xpath:'* | *'unknown verb'*) missing="$missing ${e%% *}" ;; esac
    done <<XPEOF
$(printf '%s\n' $XP_AXES $XP_FUNCS $XP_TESTS)
XPEOF
    report_containment xps "xpath 1.0 construct" "$missing"
}

# report_containment KIND LABEL MISSING — the shared scoring half of the two
# probes above and of `superset_probe`'s report, so all three read the same way.
report_containment() {
    local kind="$1" label="$2" missing="$3"
    if [ -z "$missing" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   %-4s every %s exists in arb\n' "$kind" "$label"
    else
        local n
        n=$(printf '%s' "$missing" | wc -w | tr -d ' ')
        fail=$((fail + 1))
        fails+=("$kind  $n ${label}s MISSING from arb:$missing")
        [ "$QUIET" = 1 ] || printf 'DIFF %-4s %d %ss missing from arb:%s\n' \
            "$kind" "$n" "$label" "$missing"
    fi
}

# yq_probe FILE FILTER — the yq leg, against mikefarah/yq over the same YAML.
#
# `yq -o=json -I=0` is the invocation that compares the QUERY rather than the
# serializer: arb's YAML source emits one compact JSON line per document, and
# that is what this asks yq for too. One normalization is applied, to the
# REFERENCE only and printed here: a result that is a JSON STRING is unwrapped,
# because arb's stream renders a top-level string RAW the way `jq -r` does
# (`.name` is `widget`, not `"widget"`). It cannot mask a selection difference —
# it only removes the quotes around a value both engines already agree on.
#
# This corpus covers the jq-shaped filters yq can also spell. That is NOT the
# whole of yq, and the comment here used to claim otherwise — that "yq's
# expression language is SMALLER than jq's (no `keys_unsorted`, no `paths`, no
# `add`, no `sort_by`)". Two of those four are simply wrong (`yq -n '[3,1,2] |
# sort_by(.)'` runs, and so does `keys`), and the inference was backwards: yq
# being smaller on the jq OVERLAP says nothing about its own surface, which
# carries ~60 operators jq has no equivalent for — anchors, tags, styles,
# comments, document index, the encode/decode family, the file/env family.
# `yq_superset_probe` below is what measures those; these probes do not.
yq_probe() {
    local file="$1" filter="$2" a b
    command -v yq >/dev/null || { skip=$((skip + 1)); return; }
    a=$("$ARB" -e "out { in.yaml; $filter }" <"$file" 2>&1)
    b=$(yq $YQ_MERGE -o=json -I=0 "$filter" "$file" 2>/dev/null)
    # Unwrap a reference that is a bare JSON string.
    case "$b" in
        '"'*'"') b=$(printf '%s' "$b" | "$JQ" -r . 2>/dev/null || printf '%s' "$b") ;;
    esac
    if [ "$a" = "$b" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   yq   %-30s %s\n' "$filter" "$(printf %s "$a" | tr '\n' '|')"
    else
        fail=$((fail + 1))
        fails+=("yq   $filter"$'\n'"       arb: $(printf %s "$a" | tr '\n' '|')"$'\n'"       yq : $(printf %s "$b" | tr '\n' '|')")
        [ "$QUIET" = 1 ] || printf 'DIFF yq   %-30s\n       arb: %s\n       yq : %s\n' \
            "$filter" "$(printf %s "$a" | tr '\n' '|')" "$(printf %s "$b" | tr '\n' '|')"
    fi
}

# yq_rt_probe FILE LABEL — the ROUND TRIP, which is yq's defining behaviour.
#
# `yq '.' file.yaml` gives the file back essentially unchanged: comments in every
# position, anchor names, aliases, merge keys, quoting style, block scalars,
# flow-vs-block, tags, empty values and non-ASCII all survive. `arb -e 'out {
# in.yaml; out.yaml }'` must do the same.
#
# The comparison is against the SOURCE FILE rather than against yq's output, and
# that is deliberately STRICTER rather than looser: `yq '.'` is not idempotent on
# two of the shapes below — it re-folds a `>` block onto one line, and escapes a
# non-BMP character as an "\U0001F680" sequence — so requiring arb to match yq
# would require arb to reproduce yq's own infidelities. "The file comes back" is
# the property the claim names, and it implies matching yq everywhere yq does
# return the file.
#
# It is the strongest single check of the node model in this file. A metadata
# accessor that answers correctly proves one field is carried; a byte-identical
# round trip proves every field is carried AND put back in the right place, on a
# document that exercises all of them at once. Nothing is normalized on either
# side.
yq_rt_probe() {
    local file="$1" label="$2" a b
    command -v yq >/dev/null || { skip=$((skip + 1)); return; }
    a=$("$ARB" -e 'out { in.yaml; out.yaml }' <"$file" 2>&1)
    b=$(cat "$file")
    if [ "$a" = "$b" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   rt   %s\n' "$label"
    else
        fail=$((fail + 1))
        fails+=("rt   $label — round trip differs from the source"$'\n'"$(diff <(printf '%s\n' "$b") <(printf '%s\n' "$a") | sed 's/^/       /')")
        [ "$QUIET" = 1 ] || {
            printf 'DIFF rt   %s\n' "$label"
            diff <(printf '%s\n' "$b") <(printf '%s\n' "$a") | sed 's/^/       /'
        }
    fi
}

# yq_fmt_probe FILE FMT — an OUTPUT MODE, against `yq -o=FMT`.
#
# `out.yaml`/`out.props`/`out.json` are arb's spelling of yq's `-o=`, and each
# renders the whole stream rather than one value, so `yq_probe`'s per-filter
# comparison cannot reach them. Byte-diffed, nothing normalized.
yq_fmt_probe() {
    local file="$1" fmt="$2" a b
    command -v yq >/dev/null || { skip=$((skip + 1)); return; }
    a=$("$ARB" -e "out { in.yaml; out.$fmt }" <"$file" 2>&1)
    b=$(yq $YQ_MERGE -o="$fmt" '.' "$file" 2>/dev/null)
    if [ "$a" = "$b" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   fmt  %-6s %s\n' "$fmt" "$file"
    else
        fail=$((fail + 1))
        fails+=("fmt  -o=$fmt on $file"$'\n'"       arb: $(printf %s "$a" | tr '\n' '|')"$'\n'"       yq : $(printf %s "$b" | tr '\n' '|')")
        [ "$QUIET" = 1 ] || printf 'DIFF fmt  %-6s\n       arb: %s\n       yq : %s\n' \
            "$fmt" "$(printf %s "$a" | tr '\n' '|')" "$(printf %s "$b" | tr '\n' '|')"
    fi
}

# xp_probe FILE XPATH — compare arb's xpath front-end against xmllint.
# `@attr` probes normalize the reference from ` name="v"` to `v` (see header).

# Canonicalize element serialization: sort each start tag's attributes. arb parses
# with html5ever and xmllint with libxml2, and the two emit a multi-attribute tag's
# attributes in different ORDERS (`<a rel="nf" href="/z">` vs `<a href="/z"
# rel="nf">`). That is the serializer talking, not the selection — the same class
# of difference as the `@attr` node-vs-value normalization, and applied to BOTH
# sides so it can never mask one engine selecting a different node than the other.
sort_attrs() {
    perl -pe 's{<([A-Za-z][-\w]*)((?:\s+[-\w:]+(?:="[^"]*")?)+)(\s*/?)>}{
        "<$1" . join("", sort map { " $_" } ($2 =~ /([-\w:]+(?:="[^"]*")?)/g)) . "$3>"
    }ge'
}

xp_probe() {
    local file="$1" xp="$2" a b
    command -v xmllint >/dev/null || { skip=$((skip + 1)); return; }
    a=$("$ARB" -e "out { in.html; $xp }" <"$file" 2>&1 | sort_attrs)
    b=$(xmllint --html --xpath "$xp" "$file" 2>/dev/null | sort_attrs)
    # Normalize ONLY when the path SELECTS attributes (a trailing `/@name`, a
    # bare `@name` step, or either in the `@*` WILDCARD form). An `@` inside a
    # predicate (`//div[@class]//span`) still selects elements, so its output is
    # compared unmodified.
    #
    # The rule is unchanged; the `@*` spellings are added because the engine can
    # now select with them. It stays per-LINE, so the attribute VALUES and their
    # ORDER are still both compared and a selection difference cannot hide in it.
    case "$xp" in
        *'/@'[A-Za-z_]* | '@'[A-Za-z_]* | *'/@*' | '@*')
            b=$(printf '%s\n' "$b" | perl -ne 'print "$1\n" if /="(.*)"\s*$/') ;;
    esac
    if [ "$a" = "$b" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   xp   %s\n' "$xp"
    else
        fail=$((fail + 1))
        fails+=("xp   $xp"$'\n'"       arb    : $(printf %s "$a" | tr '\n' '|')"$'\n'"       xmllint: $(printf %s "$b" | tr '\n' '|')")
        [ "$QUIET" = 1 ] || printf 'DIFF xp   %s\n       arb    : %s\n       xmllint: %s\n' \
            "$xp" "$(printf %s "$a" | tr '\n' '|')" "$(printf %s "$b" | tr '\n' '|')"
    fi
}

# ── jq: identity, key paths, iterate, index, slice ──────────────────────────
jq_probe '{"a":1}'                       '.'
jq_probe '{"a":1}'                       '.a'
jq_probe '{"a":{"b":7}}'                 '.a.b'
jq_probe '{"a":{"b":{"c":"deep"}}}'      '.a.b.c'
jq_probe '{"foo":9}'                     '.["foo"]'
jq_probe '{"a b":1}'                     '.["a b"]'
jq_probe '[1,2,3]'                       '.[]'
jq_probe '{"items":[1,2]}'               '.items[]'
jq_probe '["a","b","c"]'                 '.[1]'
jq_probe '["a","b","c"]'                 '.[-1]'
jq_probe '{"foo":["x","y"]}'             '.foo[0]'
jq_probe '[10,20,30,40,50]'              '.[1:3]'
jq_probe '[10,20,30]'                    '.[:2]'
jq_probe '[10,20,30,40]'                 '.[2:]'
jq_probe '[10,20,30,40]'                 '.[-2:]'
jq_probe '{"foo":[1,2,3,4]}'             '.foo[1:3]'
jq_probe '"hello"'                       '.[1:3]'

# ── jq: pipes and multi-stage ───────────────────────────────────────────────
jq_probe '{"foo":{"bar":5}}'             '.foo | .bar'
jq_probe '[{"n":1},{"n":2},{"n":3}]'     '.[] | select(.n >= 2)'
jq_probe '[["ab","cde"]]'                '.[] | length'
jq_probe '{"u":{"name":"bob"}}'          '.u.name'

# ── jq: select / map ────────────────────────────────────────────────────────
jq_probe '{"amount":150}'                'select(.amount > 100)'
jq_probe '{"amount":50}'                 'select(.amount > 100)'
jq_probe '{"status":"ok"}'               'select(.status == "ok")'
jq_probe '{"status":"bad"}'              'select(.status == "ok")'
jq_probe '{"status":"ok"}'               'select(.status != "ok")'
# Every comparison operator, on numbers AND on strings. SPEC §8: "a compare may
# test strings as well as numbers". The ordered string forms used to fall through
# to the numeric evaluator, which reads a non-numeric field as NaN — and every NaN
# compare is false, so the filter silently dropped every row instead of erroring.
jq_probe '{"n":5}'                       'select(.n < 10)'
jq_probe '{"n":5}'                       'select(.n <= 5)'
jq_probe '{"n":5}'                       'select(.n >= 6)'
jq_probe '{"n":5}'                       'select(.n > 5)'
jq_probe '{"s":"abc"}'                   'select(.s < "abd")'
jq_probe '{"s":"abc"}'                   'select(.s > "abd")'
jq_probe '{"s":"b"}'                     'select(.s >= "b")'
jq_probe '{"s":"b"}'                     'select(.s <= "a")'
jq_probe '{"s":"b"}'                     'select(.s > "a")'
jq_probe '{"a":1,"b":2}'                 'select(.a == 1 and .b == 2)'
jq_probe '{"a":1,"b":2}'                 'select(.a == 9 or .b == 2)'
jq_probe '[{"s":"a"},{"s":"c"}]'         '.[] | select(.s < "b")'

# ── jq: map(…) — SPEC §8 lists it as supported and shows `map(.price)` ──────
# jq's `map(f)` IS `[.[] | f]`: the array rewrap and the per-input scope are both
# part of the builtin. These probe the shape, the element types, a nested filter,
# a nested reducer, and — the one a single-line corpus cannot see — that two input
# lines stay two arrays instead of merging into one flat stream.
jq_probe '[{"price":3},{"price":4}]'     'map(.price)'
jq_probe '[1,2,3]'                       'map(. * 2)'
jq_probe '[1,2,3]'                       'map(.)'
jq_probe '{"a":1,"b":2}'                 'map(.+1)'
jq_probe '[{"a":1},{"a":2}]'             'map(select(.a > 1))'
jq_probe '[[1,2],[3]]'                   'map(length)'
jq_probe '["a","bb"]'                    'map(length)'
jq_probe '[{"a":{"b":1}}]'               'map(.a)'
jq_probe '[[1,2],[3,4]]'                 'map(add)'
jq_probe '[]'                            'map(.x)'
jq_probe '[{"n":1},{"n":2}]'             'map(.n) | add'
jq_probe '[1,2,3]'                       'map(. * 2) | length'
# The iterate-WITHOUT-rewrap reading is jq's own `.[] | f` — it must stay flat.
jq_probe '[{"price":3},{"price":4}]'     '.[] | .price'

# ── jq: builtins the SPEC claims (keys/values/length/add/has/to_entries) ────
# `keys` reached through the jq LITERAL front-end (a leading `.` routes the whole
# command there) must be jq's one sorted array. The bare `keys` probe below is the
# native VERB of the same spelling and is expected to differ — SPEC §8 documents
# that collision, and this harness has no allowlist, so it is reported every run.
jq_probe '{"b":1,"a":2}'                 '. | keys'
jq_probe '[9,8]'                         '. | keys'
jq_probe '{"o":{"z":1,"y":2}}'           '.o | keys'
jq_probe '{"a":1,"b":2}'                 'keys'
jq_probe '{"a":1,"b":2}'                 'values'
jq_probe 'null'                          'values'
jq_probe '{"a":1}'                       'has("a")'
jq_probe '{"x":2}'                       'has("id")'
jq_probe '{"a":1,"b":2}'                 'to_entries'
jq_probe '[1,[2,[3,[4]]]]'               '. | flatten'
jq_probe '[[1],[2]]'                     '. | flatten'
jq_probe '[1,2,3]'                       'add'
jq_probe '["a","b"]'                     'add'
jq_probe '[1,2,3]'                       'length'
jq_probe '{"a":1,"b":2}'                 'length'
jq_probe '"hello"'                       'length'
jq_probe '-7'                            'length'
jq_probe 'null'                          'length'

# ── jq: a top-level JSON STRING through the PASSTHROUGH filters ─────────────
# `.` is the FIRST construct SPEC §8 names, and every probe of it above feeds an
# object, a number, a boolean or a null — never a string. That is the same blind
# spot shape as the numeric band: for those four types arb's line passes through
# and jq's rendering coincide exactly, so the probe agreed for a reason that had
# nothing to do with the string case.
#
# It does not coincide for a string. `jq -r` strips the quotes, and arb emitted
# the raw line, so a line reading `"hello"` printed `"hello"`. arb also disagreed
# with ITSELF: `.[1:3]` on the same input already rendered `el` raw, because a
# slice RENDERS its result while identity emitted no ops at all.
#
# `select(…)` and `values` re-emit the input line for the same reason, so all
# three spellings are probed, along with the escapes that prove it is a real
# unquote and not a quote-strip.
jq_probe '"hello"'                       '.'
jq_probe '""'                            '.'
jq_probe '"a b"'                         '.'
jq_probe '"1"'                           '.'
jq_probe '"null"'                        '.'
jq_probe '"true"'                        '.'
jq_probe '"a\"b"'                        '.'
jq_probe '"tab\there"'                   '.'
jq_probe '"é日本"'                        '.'
jq_probe '"hello"'                       'values'
jq_probe '"hello"'                       '. | values'
jq_probe '"hello"'                       'select(.)'
jq_probe '"hello"'                       'select(. == "hello")'
jq_probe '""'                            'values'
# The other four types must KEEP passing the source line through. jq reprints the
# literal of any number it never computed, so `1.50` stays `1.50` — re-rendering
# it through `fmt_num` would produce `1.5`, and rebuilding an object would come
# back key-SORTED out of `serde_json`'s BTreeMap where jq preserves input order.
# These pin that the fix stayed string-only.
jq_probe '1.50'                          '.'
jq_probe '{"b":1,"a":2}'                 '.'
jq_probe '[1,2]'                         '.'
jq_probe '1.50'                          'values'
jq_probe '{"b":1,"a":2}'                 'select(.)'
jq_probe '[1,2]'                         'select(.)'
# The COST of passing the line through, measured rather than assumed: where the
# source line carries interior whitespace, `jq -c` re-serializes it compact and
# arb emits it as written. These probes are RED and stay red until arb can render
# a container without losing what the passthrough currently preserves.
#
# It is a strictly harder problem than the string case above, which is why that
# one is fixed and this one is only recorded. Compacting means rebuilding the
# value, and rebuilding costs two things jq keeps: `serde_json::Map` is a
# `BTreeMap`, so `{"b":1,"a":2}` comes back key-SORTED where jq preserves input
# order, and every number is reprinted through `fmt_num`, so jq's preserved `1.50`
# becomes `1.5`. Both are pinned as PASSING probes just above. Trading one
# divergence for two deeper ones is not a fix, and the honest state is a red probe
# plus the narrowed SPEC §8 sentence, not a reworded probe that agrees with arb.
jq_probe '{ "a" : 1 }'                   '.'
jq_probe '[1,  2]'                       '.'
jq_probe '{ "a" : 1 }'                   'select(.a == 1)'
# A string whose CONTENT is quotes must be unquoted exactly ONCE. These reach the
# already-rendered paths (`.a`, `.[]`), which must NOT be unquoted a second time.
jq_probe '{"a":"\"q\""}'                 '.a'
jq_probe '["\"q\""]'                     '.[]'
jq_probe '{"a":"\"q\""}'                 'select(.a)'
# And a downstream stage must still see the JSON string, so the type errors that
# guard it keep firing (`"abc" | keys` raises). Those are `type_probe`s below;
# these pin the ones that legitimately ANSWER.
jq_probe '"abc"'                         '. | length'
jq_probe '"abc"'                         '. * 2'
jq_probe '"abc"'                         '. + "d"'

# ── jq: null handling ───────────────────────────────────────────────────────
# jq renders an explicit null, an absent key and an out-of-range index all as
# `null`. Nothing in SPEC.md carves out a different rule, so the jq answer is the
# contract for every path construct the docs claim.
jq_probe '{"a":null}'                    '.a'
jq_probe '{"a":1}'                       '.b'
jq_probe '{"a":{"b":null}}'              '.a.b'
jq_probe '{"a":1}'                       '.b.c'
jq_probe '[1,null,2]'                    '.[]'
jq_probe '[1,2]'                         '.[5]'
jq_probe '["a"]'                         '.[-4]'
jq_probe '{"a":[null]}'                  '.a[0]'
jq_probe 'null'                          '.'
jq_probe '[null,null]'                   '.[]'

# ── jq: scalar rendering / number formatting ────────────────────────────────
jq_probe 'true'                          '.'
jq_probe '1.0'                           '.'
jq_probe '[1.5,2.0]'                     '.[]'
jq_probe '{"n":3.25}'                    '.n'
jq_probe '{"n":-0.5}'                    '.n'
jq_probe '{"s":"has space"}'             '.s'

# ── jq: a COMPUTED number, across the magnitude range ───────────────────────
# Every numeric probe above is a small value, so the whole corpus only ever
# exercised the one decade band where any formatter agrees with jq's. Outside it
# arb printed `1e308 * 1` as 309 digits and `1e18 / 3` as `333333333333333312`
# (18 digits a double does not carry, against jq's 16 zero-padded).
#
# `* 1` and `+ 0` are identities on the value, so a difference is the RENDERER
# alone. They also force jq to COMPUTE: on a number it never touched, jq reprints
# the source literal via decNumber (`1e17` comes back as `1E+17`), which arb has
# no equivalent for — SPEC §6 makes every value an f64 and keeps no literal. The
# probes therefore stay on computed values, and the deviation is documented
# rather than papered over with a probe that reads the literal back.
jq_probe '{"n":1e16}'                    '.n * 1'
jq_probe '{"n":1e17}'                    '.n * 2'
jq_probe '{"n":1e18}'                    '.n / 3'
jq_probe '{"n":1e21}'                    '.n + 0'
jq_probe '{"n":1.5e16}'                  '.n * 1'
jq_probe '{"n":1.5e17}'                  '.n * 1'
jq_probe '{"n":1e-5}'                    '.n * 1'
jq_probe '{"n":1e-7}'                    '.n + 0'
jq_probe '{"n":2e-9}'                    '.n * 2'
jq_probe '{"n":1e-300}'                  '.n * 1'
jq_probe '{"n":1.23456789012345e20}'     '.n * 1'
# The same values through `map(…)`, which REBUILDS a JSON array from the rendered
# elements. Re-parsing them into a `serde_json::Value` and printing that ran the
# number through a second, different formatter, so the array disagreed with the
# scalar next to it: `1e-06` came back out as `1e-6` and `1e-05` as `0.00001`.
jq_probe '[1e16,1e17,1e18]'              'map(. * 1)'
jq_probe '[1e-5,1e-6,1e-7,2e-9]'         'map(. * 1)'
jq_probe '[1.23456789012345e20]'         'map(. + 0)'
jq_probe '[1e17,2e-9]'                   'map(. * 2)'
jq_probe '[1e15,1.5e16,9.99e16]'         'map(. * 1)'
jq_probe '[1,2,3]'                       'map(. / 3)'
jq_probe '[1e17,1e17]'                   'add'
jq_probe '[0.1,0.2]'                     'add'

# ── jq: `%` on a fractional operand ─────────────────────────────────────────
# `%` reaches the jq front-end (a leading `.` routes the whole body there) and
# lowers to the same fusevm `Op::Mod` as arb's own expression language, i.e. the
# f64 remainder. jq's `%` truncates BOTH operands to integers first, so
# `.n % 3` on 5.5 is 2.5 here and 2 in jq.
#
# Every `%` probe in this file's history used an integer, where the two rules
# coincide exactly — which is why an operator answering differently from jq
# inside a front-end that promises jq's answers went unmeasured. It is measured
# now. These probes are RED and stay red until the rule is decided: SPEC §6 keeps
# the f64 remainder for arb's own `map x % 3` on the grounds that every arb value
# is an f64, and the open question is only whether the JQ CONTEXT should follow
# jq the way `keys` / `flatten` / `to_entries` already do (README: "context
# decides"). Rewording these to match arb would make the harness agree with the
# thing it exists to check.
jq_probe '{"n":5.5}'                     '.n % 3'
jq_probe '{"n":7.25}'                    '.n % 2'
jq_probe '{"n":-5.5}'                    '.n % 3'
jq_probe '{"n":0.5}'                     '.n % 2'
jq_probe '{"n":100.75}'                  '.n % 4'
# The integer case, which agrees — pinned so a change to either rule has to keep
# the band the two share.
jq_probe '{"n":7}'                       '.n % 3'

# ── jq: per-element arithmetic between two FIELDS (SPEC §8 names `.a + .b`) ──
# The corpus only ever put a LITERAL on the right (`.n * 1`), so the spelling the
# SPEC actually prints was unmeasured. `+` is the interesting one: jq overloads it
# per type — string concat, array concat, object merge, and null as the identity
# on either side — where an f64 evaluator reads every non-number as NaN and
# answers `null` to all five.
jq_probe '{"a":3,"b":4}'                 '.a + .b'
jq_probe '{"a":3,"b":4}'                 '.a - .b'
jq_probe '{"a":3,"b":4}'                 '.a * .b'
jq_probe '{"a":3,"b":4}'                 '.a / .b'
jq_probe '{"a":7,"b":4}'                 '.a % .b'
jq_probe '{"a":3}'                       '.a * 2 + 1'
jq_probe '{"a":"x","b":"y"}'             '.a + .b'
jq_probe '{"a":[1],"b":[2]}'             '.a + .b'
jq_probe '{"a":{"x":1},"b":{"x":9,"y":2}}' '.a + .b'
jq_probe '{"a":{"x":{"p":1}},"b":{"x":{"q":2}}}' '.a * .b'
jq_probe '{"a":[1,2,3],"b":[2]}'         '.a - .b'
jq_probe '{"a":"a,b,c","b":","}'         '.a / .b'
jq_probe '{"a":"ab","b":3}'              '.a * .b'
jq_probe '{"a":null}'                    '.a + 1'
jq_probe '{"a":1}'                       '.a + null'
jq_probe '[{"a":1,"b":2},{"a":3,"b":4}]' 'map(.a + .b)'
jq_probe '[{"a":1,"b":2},{"a":3,"b":4}]' '.[] | select(.a + .b > 4)'

# ── jq: a comparison used as a VALUE ────────────────────────────────────────
# jq's compare yields a BOOLEAN. arb's f64 evaluator yielded 1/0, so `map(. > 1)`
# rendered `[0,1,1]` against jq's `[false,true,true]` — a wrong value in the
# claimed subset that no probe covered, because every earlier compare sat inside
# a `select(…)` where only its truthiness was ever observed.
jq_probe '[1,2,3]'                       'map(. > 1)'
jq_probe '[1,2,3]'                       '.[] | . > 1'
jq_probe '{"a":1}'                       '.a == 1'
jq_probe '{"a":1}'                       '.a != 1'
jq_probe '["a","b"]'                     'map(. == "a")'
jq_probe '{"a":1,"b":2}'                 '.a < .b'
jq_probe '{"a":1,"b":2}'                 '.a and .b'
jq_probe '{"a":null,"b":false}'          '.a or .b'

# ── jq: `select` truthiness and type-strict equality ─────────────────────────
# jq's falsy set is exactly `false` and `null`. The f64 evaluator's was `0` and
# NaN, which inverted BOTH ends: `select(.a)` dropped `0` and kept `null`.
jq_probe '{"a":0}'                       'select(.a)'
jq_probe '{"a":""}'                      'select(.a)'
jq_probe '{"a":[]}'                      'select(.a)'
jq_probe '{"a":{}}'                      'select(.a)'
jq_probe '{"a":null}'                    'select(.a)'
jq_probe '{"a":false}'                   'select(.a)'
jq_probe '{"a":true}'                    'select(.a)'
# `==` compares TYPE as well as value, and an absent key is `null`.
jq_probe '{"a":"1"}'                     'select(.a == 1)'
jq_probe '{"a":1}'                       'select(.a == "1")'
jq_probe '{"a":true}'                    'select(.a == true)'
jq_probe '{"a":1}'                       'select(.b == null)'
jq_probe '{"a":[1]}'                     'select(.a == [1])'
jq_probe '{"a":1}'                       'select(.a and true)'

# ── jq: the TOTAL ORDER across types (SPEC §8 names it verbatim) ────────────
# SPEC §8 states the order in full — `null < false < true < numbers < strings <
# arrays < objects` — but every ordered compare in the corpus above puts two
# values of the SAME type on the two sides. The cross-type half of a claim spelled
# out that explicitly was scored by nothing, and it is the half an f64 evaluator
# cannot express at all (it has one type), so it is exactly where a regression
# would land.
jq_probe '{"a":null,"b":false}'          '.a < .b'
jq_probe '{"a":false,"b":true}'          '.a < .b'
jq_probe '{"a":true,"b":1}'              '.a < .b'
jq_probe '{"a":1,"b":"s"}'               '.a < .b'
jq_probe '{"a":"s","b":[1]}'             '.a < .b'
jq_probe '{"a":[1],"b":{"x":1}}'         '.a < .b'
jq_probe '{"a":null,"b":{"x":1}}'        '.a < .b'
jq_probe '{"a":1,"b":"s"}'               '.a > .b'
jq_probe '{"a":null,"b":0}'              '.a <= .b'
# Arrays and objects compare ELEMENTWISE, not by length or address.
jq_probe '{"a":[1,2],"b":[1,3]}'         '.a < .b'
jq_probe '{"a":[1,2],"b":[1]}'           '.a > .b'
jq_probe '{"a":{"x":1},"b":{"y":1}}'     '.a < .b'
jq_probe '[[2],[1,9]]'                   'map(. > [1])'
jq_probe '[null,false,true,1,"s"]'       'map(. > null)'

# ── jq: ITERATE MID-PATH — `.users[].name`, SPEC's own first table row ──────
# The SPEC §8 translation table opens with `.users[].name`, and the corpus never
# ran it: every iterate probe either ENDS at `[]` (`.items[]`) or indexes a fixed
# element first (`.a[0].b`). Continuing a path THROUGH an iterate is a different
# code path — it fans one input into many and then keeps walking each one.
jq_probe '{"users":[{"name":"a"},{"name":"b"}]}' '.users[].name'
jq_probe '{"a":[{"b":1},{"b":2}]}'       '.a[].b'
jq_probe '{"a":[[1,2],[3]]}'             '.a[][]'
jq_probe '{"a":[{"b":{"c":1}}]}'         '.a[].b.c'
jq_probe '{"a":{"x":{"n":1},"y":{"n":2}}}' '.a[].n'
jq_probe '{"a":[{"b":1}]}'               '.a[].b'
jq_probe '{"a":[]}'                      '.a[].b'
jq_probe '{"a":[{"b":1},{"c":2}]}'       '.a[].b'
jq_probe '{"a":[1,2]}'                   '.a[1:]'

# ── jq: the pipelines SPEC §8 PRINTS as its own examples ────────────────────
# SPEC §8's "Literal front-ends" block shows four runnable lines. A documented
# example is the highest-traffic construct in any spec — it is what a reader
# copies first — and none of them was in the corpus. Each combines stages that
# were only ever probed in isolation.
jq_probe '{"users":[{"age":20,"name":"x"},{"age":9,"name":"y"}]}' '.users[] | select(.age >= 18) | .name'
jq_probe '[{"price":3},{"price":4}]'     'map(.price) | add'
jq_probe '{"a":[1,2,3]}'                 '.a[] | select(. > 1) | . * 10'
jq_probe '[{"n":1},{"n":2}]'             '.[] | .n | . + 1'
jq_probe '{"a":{"b":[1,2]}}'             '.a.b | map(. * 2) | add'
jq_probe '[{"a":1},{"a":2},{"a":3}]'     'map(select(.a > 1)) | length'

# ── jq: a SUBSCRIPT KEEPS ITS TYPE (SPEC §8 spells out `.["0"]`) ────────────
# SPEC §8: "A subscript keeps its type: `.["0"]` is an object key and `.[0]` is an
# array index, so `[1,2] | .["0"]` refuses rather than reading the first element."
# The digit-string is the case that separates a real type check from a coercion,
# and only the non-digit `.["a"]` form was probed.
jq_probe '{"0":"v"}'                     '.["0"]'
jq_probe '{"0":{"1":"deep"}}'            '.["0"]["1"]'
jq_probe '{"-1":"neg"}'                  '.["-1"]'
jq_probe '[10,20]'                       '.[0]'

# ── jq: MULTIPLE INPUT LINES ────────────────────────────────────────────────
# arb is a LINE stream, so per-line scoping is a property no single-line probe can
# observe. The `map(…)` rewrap bug in round 1 merged two lines into one flat
# stream and a one-line corpus called it a pass. Every stage family is re-run here
# over two lines to pin that they stay independent.
jq_probe '[1,2]
[3,4]'                                   'map(. * 2)'
jq_probe '{"a":1}
{"a":2}'                                 '.a'
jq_probe '{"a":1}
{"a":2}'                                 'select(.a > 1)'
jq_probe '[1]
[2]'                                     'add'
jq_probe '{"a":1}
{"a":2}'                                 '. | keys'
jq_probe '{"a":[1,2]}
{"a":[3]}'                               '.a[]'
jq_probe '"x"
"y"'                                     '.'
jq_probe '{"a":1}
{"b":2}'                                 '.a'
jq_probe '[1,2]
[3]'                                     '. | length'

# ── jq: UNICODE — length, slice and key order are all codepoint-based ───────
# `length` on a string is jq's CODEPOINT count and a slice indexes codepoints, so
# a byte-oriented implementation passes every ASCII probe in this file and fails
# the moment real text arrives. `keys` sorts by codepoint too.
jq_probe '"héllo"'                       'length'
jq_probe '"日本語"'                       'length'
jq_probe '"日本語abc"'                    '.[1:3]'
jq_probe '"日本語abc"'                    '.[-3:]'
jq_probe '{"é":1,"z":2,"A":3}'           '. | keys'
jq_probe '{"b":1,"B":2,"á":3,"1":4}'     '. | keys'
jq_probe '["日本"]'                       'map(length)'
jq_probe '{"a":"héllo"}'                 '.a'

# ── jq: COMPOSITION — map inside map, reducer inside map ────────────────────
# Every `map(…)` probe above has a one-stage body. Nesting is where the per-input
# scope and the array rewrap have to hold at two levels at once.
jq_probe '[[1,2],[3,4]]'                 'map(map(. * 2))'
jq_probe '[[1,2],[3,4]]'                 'map(map(. > 1))'
jq_probe '[{"a":[1,2]}]'                 'map(.a | length)'
jq_probe '[{"a":{"b":[1,2]}}]'           'map(.a.b | add)'
jq_probe '[1,2,3]'                       'map(. + 1) | map(. * 2)'
jq_probe '[[1,2],[3]]'                   'map(add)'
jq_probe '[[1,2],[3]]'                   'map(length) | add'
jq_probe '[{"a":1},{"a":2}]'             'map(select(.a > 1)) | map(.a)'

# ── jq: `values` is `select(. != null)`, not object-value iteration ─────────
jq_probe '[1,null,2,null]'               'map(values)'
jq_probe '0'                             'values'
jq_probe '[]'                            'values'
jq_probe '{}'                            'values'
jq_probe '[1,""]'                        '.[] | values'
jq_probe '{"a":null,"b":1}'              '.a | values'

# ── jq: string ESCAPES survive a path ───────────────────────────────────────
# `-r` prints a string's CONTENT, so the escapes have to be decoded exactly once.
jq_probe '{"a":"tab\there"}'             '.a'
jq_probe '{"a":"nl\nhere"}'              '.a'
jq_probe '{"a":"q\"uote"}'               '.a'
jq_probe '{"a":"sl\\ash"}'               '.a'
jq_probe '{"a":"unié"}'                  '.a'
jq_probe '["tab\there"]'                 '.'
jq_probe '{"a":"tab\there"}'             'map(.)'

# ── jq: to_entries / flatten at depth ───────────────────────────────────────
jq_probe '{"a":null}'                    '. | to_entries'
jq_probe '{"a":[1],"b":{"c":2}}'         '. | to_entries'
jq_probe '[[1,[2]],3]'                   '. | flatten'
jq_probe '[null,[null]]'                 '. | flatten'
jq_probe '[[["a"]]]'                     '. | flatten'
jq_probe '{"a":{"b":1}}'                 '.a | keys'
jq_probe '{"a":[1,2]}'                   '.a | has(1)'
jq_probe '{"a":{"b":1}}'                 '.a | has("b")'

# ── jq: an EMPTY result stream ──────────────────────────────────────────────
# `select` that matches nothing must emit NOTHING, not a blank line or a `null`.
jq_probe '[1,2,3]'                       '.[] | select(. > 5)'
jq_probe '[1,2,3]'                       'map(select(. > 5))'
jq_probe '{"a":1}'                       'select(.a > 5) | .a'
jq_probe '[]'                            'map(select(.))'

# ── jq: operator PRECEDENCE in a value expression ───────────────────────────
jq_probe '{"a":2}'                       '.a + 3 * 2'
jq_probe '{"a":10}'                      '.a - 2 - 3'
jq_probe '{"a":2,"b":3,"c":4}'           '.a * .b + .c'
jq_probe '{"a":12}'                      '.a / 2 / 3'
jq_probe '{"a":1,"b":2}'                 '.a < .b and .b < 3'

# ── jq: nested field paths inside select/map ────────────────────────────────
# `select(.a.b > 1)` used to be refused outright ("nested field path … is
# unsupported"). jq accepts it, so the refusal was a gap in the claimed subset.
jq_probe '{"a":{"b":2}}'                 'select(.a.b > 1)'
jq_probe '{"a":{"b":2}}'                 'select(.a.b > 5)'
jq_probe '[{"a":{"b":2}}]'               'map(.a.b)'
jq_probe '[{"a":{"b":2}},{"a":{"b":9}}]' 'map(.a.b) | add'

# ── jq: deeper paths and pipes ──────────────────────────────────────────────
jq_probe '{"a":{"b":{"c":1}}}'           '.a | .b | .c'
jq_probe '{"a":[{"b":5}]}'               '.a[0].b'
jq_probe '{"a":{"b":1}}'                 '.["a"]["b"]'
jq_probe '{"a":{"b":1}}'                 '.a["b"]'
jq_probe '{"a b":{"c":1}}'               '.["a b"].c'
jq_probe '{"a":[[1,2],[3]]}'             '.a[1][0]'

# ── jq: iterate over every container shape ──────────────────────────────────
jq_probe '{"a":1,"b":2}'                 '.[]'
jq_probe '{}'                            '.[]'
jq_probe '[]'                            '.[]'
jq_probe '{"a":{"x":1,"y":2}}'           '.a[]'
jq_probe '[[1,2],[3]]'                   '.[] | .[]'

# ── jq: slice edges ─────────────────────────────────────────────────────────
jq_probe '[1,2,3]'                       '.[5:9]'
jq_probe '[1,2,3]'                       '.[3:1]'
jq_probe '[1,2,3,4,5]'                   '.[-3:-1]'
jq_probe '[1,2,3]'                       '.[-9:]'
jq_probe '[1,2,3]'                       '.[0:0]'
jq_probe '"hello"'                       '.[:2]'
jq_probe '"hello"'                       '.[-2:]'
jq_probe 'null'                          '.[1:2]'

# ── jq: the builtins reached through the jq front-end, at their edges ───────
# Every one of these routes through `. | …`, which is the jq CONTEXT: SPEC §8's
# rule is that a body command beginning with a jq literal answers as jq does,
# while the bare alphanumeric spelling stays arb's native verb.
jq_probe '[]'                            '. | add'
jq_probe '[null,null]'                   '. | add'
jq_probe '{"a":1,"b":2}'                 '. | add'
jq_probe '["a","b"]'                     '. | add'
jq_probe '[1,2,3]'                       '. | add'
jq_probe '[]'                            '. | length'
jq_probe '{}'                            '. | length'
jq_probe '""'                            '. | length'
jq_probe 'null'                          '. | length'
jq_probe '-7'                            '. | length'
jq_probe '3.5'                           '. | length'
jq_probe '[]'                            '. | keys'
jq_probe '{}'                            '. | keys'
jq_probe '{"b":1,"A":2,"a":3}'           '. | keys'
jq_probe '[7,8]'                         '. | keys'
jq_probe '[]'                            '. | to_entries'
jq_probe '[7,8]'                         '. | to_entries'
jq_probe '{"a":{"b":1}}'                 '. | to_entries'
jq_probe '[]'                            '. | flatten'
jq_probe '[[[]]]'                        '. | flatten'
jq_probe '{"a":[1,2]}'                   '. | flatten'
jq_probe '[1,2]'                         'has(0)'
jq_probe '[1,2]'                         'has(5)'
jq_probe 'null'                          'has("a")'
jq_probe '[1,null]'                      '.[] | values'
jq_probe 'false'                         'values'

# ── jq: the constructs that used to be refused ──────────────────────────────
# Every probe below was an `err_probe` — an assertion that arb REFUSES the
# construct, because a `Vec<QueryOp>` could not express it and SPEC §8 promised a
# hard error rather than a silent reinterpretation. arb now implements all of
# them on its own jq engine, so each one is checked against the REFERENCE instead
# of against a refusal: `jq_probe` byte-diffs the answer, and `type_probe` (which
# verifies that jq refuses too) covers the ones that raise on this input.
#
# The direction matters. "arb exits non-zero" is satisfied by any error at all,
# including the wrong one; "arb's stdout equals jq's, byte for byte" is not. Every
# line here is a tighter assertion than the one it replaced, on the same input.
jq_probe    '{"a":1,"b":2,"foo":null}'   '.foo // 0'                      # alternative operator
jq_probe    '{"a":1,"b":2,"foo":null}'   '.foo?'                          # error suppression
jq_probe    '{"a":1,"b":2,"foo":null}'   '..'                             # recursive descent
jq_probe    '{"a":1,"b":2,"foo":null}'   '.a as $x | $x'                  # variable binding
jq_probe    '{"a":1,"b":2,"foo":null}'   'reduce .[] as $x (0; . + $x)'   # reduce
jq_probe    '{"a":1,"b":2,"foo":null}'   'foreach .[] as $x (0; .+$x; .)' # foreach
jq_probe    '{"a":1,"b":2,"foo":null}'   'try .a catch 0'                 # try/catch
jq_probe    '{"a":1,"b":2,"foo":null}'   'paths'
ext_probe    '{"a":1,"b":[2]}'  'leaf_paths' $'["a"]\n["b",0]'
jq_probe    '{"a":1,"b":2,"foo":null}'   'getpath(["a"])'
jq_probe    '{"a":1,"b":2,"foo":null}'   'setpath(["a"];9)'
jq_probe    '{"a":1,"b":2,"foo":null}'   'delpaths([["a"]])'
type_probe  '{"a":1,"b":2,"foo":null}'   'from_entries'
jq_probe    '{"a":1,"b":2,"foo":null}'   'with_entries(.value += 1)'
type_probe  '{"a":1,"b":2,"foo":null}'   'group_by(.a)'
type_probe  '{"a":1,"b":2,"foo":null}'   'unique_by(.a)'
type_probe  '{"a":1,"b":2,"foo":null}'   'min_by(.a)'
type_probe  '{"a":1,"b":2,"foo":null}'   'max_by(.a)'
jq_probe    '{"a":1,"b":2,"foo":null}'   'any'
jq_probe    '{"a":1,"b":2,"foo":null}'   'all'
jq_probe    '{"a":1,"b":2,"foo":null}'   'range(3)'
type_probe  '{"a":1,"b":2,"foo":null}'   'splits(",")'
type_probe  '{"a":1,"b":2,"foo":null}'   'sub("a";"b")'
type_probe  '{"a":1,"b":2,"foo":null}'   'gsub("(?<x>a)";"\(.x)")'
type_probe  '{"a":1,"b":2,"foo":null}'   'ascii_downcase'
jq_probe    '{"a":1,"b":2,"foo":null}'   'env.HOME'
jq_probe    '{"a":1,"b":2,"foo":null}'   '$ENV.HOME'
type_probe  '{"a":1,"b":2,"foo":null}'   'input'
jq_probe    '{"a":1,"b":2,"foo":null}'   'inputs'
jq_probe    '{"a":1,"b":2,"foo":null}'   '@base64'
type_probe  '{"a":1,"b":2,"foo":null}'   '@csv'
type_probe  '{"a":1,"b":2,"foo":null}'   '@tsv'
jq_probe    '{"a":1,"b":2,"foo":null}'   '@json'

# A `@format` whose name is followed immediately by an operator, with no space.
# The body dispatcher used to claim a format only as an exact word or with a
# TRAILING SPACE, so every one of these fell through to the xpath front-end —
# where `@base64` is an attribute step and `|` is the union operator, so they
# parsed, ran against JSON, and answered nothing. `@base64|.` printed the input
# unencoded. Nothing in this corpus wrote a format without a space after it,
# which is why a jq program silently answering as xpath survived it.
jq_probe    '"hello"'                    '@base64|@base64d'
jq_probe    '"hello"'                    '@base64|.'
jq_probe    '"hello"'                    '@base64|length'
jq_probe    '"aGVsbG8="'                 '@base64d|length'
jq_probe    '"hello"'                    '@text|length'
jq_probe    '"hi"'                       '@uri|ascii_upcase'
jq_probe    '["a","b"]'                  '@csv|length'
jq_probe    '["a","b"]'                  '@tsv|length'
jq_probe    '"<p>"'                      '@html|length'
jq_probe    '"a b"'                      '@sh|length'
jq_probe    '{"a":1}'                    '@json|length'
# The same name inside a parenthesised position, the other boundary character
# a format can end on.
jq_probe    '"hello"'                    '[@base64]|length'
jq_probe    '"hello"'                    '(@base64)|length'
# A `@` that is NOT one of the nine names still selects an xpath attribute, and
# a name that merely STARTS with one is not claimed either.
jq_probe    '"hello"'                    '@base64 |length'

# `@base64d` on input that is not a whole number of base64 groups. A group is
# 2, 3 or 4 characters; ONE leftover character encodes no byte, and the
# reference rejects exactly that case — every other length decodes with its
# spare bits discarded. arb used to decode the rejected case to garbage rather
# than refuse it, so these pin both sides of the boundary.
type_probe  '"a"'                        '@base64d'
type_probe  '"abcde"'                    '@base64d'
type_probe  '"hello"'                    '@base64d'
jq_probe    '"ab"'                       '@base64d|length'
jq_probe    '"abc"'                      '@base64d|length'
jq_probe    '"abcd"'                     '@base64d|length'
jq_probe    '"abcdef"'                   '@base64d|length'
jq_probe    '"YQ=="'                     '@base64d'
jq_probe    '"aGVsbG8="'                 '@base64d'
jq_probe    '{"a":1,"b":2,"foo":null}'   'first(.[])'
jq_probe    '{"a":1,"b":2,"foo":null}'   'limit(2;.[])'
jq_probe    '{"a":1,"b":2,"foo":null}'   '.[] | not'
jq_probe    '{"a":1,"b":2,"foo":null}'   'tostring'
type_probe  '{"a":1,"b":2,"foo":null}'   'tonumber'
jq_probe    '{"a":1,"b":2,"foo":null}'   '. as [$a] ?// {$a} | $a'         # optional destructuring
# Arithmetic against a whole OBJECT. jq refuses every one of these by name
# ("object and number cannot be divided"), and arb answers `null` with exit 0 —
# a silent reinterpretation of a construct that has no meaning, which SPEC §8
# rules out explicitly. This is the `select(.status == "ok")` shape again: not a
# wrong number, but an answer where there should be a refusal, so nothing in the
# output says the query did not do what it said. It is not `%`-specific — every
# arithmetic operator does it, so all five are probed rather than the one that
# happened to be under the microscope.
type_probe  '{"a":1,"b":2,"foo":null}'   '. + 3'
type_probe  '{"a":1,"b":2,"foo":null}'   '. - 3'
type_probe  '{"a":1,"b":2,"foo":null}'   '. * 3'
type_probe  '{"a":1,"b":2,"foo":null}'   '. / 3'
type_probe  '{"a":1,"b":2,"foo":null}'   '. % 3'
# jq's CONSTRUCTORS and control flow. SPEC §8's out-of-subset list names builtins
# and operators but no SYNTAX form, so object construction, array construction,
# the comma operator, `if/then/else` and plain PARENTHESES were all unlisted and
# unprobed — while being the constructs a jq user reaches for soonest after the
# ones already covered. A silent answer from any of them would be the
# `select(.status == "ok")` shape again: a filter that quietly means something
# else. (`(.a + 3) * 2` refuses with `unknown verb \`(.a\``, because the body
# dispatcher routes only `.`/`select(`/`map(`/`has(` to the jq front-end — a hard
# error either way, which is what the contract requires.)
jq_probe    '{"a":1,"b":2,"foo":null}'   '{a: .a}'
jq_probe    '{"a":1,"b":2,"foo":null}'   '{"k": .a}'
jq_probe    '{"a":1,"b":2,"foo":null}'   '[.a, .b]'
jq_probe    '{"a":1,"b":2,"foo":null}'   '[.a]'
jq_probe    '{"a":1,"b":2,"foo":null}'   '.a, .b'
jq_probe    '{"a":1,"b":2,"foo":null}'   'if .a then 1 else 2 end'
jq_probe    '{"a":1,"b":2,"foo":null}'   '(.a + 3) * 2'
jq_probe    '{"a":1,"b":2,"foo":null}'   'empty'
type_probe  '{"a":1,"b":2,"foo":null}'   'error'
# Type/encoding builtins.
jq_probe    '{"a":1,"b":2,"foo":null}'   'type'
jq_probe    '{"a":1,"b":2,"foo":null}'   'tojson'
type_probe  '{"a":1,"b":2,"foo":null}'   'fromjson'
jq_probe    '{"a":1,"b":2,"foo":null}'   'tostream'
jq_probe    '{"a":1,"b":2,"foo":null}'   'input_line_number'
jq_probe    '{"a":1,"b":2,"foo":null}'   '$__loc__'
superset_probe
jq_probe    '{"a":1,"b":2,"foo":null}'   'halt'
jq_probe    '{"a":1,"b":2,"foo":null}'   'debug'
# String builtins beyond the regex family already listed.
type_probe  '{"a":1,"b":2,"foo":null}'   'ltrimstr("x")'
type_probe  '{"a":1,"b":2,"foo":null}'   'rtrimstr("x")'
type_probe  '{"a":1,"b":2,"foo":null}'   'startswith("x")'
type_probe  '{"a":1,"b":2,"foo":null}'   'endswith("x")'
type_probe  '{"a":1,"b":2,"foo":null}'   'ascii_upcase'
type_probe  '{"a":1,"b":2,"foo":null}'   'explode'
type_probe  '{"a":1,"b":2,"foo":null}'   'implode'
jq_probe    '{"a":1,"b":2,"foo":null}'   'join(",")'
type_probe  '{"a":1,"b":2,"foo":null}'   'test("x")'
type_probe  '{"a":1,"b":2,"foo":null}'   'capture("x")'
type_probe  '{"a":1,"b":2,"foo":null}'   'match("x")'
type_probe  '{"a":1,"b":2,"foo":null}'   'scan("x")'
# Array/object builtins that are NOT arb native verbs — the ones that are
# (`sort`, `min`, `max`, `floor`, `abs`) stay out of this list on purpose: SPEC §8
# makes a bare alphanumeric word the NATIVE verb, so accepting them is the
# documented context rule, not a jq leak.
type_probe  '{"a":1,"b":2,"foo":null}'   'reverse'
type_probe  '{"a":1,"b":2,"foo":null}'   'unique'
type_probe  '{"a":1,"b":2,"foo":null}'   'contains("x")'
type_probe  '{"a":1,"b":2,"foo":null}'   'inside([1])'
type_probe  '{"a":1,"b":2,"foo":null}'   'indices(1)'
jq_probe    '{"a":1,"b":2,"foo":null}'   'flatten(1)'
jq_probe    '{"a":1,"b":2,"foo":null}'   'del(.a)'
jq_probe    '{"a":1,"b":2,"foo":null}'   'path(.a)'
jq_probe    '{"a":1,"b":2,"foo":null}'   'walk(.)'
type_probe  '{"a":1,"b":2,"foo":null}'   'combinations'
type_probe  '{"a":1,"b":2,"foo":null}'   'transpose'
jq_probe    '{"a":1,"b":2,"foo":null}'   'to_entries[]'
# The type-filter family.
jq_probe    '{"a":1,"b":2,"foo":null}'   'recurse'
jq_probe    '{"a":1,"b":2,"foo":null}'   'objects'
jq_probe    '{"a":1,"b":2,"foo":null}'   'arrays'
jq_probe    '{"a":1,"b":2,"foo":null}'   'booleans'
jq_probe    '{"a":1,"b":2,"foo":null}'   'nulls'
jq_probe    '{"a":1,"b":2,"foo":null}'   'scalars'
jq_probe    '{"a":1,"b":2,"foo":null}'   'iterables'
# Math builtins.
type_probe  '{"a":1,"b":2,"foo":null}'   'sqrt'
jq_probe    '{"a":1,"b":2,"foo":null}'   'infinite'
jq_probe    '{"a":1,"b":2,"foo":null}'   'nan'
jq_probe    '{"a":1,"b":2,"foo":null}'   'isnan'
type_probe  '{"a":1,"b":2,"foo":null}'   'todate'
jq_probe    '{"a":1,"b":2,"foo":null}'   'now | type'

# ── jq: the surface the engine added, probed on its own terms ───────────────
# The block above is the OLD out-of-subset list, re-pointed at the reference. It
# was written to enumerate refusals, so it probes each construct once, in its
# simplest form, against one object. These probe the same constructs where they
# actually get used: generators feeding generators, paths under assignment,
# destructuring with alternatives, and the libm surface `builtins` names.
D='{"a":1,"b":"x","c":[1,2,3],"d":{"e":5},"n":null,"t":true}'
R='[{"id":1,"n":"a","v":10},{"id":2,"n":"b","v":5},{"id":3,"n":"a","v":7}]'

jq_probe "$D" '[(1,2) + (10,20)]'
jq_probe "$D" '[{x:(1,2), y:(3,4)}]'
jq_probe "$D" '[.c[] as $x | .d.e as $y | $x + $y]'
jq_probe "$D" '. as $r | .c | map(. + $r.a)'
jq_probe "$D" '[.c[] | . as $x | {($x|tostring): $x}]'
jq_probe "$D" 'reduce (.c[]) as $x ({}; .[$x|tostring] = $x)'
jq_probe "$D" '[foreach (.c[]) as $x ([]; . + [$x]; .)]'
jq_probe "$D" '[label $out | (.c[] | if . == 2 then break $out else . end)]'
jq_probe "$D" 'def f: . * 2; .c | map(f)'
jq_probe "$D" 'def g(x): x + x; .a | g(.)'
jq_probe "$D" 'def h($n): $n * 3; .a | h(.)'
jq_probe "$D" 'def fact: if . <= 1 then 1 else . * (. - 1 | fact) end; 5 | fact'
jq_probe "$D" '. as {c: [$first]} | $first'
jq_probe "$D" '. as {$a, c: $cc} | [$a, $cc]'
type_probe "$D" '[.c[] | . as [$x] | $x]'
jq_probe "$D" 'del(.c[0], .d.e)'
jq_probe "$D" '(.a, .d.e) |= . + 100'
jq_probe "$D" '.c[1:2] |= map(. * 10)'
jq_probe "$D" '.c |= map(. * 2)'
jq_probe "$D" 'pick(.a, .d)'
jq_probe "$D" '[paths(type == "number")]'
jq_probe "$D" '[tostream] | fromstream(.[])'
jq_probe "$D" '[.. | numbers]'
jq_probe "$D" 'walk(if type == "number" then . + 1 else . end)'
jq_probe "$D" 'getpath(["d","e"]), getpath(["z"])'
jq_probe "$D" '[limit(2; range(10))]'
jq_probe "$D" '[first(range(10)), last(range(10)), nth(3; range(10))]'
jq_probe "$D" '[range(0;10;3)], [range(10;0;-3)]'
jq_probe "$D" 'isempty(.c[]), isempty(empty)'
jq_probe "$D" '[.c[] | while(. < 10; . * 2)]'
jq_probe "$D" '[.c[] | until(. > 5; . + 1)]'
jq_probe "$D" '[limit(3; repeat(1))]'
jq_probe "$D" '"n=\(.a) s=\(.b)"'
jq_probe "$D" '["\(.c[])"]'
jq_probe "$D" '@base64 "v=\(.a)"'
jq_probe "$D" '.b | @uri, @html, @sh, @json, @text'
jq_probe "$D" '.c | @csv, @tsv'
jq_probe "$D" '.b | test("X"; "i"), test("X")'
jq_probe "$D" '.b | [match("."; "g") | .offset]'
jq_probe "$D" '.b | sub("(?<c>.)"; "[\(.c)]")'
jq_probe "$D" '.b | gsub("(?<c>.)"; "[\(.c)]")'
jq_probe "$D" '.b | [scan(".")], [splits("")]'
jq_probe "$D" '.b | capture("(?<w>.+)")'
jq_probe "$D" '.b | ltrimstr("x"), rtrimstr("x"), trimstr("x")'
jq_probe "$R" 'group_by(.n) | map({n: .[0].n, total: (map(.v) | add)})'
jq_probe "$R" 'INDEX(.id) | keys_unsorted'
jq_probe "$R" 'map(.n) | IN("a")'
jq_probe "$R" 'sort_by(.n, .v) | map(.id)'
jq_probe "$R" '[.[] | with_entries(select(.key != "id"))]'
jq_probe "$R" 'INDEX(.id|tostring) as $i | [{"id":1}] | JOIN($i; .id|tostring)'
jq_probe "$R" '[skip(1; .[]) | .id]'
jq_probe "$R" 'map(.v) | sort | bsearch(7)'
jq_probe "$D" '[., inputs] | length'
jq_probe "$D" 'input_line_number'
jq_probe "$D" '$__loc__'

# The libm surface `builtins` names. Every one of these was reported MISSING by
# `superset_probe` before it was implemented, which is what that probe is for.
jq_probe '0.5' 'lgamma, gamma, tgamma, erf, erfc'
jq_probe '0.5' 'j0, j1, y0, y1'
jq_probe '0.5' 'frexp, modf, lgamma_r'
jq_probe '0.5' 'asinh, atanh, expm1, log1p, isfinite'
jq_probe '2'   'acosh, significand, logb, trunc, nearbyint'
jq_probe '2.5' 'rint, round, floor, ceil'
jq_probe '3.5' 'rint, round'
jq_probe 'null' 'drem(5;3), remainder(5;3), fdim(5;3), fmod(5;3)'
jq_probe 'null' 'hypot(3;4), copysign(2;-3), nextafter(1;2), nexttoward(1;2)'
jq_probe 'null' 'ldexp(2;3), scalb(3;2), scalbln(3;2), fma(2;3;4)'
jq_probe 'null' 'jn(1;2), yn(1;2), pow(2;10), atan2(1;1)'
jq_probe '"true"' 'toboolean'
jq_probe 'false' 'toboolean'
jq_probe '[1,2]' 'format("csv"), format("json"), format("text")'
jq_probe '[1,2,3]' 'bsearch(2), bsearch(2.5), bsearch(0), bsearch(9)'
jq_probe '[]' 'bsearch(1)'

# `type_probe` covers the refusals these new builtins owe.
type_probe '{"a":1}' 'toboolean'
type_probe '"x"'     'toboolean'
type_probe '0.5'     'bsearch(1)'
type_probe '0.5'     'format("csv")'
type_probe '{"a":1}' 'trimstr("x")'
type_probe '"x"'     'modulemeta'

# ── jq: TYPE errors — the other half of "never silently reinterpreted" ───────
# Every one of these is an IN-subset construct applied to the wrong type. jq
# raises on all of them (the probe checks that, so none of these refusals is one
# arb invented). Before this wave arb answered every single one with exit 0:
# `null` for an index, the raw line for an iterate or a slice, the line's
# character count for `length` on a boolean, `false` for `has` on a string.
# A wrong answer that looks like an answer is the worst shape a gap can take —
# nothing in the output says the query did not do what it said.
type_probe 'null'                        '.[]'
type_probe '3'                           '.[]'
type_probe '"abc"'                       '.[]'
type_probe 'true'                        '.[]'
type_probe '{"a":1}'                     '.[0]'
type_probe '[1,2]'                       '.["a"]'
type_probe '"hello"'                     '.[1]'
type_probe '3'                           '.a'
type_probe '"s"'                         '.a'
type_probe 'true'                        '.a'
type_probe '[1,2]'                       '.a'
type_probe '[1,2]'                       '.a.b'
type_probe '3'                           '.[1:2]'
type_probe 'true'                        '.[1:2]'
type_probe '{"a":1}'                     '.[1:2]'
type_probe 'null'                        '. | add'
type_probe '3'                           '. | add'
type_probe '[1,"a"]'                     '. | add'
type_probe 'true'                        '. | length'
type_probe 'null'                        '. | keys'
type_probe '3'                           '. | keys'
type_probe '"s"'                         '. | keys'
type_probe 'null'                        '. | to_entries'
type_probe '3'                           '. | to_entries'
type_probe 'null'                        '. | flatten'
type_probe '3'                           '. | flatten'
type_probe 'null'                        'map(.)'
type_probe '3'                           'map(.)'
type_probe '"s"'                         'map(.)'
type_probe '"s"'                         'has("a")'
type_probe '3'                           'has("a")'
type_probe '[1]'                         'has("a")'
type_probe '{"a":1}'                     'has(0)'
# Division and remainder by zero. jq refuses BOTH by name; arb answered `0` and
# `null`. `% 0.5` counts too — jq truncates the divisor to an integer first, so
# a fractional divisor below 1 IS a zero divisor.
type_probe '{"n":6}'                     '.n / 0'
type_probe '{"n":6}'                     '.n % 0'
type_probe '{"n":7}'                     '.n % 0.5'
# Mixed-type arithmetic between two fields.
type_probe '{"a":"x","b":3}'             '.a - .b'
type_probe '{"a":[1],"b":3}'             '.a + .b'
type_probe '{"a":true,"b":3}'            '.a * .b'
type_probe '{"a":{"x":1},"b":3}'         '.a / .b'
# A top-level JSON STRING now renders raw (see the passthrough section above), and
# these pin that the unquote did NOT leak into the stages that must still refuse.
# If the rendering were applied mid-pipeline instead of at the end, each of these
# would receive a bare `abc` — a non-JSON line, the one input the type checks
# cannot refuse — and would answer with exit 0 instead of raising.
type_probe '"abc"'                       '. | keys'
type_probe '"abc"'                       '. | add'
type_probe '"abc"'                       '. | to_entries'
type_probe '"abc"'                       '. | flatten'
type_probe '"abc"'                       'map(.)'
type_probe '"abc"'                       '.[]'
type_probe '"abc"'                       '.a'
type_probe '"abc"'                       'has("a")'
type_probe '"abc"'                       'select(.) | keys'
# Iterating THROUGH a path onto the wrong type (`.a[].b` where an element is a
# scalar) — the mid-path iterate above, on data that breaks it.
type_probe '{"a":[{"b":1}]}'             '.a[].b.c'
type_probe '{"a":[1,2]}'                 '.a[].b'
type_probe '{"a":3}'                     '.a[]'

# ── the NON-JSON line: SPEC §8's text carve-out, which had no probes ────────
# See `text_probe` above for why there is no oracle here and where EXPECTED comes
# from. SPEC §8, verbatim: "a path yields `null`, an iterate/slice passes the line
# through, and an EXPRESSION sees the line as jq's string — `. * 2` over a line
# reading `abc` is `abcabc`."
#
# This is the half of the value model that makes arb a line stream rather than a
# jq clone, it is stated in three clauses, and NOTHING measured it. A change to
# the jq value path could have silently turned any of these into a hard error —
# which for a text stream would break every non-JSON pipeline arb exists to serve.
# rule 1: a path yields null
text_probe 'abc'          '.a'          'null'
text_probe 'abc'          '.a.b'        'null'
text_probe 'hello world'  '.foo'        'null'
text_probe 'abc'          '.["k"]'      'null'
text_probe 'abc'          '.[0]'        'null'
# rule 2: an iterate/slice passes the line through
text_probe 'abc'          '.[]'         'abc'
text_probe 'abc'          '.[1:2]'      'abc'
text_probe 'hello'        '.[:2]'       'hello'
# rule 3: an expression sees the line as jq's string
text_probe 'abc'          '. * 2'       'abcabc'
text_probe 'abc'          '. + "d"'     'abcd'
text_probe 'abc'          '.'           'abc'
text_probe 'abc'          '. == "abc"'  'true'
text_probe 'abc'          '. < "abd"'   'true'
text_probe 'abc'          'select(. == "abc")' 'abc'
text_probe 'abc'          'select(.)'   'abc'
# `length` on a text line is the native verb's character count, which SPEC §8's
# spelling table records as the deliberate difference from jq's strict `length`.
text_probe 'abc'          'length'      '3'
text_probe 'abc'          '. | length'  '3'

# ── xpath / css ─────────────────────────────────────────────────────────────
XPF=$(mktemp -t arbxp).html
cat >"$XPF" <<'EOF'
<html><body>
<div class="card"><h2>Title</h2><span>inner</span><a href="/x">X</a></div>
<div class="other"><h2>Second</h2><a href="/z" rel="nf">Z</a></div>
<a href="/y">Y</a>
<ul><li>one</li><li>two</li><li>three</li></ul>
</body></html>
EOF
# Element selection and text extraction.
xp_probe "$XPF" '//a/text()'
xp_probe "$XPF" '//h2/text()'
xp_probe "$XPF" '//li/text()'
xp_probe "$XPF" '//span/text()'
xp_probe "$XPF" '//a'
xp_probe "$XPF" '//h2'
# Descendant `//`, child `/`, and a rooted absolute path.
xp_probe "$XPF" '//div/h2/text()'
xp_probe "$XPF" '//div//a/text()'
xp_probe "$XPF" '/html/body/ul/li/text()'
xp_probe "$XPF" '/html/body/div/h2/text()'
# Attribute selection (reference normalized from node to value — see header).
xp_probe "$XPF" '//a/@href'
xp_probe "$XPF" '//div//a/@href'
xp_probe "$XPF" '//a/@rel'
# Predicates: existence, equality (BOTH quote styles — XPath accepts either), and
# contains(). `[@class="card"]` used to be split by the lexer at the double quote
# and rejected as unquoted; it is as legal as the single-quoted form.
xp_probe "$XPF" '//div[@class]//span/text()'
xp_probe "$XPF" '//a[@href]/text()'
xp_probe "$XPF" '//a[@rel]/text()'
xp_probe "$XPF" "//div[@class='card']/h2/text()"
xp_probe "$XPF" '//div[@class="card"]/h2/text()'
xp_probe "$XPF" "//div[@class='other']//a/@href"
xp_probe "$XPF" "//a[contains(@href,'x')]/text()"
xp_probe "$XPF" "//div[contains(@class,'card')]/h2/text()"
# Union — SPEC §8's prose says "no union", but the engine implements it and it
# agrees with xmllint, so the probe pins the behaviour and the prose was corrected.
xp_probe "$XPF" '//h2|//li'

# ── xpath: the constructs the containment probe found missing ───────────────
# These are XPath 1.0 that SPEC §8 says must be "a hard error … never silently
# reinterpreted". Two of them are not: `or` and a chained predicate answer with
# exit 0 and an EMPTY selection where XPath selects nodes, and a ROOTED path
# answers with a non-empty node set where XPath selects nothing. A wrong answer
# that looks like an answer is the failure this harness exists to catch, so they
# are byte-diffed against xmllint rather than being asked only to exit non-zero.
xp_probe "$XPF" "//a[@href='/x']/text()"
xp_probe "$XPF" "//a[@href='/z' and @rel='nf']/text()"
xp_probe "$XPF" "//div[@class='card' or @class='other']/h2/text()"
xp_probe "$XPF" "//div[@class='other'][@class='other']/h2/text()"
xp_probe "$XPF" '/div/h2/text()'
xp_probe "$XPF" '/li/text()'
xp_probe "$XPF" '//a[not(@rel)]/text()'
xp_probe "$XPF" '//li[last()]/text()'
# Predicate + descendant + accessor in one path, both quote styles on
# `contains()`, a three-branch union, and a rooted path with a `//` in the
# middle. Each combines constructs the corpus only ever probed in isolation.
xp_probe "$XPF" "//div[@class='card']//a/text()"
xp_probe "$XPF" '//div[@class="other"]/h2/text()'
xp_probe "$XPF" '//a[contains(@href,"x")]/text()'
xp_probe "$XPF" "//a[@rel='nf']/text()"
xp_probe "$XPF" "//div[@class='card']//a/@href"
xp_probe "$XPF" '//h2|//li|//span'
xp_probe "$XPF" '/html/body//a/@href'
xp_probe "$XPF" '//div/span/text()'
xp_probe "$XPF" '//ul//li/text()'
xp_probe "$XPF" '//div[@class]/h2/text()'
xp_probe "$XPF" '//li'
xp_probe "$XPF" '//span'

# These SEVENTEEN were the `xp!` list: each asserted that arb REFUSES an XPath
# construct, and each is now an `xp_probe` against xmllint on the same input.
#
# That is the measurement moving, not a change to it. The refusal contract was
# true only because the xpath front-end compiled XPath-shaped syntax to a CSS
# selector, which has no spelling for an axis, a function or a positional
# predicate — so the honest thing was to refuse them. arb now runs a real XPath
# 1.0 engine, so asserting "arb refuses this" would assert something FALSE, and
# a probe that encodes a retired contract measures nothing. Same move, and same
# reasoning, as the 99 `err_probe`s in the jq wave (`d3fb2c880f`).
#
# Every converted line asserts strictly MORE than the one it replaces. "arb
# exits non-zero" is satisfied by any error at all, including the wrong one;
# "arb's stdout equals xmllint's, byte for byte" is not.
xp_probe "$XPF" '//a[1]'
xp_probe "$XPF" '//a[position()=1]'
xp_probe "$XPF" '//a/../span'
xp_probe "$XPF" '//a[text()="X"]'
xp_probe "$XPF" '//a/@*'
xp_probe "$XPF" '//a[last()]'
xp_probe "$XPF" 'ancestor::div'
xp_probe "$XPF" '//a[@href][2]'
xp_probe "$XPF" '//*'
xp_probe "$XPF" 'count(//a)'
xp_probe "$XPF" '//@href'
xp_probe "$XPF" '//a[@href and @rel]'
xp_probe "$XPF" '//a[@href!="/x"]'
xp_probe "$XPF" '//a/following-sibling::span'
xp_probe "$XPF" '//a[contains(text(),"X")]'
xp_probe "$XPF" '//a[not(@rel)]'
xp_probe "$XPF" 'normalize-space(//a)'

# ── css leg: `sel { CSS }` ──────────────────────────────────────────────────
# The README's headline claim is a jq/xpath/CSS/yq superset, and the css leg had
# ZERO probes — a whole third of the claim scored by nothing at all, which is a
# worse state than the yq leg, because yq at least REPORTS itself as unverified.
#
# There is no css tool on this machine, so each probe carries the XPath that
# selects the same elements and the reference is xmllint on that path. That is a
# translation, so it is stated per probe and kept trivial: only tag, `.class`,
# and descendant combinators appear, whose XPath equivalents are exact.
#
# `sel` emits an element's TEXT CONTENT, so every probe picks elements with a
# single text child, where the text content and xmllint's `text()` node are the
# same string. That keeps the comparison about which elements were SELECTED — a
# selector matching the wrong element still shows up as different text.
css_probe() {
    local file="$1" css="$2" xp="$3" a b
    command -v xmllint >/dev/null || { skip=$((skip + 1)); return; }
    a=$("$ARB" -e "out { in.html; sel { $css } }" <"$file" 2>&1)
    b=$(xmllint --html --xpath "$xp" "$file" 2>/dev/null)
    if [ "$a" = "$b" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   css  %-22s (= %s)\n' "$css" "$xp"
    else
        fail=$((fail + 1))
        fails+=("css  $css   (xpath equivalent: $xp)"$'\n'"       arb    : $(printf %s "$a" | tr '\n' '|')"$'\n'"       xmllint: $(printf %s "$b" | tr '\n' '|')")
        [ "$QUIET" = 1 ] || printf 'DIFF css  %-22s (= %s)\n       arb    : %s\n       xmllint: %s\n' \
            "$css" "$xp" "$(printf %s "$a" | tr '\n' '|')" "$(printf %s "$b" | tr '\n' '|')"
    fi
}
# css_bad CSS — a MALFORMED selector must be REFUSED, not answered.
#
# `Selector::parse` failure used to map to an empty result, so a selector no CSS
# engine accepts was indistinguishable from one that legitimately matches
# nothing — the same silent-answer failure the xpath leg had, and the one SPEC §8
# rules out. Every selector below is rejected by `scraper` (which implements
# Selectors 3/4 via `cssparser`), so answering ANYTHING for one is a bug.
css_bad() {
    local css="$1"
    if "$ARB" -e "out { in.html; sel { $css } }" </dev/null >/dev/null 2>&1; then
        fail=$((fail + 1))
        fails+=("css! $css — MALFORMED but accepted silently")
        [ "$QUIET" = 1 ] || printf 'DIFF css! %-22s accepted silently\n' "$css"
    else
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   css! %-22s (refused)\n' "$css"
    fi
}
css_bad 'a[href=/x]'
css_bad 'div..card'
css_bad 'a[[href]]'
css_bad '>'
css_bad 'a:::hover'

# An attribute value in DOUBLE quotes. CSS accepts `'` and `"` alike, but arb's
# command lexer turns `"…"` into a string ARG and the reconstruction dropped the
# quotes, so `a[href="/x"]` reached `Selector::parse` as `a[href=/x]` — not a
# legal selector, and therefore (before the fix above) an empty answer. Both
# quote styles are probed here so neither can regress alone.
css_probe "$XPF" 'a[href="/x"]'   "//a[@href='/x']/text()"
css_probe "$XPF" "a[href='/x']"   "//a[@href='/x']/text()"
css_probe "$XPF" 'a[rel="nf"]'    "//a[@rel='nf']/text()"
css_probe "$XPF" 'div[class="card"] h2' "//div[@class='card']//h2/text()"
css_probe "$XPF" 'a[href^="/"]'   '//a[@href]/text()'

# ── the css leg's CONTAINMENT probe ─────────────────────────────────────────
#
# jq, xpath and yq each have one; css had only SAMPLES, which measure what was
# thought to write down rather than what the reference defines.
#
# One honest difference from the other three, stated because it weakens the
# oracle: jq enumerates itself (`jq -rn builtins`), yq's lexer refuses a name it
# does not define, and xmllint refuses an expression libxml2 cannot parse — so
# those three probes cannot charge arb for something the reference lacks. There
# is no CSS tool on this machine, so this list is taken by hand from the
# Selectors Level 3 Recommendation (w3.org/TR/css3-selectors/): the four
# combinators of §8, the seven attribute operators of §6.3, and the structural
# pseudo-classes of §6.6.5. It is therefore an enumeration of the SPEC, not of a
# running reference, and it is only as complete as this list.
#
# What each entry asserts is that arb ACCEPTS the selector. Whether it selects
# the right nodes is the `css_probe` rows below, which byte-diff against the
# equivalent XPath through xmllint.
CSS_SURFACE='p * .card #main div|p div>p p+p p~p
[href] [href="/x"] [rel~="nf"] [lang|="en"] [href^="/"] [href$="x"] [title*="lo"]
p:first-child p:last-child p:nth-child(2) p:nth-of-type(2) p:not(.a) span:empty
:root em:only-child h2,a'
css_superset_probe() {
    local missing='' sel out
    for sel in $CSS_SURFACE; do
        # `|` stands in for the space in a descendant combinator, which the
        # word-split list cannot carry literally.
        sel=$(printf '%s' "$sel" | tr '|' ' ')
        out=$("$ARB" -e "out { in.html; sel { $sel } }" <"$CSSF" 2>&1)
        if [ $? -ne 0 ]; then
            missing="$missing $sel"
        fi
        case "$out" in *'not a valid CSS selector'*) missing="$missing $sel" ;; esac
    done
    report_containment css "css selector" "$missing"
}

# A fixture with the structure the surface above needs: siblings to count, an
# empty element, an `|=` language value, and an attribute value with a space.
CSSF=$(mktemp -t arbcss).html
cat >"$CSSF" <<'EOF'
<html><body><div id="main" class="card wide" lang="en"><h2>T</h2><p class="a">1</p><p class="b">2</p><p class="c">3</p><a href="/x" rel="nf" title="hello world">X</a></div><div class="other"><span></span><em>e</em></div></body></html>
EOF
css_superset_probe

# The behavioural half of the surface: every construct above that has an exact
# XPath equivalent, byte-diffed against xmllint on the same document. The
# translation is stated per row and kept exact — a structural pseudo-class
# becomes the sibling count it is defined as.
css_probe "$CSSF" 'p'                '//p/text()'
css_probe "$CSSF" '.card h2'         "//div[@class='card wide']//h2/text()"
css_probe "$CSSF" '#main h2'         "//div[@id='main']//h2/text()"
css_probe "$CSSF" 'div > p'          '//div/p/text()'
css_probe "$CSSF" 'p + p'            '//p/following-sibling::p[1]/text()'
css_probe "$CSSF" '[href]'           '//*[@href]/text()'
css_probe "$CSSF" '[href="/x"]'      "//*[@href='/x']/text()"
css_probe "$CSSF" '[rel~="nf"]'      "//*[@rel='nf']/text()"
css_probe "$CSSF" '[href^="/"]'      "//*[starts-with(@href,'/')]/text()"
css_probe "$CSSF" '[title*="lo w"]'  "//*[contains(@title,'lo w')]/text()"
css_probe "$CSSF" 'p:first-child'    '//p[not(preceding-sibling::*)]/text()'
css_probe "$CSSF" 'p:last-child'     '//p[not(following-sibling::*)]/text()'
css_probe "$CSSF" 'p:nth-child(2)'   '//p[count(preceding-sibling::*)=1]/text()'
css_probe "$CSSF" 'p:not(.a)'        "//p[@class!='a']/text()"
css_probe "$CSSF" 'h2, a'            '//h2/text()|//a/text()'
rm -f "$CSSF"

css_probe "$XPF" 'h2'          '//h2/text()'
css_probe "$XPF" 'a'           '//a/text()'
css_probe "$XPF" 'li'          '//li/text()'
css_probe "$XPF" 'span'        '//span/text()'
css_probe "$XPF" 'div.card h2' "//div[@class='card']//h2/text()"
css_probe "$XPF" 'div.other a' "//div[@class='other']//a/text()"
css_probe "$XPF" 'ul li'       '//ul//li/text()'
css_probe "$XPF" 'div a'       '//div//a/text()'
css_probe "$XPF" 'div h2'      '//div//h2/text()'
css_probe "$XPF" 'body a'      '//body//a/text()'
# `sel { … }` is the spelling SPEC §8 and the README both print, and it was the
# one that did NOT compile — a braced argument lexes to a command BLOCK, whose
# text `sel` dropped, so the documented form failed with "expected a CSS
# selector" while only the undocumented `sel a` worked. Both spellings are
# probed now so the documented one cannot silently rot again.
# The css leg reaches well past the `tag` / `.class` / descendant forms probed
# above — the child combinator, attribute selectors, a bare class, a selector
# group and the structural pseudo-classes all compile and answer. None of it was
# measured, and none of it is named in SPEC §8, which prints only `sel { CSS }`
# and one `div.card h2` example. Unmeasured support is what round 1 found the css
# leg in overall, so the working forms are pinned here before they can rot.
#
# `.card` is translated as `//div[@class='card']` rather than `//*[…]`: only the
# div carries that class in this fixture, so the two select the same node set, and
# `//*` is out of arb's documented subset (it refuses it — see the xp! list).
css_probe "$XPF" 'div > h2'      '//div/h2/text()'
css_probe "$XPF" 'div.card > h2' "//div[@class='card']/h2/text()"
css_probe "$XPF" '.card h2'      "//div[@class='card']//h2/text()"
css_probe "$XPF" '.other a'      "//div[@class='other']//a/text()"
css_probe "$XPF" 'a[href]'       '//a[@href]/text()'
css_probe "$XPF" 'a[rel]'        '//a[@rel]/text()'
css_probe "$XPF" 'h2, li'        '//h2/text()|//li/text()'
css_probe "$XPF" 'li:first-child' '//li[1]/text()'
css_probe "$XPF" 'ul > li'       '//ul/li/text()'
for pair in 'a|//a/text()' 'div.card h2|//div[@class="card"]//h2/text()'; do
    css="${pair%%|*}"; xp="${pair#*|}"
    a=$("$ARB" -e "out { in.html; sel $css }" <"$XPF" 2>&1)
    b=$(xmllint --html --xpath "$xp" "$XPF" 2>/dev/null)
    if [ "$a" = "$b" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   css! %-22s (unbraced spelling)\n' "$css"
    else
        fail=$((fail + 1))
        fails+=("css! sel $css (unbraced) — arb: $(printf %s "$a" | tr '\n' '|')")
        [ "$QUIET" = 1 ] || printf 'DIFF css! %-22s arb: %s\n' "$css" "$(printf %s "$a" | tr '\n' '|')"
    fi
done
# ── xpath: the STANDALONE `@attr` step ──────────────────────────────────────
# SPEC §8 claims it by name and explains it is arb's own reading, not XPath's:
# "plus a standalone `@attr` step, which is arb's line-stream continuation
# (`//a; @href`) rather than XPath's `attribute::` axis from the document node."
# A construct the SPEC singles out for its own sentence had no probe. It is a
# separate code path from `//a/@href` — two body commands rather than one path —
# so the two spellings could drift apart without anything noticing.
sa_probe() {
    local arb_body="$1" xp="$2" a b
    command -v xmllint >/dev/null || { skip=$((skip + 1)); return; }
    a=$("$ARB" -e "out { in.html; $arb_body }" <"$XPF" 2>&1)
    b=$(xmllint --html --xpath "$xp" "$XPF" 2>/dev/null | perl -ne 'print "$1\n" if /="(.*)"\s*$/')
    if [ "$a" = "$b" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   xp@  %-24s (= %s)\n' "$arb_body" "$xp"
    else
        fail=$((fail + 1))
        fails+=("xp@  $arb_body   (xpath equivalent: $xp)"$'\n'"       arb    : $(printf %s "$a" | tr '\n' '|')"$'\n'"       xmllint: $(printf %s "$b" | tr '\n' '|')")
        [ "$QUIET" = 1 ] || printf 'DIFF xp@  %-24s\n       arb    : %s\n       xmllint: %s\n' \
            "$arb_body" "$(printf %s "$a" | tr '\n' '|')" "$(printf %s "$b" | tr '\n' '|')"
    fi
}
sa_probe '//a; @href'            '//a/@href'
sa_probe '//div; @class'         '//div/@class'
sa_probe '//a; @rel'             '//a/@rel'
sa_probe 'find a; attr href'     '//a/@href'
sa_probe "//div[@class='card']//a; @href" "//div[@class='card']//a/@href"

# ── css: the `#id` selector, in its own fixture ─────────────────────────────
# A SEPARATE file so `$XPF` stays byte-identical for every probe above — adding an
# `id=` to a shared fixture would move the element serialization those probes diff.
IDF=$(mktemp -t arbid).html
cat >"$IDF" <<'EOF'
<html><body><div id="main"><p>Hello</p></div><p id="two">Bye</p></body></html>
EOF
# The forms that WORK: an id used as a non-leading part of a compound selector,
# and the unbraced spelling.
idw() {
    local body="$1" xp="$2" a b
    command -v xmllint >/dev/null || { skip=$((skip + 1)); return; }
    a=$("$ARB" -e "out { in.html; $body }" <"$IDF" 2>&1)
    b=$(xmllint --html --xpath "$xp" "$IDF" 2>/dev/null)
    if [ "$a" = "$b" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   id   %-26s (= %s)\n' "$body" "$xp"
    else
        fail=$((fail + 1))
        fails+=("id   $body   (xpath equivalent: $xp)"$'\n'"       arb    : $(printf %s "$a" | tr '\n' '|')"$'\n'"       xmllint: $(printf %s "$b" | tr '\n' '|')")
        [ "$QUIET" = 1 ] || printf 'DIFF id   %-26s\n       arb    : %s\n       xmllint: %s\n' \
            "$body" "$(printf %s "$a" | tr '\n' '|')" "$(printf %s "$b" | tr '\n' '|')"
    fi
}
idw 'sel { div#main p }'  '//div[@id="main"]//p/text()'
idw 'sel #main'           '//div[@id="main"]//p/text()'
idw 'sel #two'            '//p[@id="two"]/text()'
idw '//div[@id="main"]//p/text()' '//div[@id="main"]//p/text()'
# The form that did NOT, until round 3: a LEADING `#` inside the braced spelling.
# `#` opens a COMMENT in arb's lexer, so `sel { #main }` lexed to an empty block,
# `block_text` reconstructed "", and the verb reported "expected a CSS selector"
# (exit 1) for the spelling SPEC §8 and the README both print.
#
# The note here used to argue the fix needed either "making `#` non-comment
# inside a block (which would break real comments in every `source { … }` body)"
# or a raw source span on `Arg::Block`. Both were false dichotomies: the comment
# rule is scoped to the brace, not to blocks as a class. `sel` is the ONE brace
# in the language whose contents are a CSS selector rather than commands, so only
# that one is re-lexed with the rule off. Every other brace keeps `#` as a
# comment, which `hash_still_comments_in_command_blocks` pins.
idw 'sel { #main }'       '//div[@id="main"]//p/text()'
rm -f "$IDF"
xpath_superset_probe
rm -f "$XPF"

# ── yq leg ──────────────────────────────────────────────────────────────────
# The README claims a `yq` superset, and until now that leg was scored by
# NOTHING — the harness only printed a note saying so. These run arb's YAML
# source against mikefarah/yq over the same document.
YQ_FIXTURE=$(mktemp -t arb_yq_XXXXXX)
cat >"$YQ_FIXTURE" <<'YAMLEOF'
name: widget
count: 3
ratio: 1.50
enabled: true
missing: null
tags:
  - alpha
  - bravo
nested:
  k: v
  n: 7
  deep:
    z: [1, 2]
items:
  - id: 1
    label: one
    v: 10
  - id: 2
    label: two
    v: 5
YAMLEOF

yq_probe "$YQ_FIXTURE" '.'
yq_probe "$YQ_FIXTURE" '.name'
yq_probe "$YQ_FIXTURE" '.count'
yq_probe "$YQ_FIXTURE" '.ratio'
yq_probe "$YQ_FIXTURE" '.enabled'
yq_probe "$YQ_FIXTURE" '.missing'
yq_probe "$YQ_FIXTURE" '.absent'
yq_probe "$YQ_FIXTURE" '.tags'
yq_probe "$YQ_FIXTURE" '.tags[0]'
yq_probe "$YQ_FIXTURE" '.tags[-1]'
yq_probe "$YQ_FIXTURE" '.tags[]'
yq_probe "$YQ_FIXTURE" '.nested'
yq_probe "$YQ_FIXTURE" '.nested.k'
yq_probe "$YQ_FIXTURE" '.nested.deep.z'
yq_probe "$YQ_FIXTURE" '.nested.deep.z[1]'
yq_probe "$YQ_FIXTURE" '.items'
yq_probe "$YQ_FIXTURE" '.items[0]'
yq_probe "$YQ_FIXTURE" '.items[].id'
yq_probe "$YQ_FIXTURE" '.items[].label'
yq_probe "$YQ_FIXTURE" '[.items[].v]'
yq_probe "$YQ_FIXTURE" '.items[] | select(.v > 6)'
yq_probe "$YQ_FIXTURE" '.items[] | select(.v > 6) | .label'
yq_probe "$YQ_FIXTURE" '.items | length'
yq_probe "$YQ_FIXTURE" '.tags | length'
yq_probe "$YQ_FIXTURE" '.items | map(.v)'
yq_probe "$YQ_FIXTURE" '.items | map(.id) | length'
# `keys` is the ONE verb where the two references contradict each other: jq's
# `keys` SORTS ("keys_unsorted" is the unsorted one), yq's preserves document
# order. No single behaviour can match both, so it is not probed against yq —
# arb follows jq (SPEC §8), which the jq leg already checks. Probing it here
# would report a divergence that says nothing about arb.
yq_probe "$YQ_FIXTURE" '.nested | to_entries | length'
yq_probe "$YQ_FIXTURE" '.items[0] | length'
yq_probe "$YQ_FIXTURE" '.items[] | .v'
yq_probe "$YQ_FIXTURE" '.items[1:2]'
yq_probe "$YQ_FIXTURE" '.tags[0:1]'
yq_probe "$YQ_FIXTURE" '.count + 1'
yq_probe "$YQ_FIXTURE" '.items | to_entries | length'
yq_probe "$YQ_FIXTURE" '.nested.deep'
yq_probe "$YQ_FIXTURE" '.items[] | {"id": .id}'
yq_superset_probe
rm -f "$YQ_FIXTURE"

# yq's postfix `ireduce`, in yq's own spelling. `yq_probe` runs the SAME text
# through both engines, so this asserts the grammar extension end to end.
YQ_RED=$(mktemp -t arb_yqr_XXXXXX)
printf 'nums: [1, 2, 3]\n' >"$YQ_RED"
yq_probe "$YQ_RED" '.nums | .[] as $item ireduce (0; . + $item)'
yq_probe "$YQ_RED" '.nums | .[] as $item ireduce (1; . * $item)'
rm -f "$YQ_RED"

# `filename` answers the PATH when the spec names one with `< FILE`, and `-` for
# a pipe. `yq_probe` always pipes, so the named-file reading needs its own probe.
yq_filename_probe() {
    local f a b
    command -v yq >/dev/null || { skip=$((skip + 1)); return; }
    f=$(mktemp -t arb_yqf_XXXXXX)
    printf 'a: 1\n' >"$f"
    a=$("$ARB" -e "< \"$f\"; out { in.yaml; filename }" 2>&1)
    b=$(yq 'filename' "$f" 2>/dev/null)
    if [ "$a" = "$b" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   yqf  filename names the file the spec read\n'
    else
        fail=$((fail + 1))
        fails+=("yqf  filename on a named file"$'\n'"       arb: $a"$'\n'"       yq : $b")
        [ "$QUIET" = 1 ] || printf 'DIFF yqf  filename\n       arb: %s\n       yq : %s\n' "$a" "$b"
    fi
    rm -f "$f"
}
yq_filename_probe
# ── the yq NODE-METADATA leg ────────────────────────────────────────────────
#
# Everything above this point compares filters jq could also spell. What follows
# is the half of yq that jq has no equivalent for: the operators that read and
# write a NODE's metadata, the encode/decode family, and the round trip that is
# the reason all of them exist.
#
# `yq_superset_probe` scores those by NAME. These score them by ANSWER, which is
# the stronger of the two and the only one that can catch an operator that exists
# and is wrong.
YQ_META=$(mktemp -t arb_yqm_XXXXXX)
cat >"$YQ_META" <<'YAMLEOF'
# leading doc comment
name: widget # trailing name
base: &anc
  k: v
  n: 7
use: *anc
merged:
  <<: *anc
  extra: 1
quoted: "dq"
single: 'sq'
lit: |
  line1
  line2
flow: {a: 1, b: [2, 3]}
tagged: !!str 123
custom: !mytag hello
empty:
explicit: null
tilde: ~
hexed: 0x10
padded: 007
ratio: 1.50
uni: "héllo → ✓"
# key head
listed:
  # item head
  - x # item line
  - y
YAMLEOF

# The metadata accessors, each against yq's own answer for the same node.
yq_probe "$YQ_META" '.base | anchor'
yq_probe "$YQ_META" '.use | alias'
yq_probe "$YQ_META" '.use | kind'
yq_probe "$YQ_META" '.base | kind'
yq_probe "$YQ_META" '.name | kind'
yq_probe "$YQ_META" 'kind'
yq_probe "$YQ_META" '.base | tag'
yq_probe "$YQ_META" '.quoted | tag'
yq_probe "$YQ_META" '.tagged | tag'
yq_probe "$YQ_META" '.custom | tag'
yq_probe "$YQ_META" '.name | tag'
yq_probe "$YQ_META" '.ratio | tag'
yq_probe "$YQ_META" '.empty | tag'
yq_probe "$YQ_META" '.listed | tag'
yq_probe "$YQ_META" '.quoted | style'
yq_probe "$YQ_META" '.single | style'
yq_probe "$YQ_META" '.lit | style'
yq_probe "$YQ_META" '.flow | style'
yq_probe "$YQ_META" '.name | style'
yq_probe "$YQ_META" '.name | line_comment'
yq_probe "$YQ_META" '.name | lineComment'
yq_probe "$YQ_META" '.listed[0] | line_comment'
yq_probe "$YQ_META" '.listed[0] | head_comment'
yq_probe "$YQ_META" '.listed | key | head_comment'
yq_probe "$YQ_META" 'head_comment'
yq_probe "$YQ_META" 'headComment'
yq_probe "$YQ_META" '.name | key'
yq_probe "$YQ_META" '.name | is_key'
yq_probe "$YQ_META" '.name | key | is_key'
yq_probe "$YQ_META" '.name | path'
yq_probe "$YQ_META" '.base.k | path'
yq_probe "$YQ_META" '.listed[1] | path'
yq_probe "$YQ_META" '.name | line'
yq_probe "$YQ_META" '.name | column'
yq_probe "$YQ_META" '.base | line'
yq_probe "$YQ_META" '.lit | line'
yq_probe "$YQ_META" 'document_index'
yq_probe "$YQ_META" 'documentIndex'
yq_probe "$YQ_META" 'di'
yq_probe "$YQ_META" '.name | fileIndex'
yq_probe "$YQ_META" '.base | parent | length'
yq_probe "$YQ_META" '.base | parent | to_entries | .[0].key'
yq_probe "$YQ_META" '.merged'
yq_probe "$YQ_META" '.merged.k'
yq_probe "$YQ_META" '.use'
yq_probe "$YQ_META" '.hexed'
yq_probe "$YQ_META" '.padded'
yq_probe "$YQ_META" '.ratio'
yq_probe "$YQ_META" '.uni'
yq_probe "$YQ_META" '.tagged'
yq_probe "$YQ_META" '.custom'
yq_probe "$YQ_META" '.empty'
yq_probe "$YQ_META" '.explicit'
yq_probe "$YQ_META" '.tilde'
yq_probe "$YQ_META" '.lit'
yq_probe "$YQ_META" '.flow'
yq_probe "$YQ_META" '.flow.b'

# The reshaping and conversion group.
yq_probe "$YQ_META" 'pick(["name","quoted"])'
yq_probe "$YQ_META" 'omit(["name"]) | length'
yq_probe "$YQ_META" 'omit(["name"]) | to_entries | .[0].key'
yq_probe "$YQ_META" 'omit(["name","base"]) | to_entries | .[0].key'
yq_probe "$YQ_META" 'sort_keys(.) | to_entries | .[0].key'
yq_probe "$YQ_META" 'sort_keys(.) | to_entries | .[1].key'
yq_probe "$YQ_META" 'sortKeys(.) | to_entries | .[0].key'
yq_probe "$YQ_META" '.name | to_string'
yq_probe "$YQ_META" '.name | upcase'
yq_probe "$YQ_META" '.name | downcase'
yq_probe "$YQ_META" '.padded | to_string'
yq_probe "$YQ_META" 'with(.name; . = "z") | .name'
yq_probe "$YQ_META" '.listed | length'
yq_probe "$YQ_META" '.base | to_json(0)'
yq_probe "$YQ_META" '.flow | to_json(0)'
yq_probe "$YQ_META" '.listed | splitDoc | length'
yq_probe "$YQ_META" '.name | splitDoc'

# The metadata WRITERS. yq spells these as a postfix on a path
# (`.a anchor = "x"`); arb's grammar is jq's, so the same edit is
# `.a | anchor = "x"`, and the reference is asked in ITS spelling.
yq_write_probe() {
    local file="$1" arb_f="$2" yq_f="$3" a b
    command -v yq >/dev/null || { skip=$((skip + 1)); return; }
    a=$("$ARB" -e "out { in.yaml; $arb_f; out.yaml }" <"$file" 2>&1)
    b=$(yq $YQ_MERGE "$yq_f" "$file" 2>/dev/null)
    if [ "$a" = "$b" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   yqw  %s\n' "$arb_f"
    else
        fail=$((fail + 1))
        fails+=("yqw  $arb_f (yq: $yq_f)"$'\n'"       arb: $(printf %s "$a" | tr '\n' '|')"$'\n'"       yq : $(printf %s "$b" | tr '\n' '|')")
        [ "$QUIET" = 1 ] || printf 'DIFF yqw  %s\n       arb: %s\n       yq : %s\n' \
            "$arb_f" "$(printf %s "$a" | tr '\n' '|')" "$(printf %s "$b" | tr '\n' '|')"
    fi
}
YQ_SMALL=$(mktemp -t arb_yqs_XXXXXX)
printf 'a: 1\nb: two\n' >"$YQ_SMALL"
yq_write_probe "$YQ_SMALL" '.a |= (anchor = "x")'        '.a anchor = "x"'
yq_write_probe "$YQ_SMALL" '.a |= (tag = "!!str")'       '.a tag = "!!str"'
yq_write_probe "$YQ_SMALL" '.b |= (style = "double")'    '.b style = "double"'
yq_write_probe "$YQ_SMALL" '.b |= (style = "single")'    '.b style = "single"'
yq_write_probe "$YQ_SMALL" '.b |= (style = "literal")'   '.b style = "literal"'
yq_write_probe "$YQ_SMALL" '.a |= (line_comment = "hi")' '.a line_comment = "hi"'
yq_write_probe "$YQ_SMALL" '.a |= (lineComment = "hi")'  '.a lineComment = "hi"'
yq_write_probe "$YQ_SMALL" '.a |= (foot_comment = "f")'  '.a foot_comment = "f"'
# yq's OWN spelling, which the grammar accepts now: a metadata postfix on a path.
# Both columns are the same text, so these assert that arb answers yq's syntax
# with yq's answer rather than merely having an equivalent of its own.
yq_write_probe "$YQ_SMALL" '.a anchor = "x"'             '.a anchor = "x"'
yq_write_probe "$YQ_SMALL" '.a tag = "!!str"'            '.a tag = "!!str"'
yq_write_probe "$YQ_SMALL" '.b style = "double"'         '.b style = "double"'
yq_write_probe "$YQ_SMALL" '.b style = "single"'         '.b style = "single"'
yq_write_probe "$YQ_SMALL" '.a line_comment = "hi"'      '.a line_comment = "hi"'
yq_write_probe "$YQ_SMALL" '.a head_comment = "top"'     '.a head_comment = "top"'
rm -f "$YQ_SMALL"

# ── the round trip ──────────────────────────────────────────────────────────
#
# One fixture per feature the model has to carry, so a failure names WHICH one
# broke rather than reporting that "yaml" broke.
yq_rt_probe "$YQ_META" 'every feature at once'
rm -f "$YQ_META"

yq_rt_case() {
    local label="$1" f
    f=$(mktemp -t arb_yqrt_XXXXXX)
    cat >"$f"
    yq_rt_probe "$f" "$label"
    yq_fmt_probe "$f" props
    rm -f "$f"
}

yq_rt_case 'comments in every position' <<'YAMLEOF'
# doc head
a: 1 # a line
# b head
b: 2
# c head 1
# c head 2
c:
  # d head
  d: 4 # d line
  # e head

  e: 5
list:
  # item head
  - x # item line
  - y
# tail
YAMLEOF

yq_rt_case 'anchors, aliases and merge keys' <<'YAMLEOF'
defaults: &def
  retries: 3
  timeout: 30
prod:
  <<: *def
  timeout: 60
dev: *def
list: &l
  - one
  - two
copy: *l
YAMLEOF

yq_rt_case 'all six scalar styles' <<'YAMLEOF'
plain: hello
single: 'it''s here'
double: "a\ttab"
literal: |
  keep
  the breaks
folded: >-
  fold these
  two lines
empty:
YAMLEOF

yq_rt_case 'flow versus block collections' <<'YAMLEOF'
flowmap: {a: 1, b: 2}
flowseq: [1, 2, 3]
nested: {x: [1, {y: 2}]}
blockmap:
  a: 1
  b: 2
blockseq:
  - 1
  - 2
emptymap: {}
emptyseq: []
YAMLEOF

yq_rt_case 'tags' <<'YAMLEOF'
s: !!str 123
i: !!int 7
f: !!float 1.5
b: !!bool true
custom: !mytag payload
seq: !!seq
  - 1
YAMLEOF

yq_rt_case 'empty values and nulls' <<'YAMLEOF'
blank:
explicit: null
tilde: ~
emptystr: ""
zero: 0
false: false
YAMLEOF

yq_rt_case 'non-ASCII' <<'YAMLEOF'
greek: αβγ
arrows: "a → b"
emoji: 🚀
mixed: héllo wörld
key→: value
YAMLEOF

yq_rt_case 'number spellings' <<'YAMLEOF'
padded: 007
hex: 0x1F
octal: 0o17
float: 1.50
exp: 1e3
neg: -42
big: 12345678901234
YAMLEOF

# Multi-document streams get their own probe: `---` separation is what
# `splitDoc` relies on and what a per-document `document_index` is counted over.
yq_rt_case 'deeply nested anchors' <<'YAMLEOF'
outer: &o
  inner: &i
    deep: &d
      leaf: 1
    other: *d
  second: *i
top: *o
YAMLEOF

yq_rt_case 'aliases inside merge keys' <<'YAMLEOF'
base: &b
  a: 1
extra: &e
  b: 2
both:
  <<: [*b, *e]
  c: 3
single:
  <<: *b
  a: 9
YAMLEOF

yq_rt_case 'comments on sequence items versus mappings' <<'YAMLEOF'
# doc
seq:
  # head of item 0
  - one # line of item 0
  # head of item 1
  - two
  # foot of the seq

map:
  # head of key a
  a: 1 # line of a
  # foot of a

  b: 2
nested:
  - # head inside item
    k: v # line of k
YAMLEOF

yq_rt_case 'an explicit leading document marker' <<'YAMLEOF'
---
a: 1
---
b: 2
YAMLEOF

# ── shapes where the REFERENCE is not idempotent either ─────────────────────
#
# `yq_rt_probe` asks for the source file back, which is the property the claim
# names and is stricter than matching yq. Three shapes cannot be asked that,
# because `yq '.'` does not return them either — it collapses a multi-line flow
# collection onto one line, reorders `!!tag &anchor` to `&anchor !!tag`, and
# drops a trailing `...`.
#
# They are not dropped from the corpus for that. They are asserted against the
# REFERENCE instead, byte for byte and nothing normalized, so arb has to
# normalize the same way yq does rather than merely being allowed to differ from
# the source. A shape nobody checks is the only bad outcome here; asserting the
# weaker of two true properties is not.
yq_norm_probe() {
    local label="$1" f a b
    command -v yq >/dev/null || { skip=$((skip + 1)); return; }
    f=$(mktemp -t arb_yqn_XXXXXX)
    cat >"$f"
    a=$("$ARB" -e 'out { in.yaml; out.yaml }' <"$f" 2>&1)
    b=$(yq '.' "$f" 2>/dev/null)
    if [ "$a" = "$b" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   norm %s\n' "$label"
    else
        fail=$((fail + 1))
        fails+=("norm $label — arb normalizes differently from yq"$'\n'"       arb: $(printf %s "$a" | tr '\n' '|')"$'\n'"       yq : $(printf %s "$b" | tr '\n' '|')")
        [ "$QUIET" = 1 ] || printf 'DIFF norm %s\n       arb: %s\n       yq : %s\n' \
            "$label" "$(printf %s "$a" | tr '\n' '|')" "$(printf %s "$b" | tr '\n' '|')"
    fi
    rm -f "$f"
}

yq_norm_probe 'multi-line flow collections' <<'YAMLEOF'
wide: [
    1,
    2,
    3
  ]
obj: {
    a: 1,
    b: 2
  }
YAMLEOF

yq_norm_probe 'tags on collections rather than scalars' <<'YAMLEOF'
seq: !!seq
  - 1
  - 2
map: !!map
  a: 1
custom: !mytype
  k: v
anchored: !!map &am
  z: 9
YAMLEOF

yq_norm_probe 'a trailing document-end marker' <<'YAMLEOF'
---
a: 1
...
---
b: 2
...
YAMLEOF

YQ_MULTI=$(mktemp -t arb_yqmd_XXXXXX)
cat >"$YQ_MULTI" <<'YAMLEOF'
# first
a: 1
---
# second
b: 2
---
c:
  - 3
YAMLEOF
yq_rt_probe "$YQ_MULTI" 'multi-document stream'
yq_probe "$YQ_MULTI" 'document_index'
yq_probe "$YQ_MULTI" 'di'
rm -f "$YQ_MULTI"


if ! command -v yq >/dev/null; then
    skip=$((skip + 1))
    yq_note="yq NOT INSTALLED — the yq leg of the superset claim is UNVERIFIED"
else
    yq_note="yq $(yq --version 2>&1 | awk '{print $NF}') — the yq leg is probed above"
fi

echo
echo "── jq_parity summary ───────────────────────────────"
printf 'pass %d   diverged %d   skipped %d\n' "$pass" "$fail" "$skip"
printf 'oracle %s; %d probes ran (floor %d)\n' "$have_jq" "$((pass + fail))" "$MIN_PROBES"
echo "note: $yq_note"
if [ "$fail" -gt 0 ]; then
    echo
    echo "diverged probes:"
    for f in "${fails[@]}"; do printf '  %s\n' "$f"; done
fi
if [ "$((pass + fail))" -lt "$MIN_PROBES" ]; then
    echo
    echo "jq_parity: only $((pass + fail)) probes ran, below the floor of $MIN_PROBES." >&2
    echo "           A shrinking denominator is not a passing run. NOT a pass." >&2
    exit 3
fi
exit "$fail"
