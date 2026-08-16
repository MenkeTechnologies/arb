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
#   yq       — NOT INSTALLED on this machine. The yq leg of the superset claim is
#              reported as SKIPPED, never as passing.
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
# The remaining 5 are recorded, not hidden. `keys` is round 1's spelling
# collision, unchanged. `sel { #main }` is NEW: a LEADING `#` opens a COMMENT in
# arb's lexer, so the braced spelling SPEC §8 prints cannot express an id selector
# at all — round 1's `sel { div.card h2 }` bug in its last corner, needing either
# a lexer change that would break real comments or a raw source span on
# `Arg::Block`. The 3 whitespace probes are the measured COST of the passthrough
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

set -u
cd "$(dirname "$0")/.." || exit 2

ARB=./target/debug/arb
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
MIN_PROBES=564

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

# err_probe FILTER — the OTHER half of the contract. SPEC §8 says a construct
# outside the documented subset "is a hard error … never silently reinterpreted",
# so arb must exit NON-ZERO. There is no reference tool here: real jq ANSWERS
# these, and arb deliberately does not. What is being checked is that it refuses
# rather than guesses, which is why a silent pass is reported as a divergence.
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
    # Normalize ONLY when the path SELECTS attributes (a trailing `/@name` or a
    # bare `@name` step). An `@` inside a predicate (`//div[@class]//span`) still
    # selects elements, so its output must be compared unmodified.
    case "$xp" in
        *'/@'[A-Za-z_]* | '@'[A-Za-z_]*)
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

# ── jq: the hard-error half of the contract (SPEC §8) ───────────────────────
# Real jq answers every one of these. arb's documented subset does not include
# them, and SPEC §8 promises a hard error rather than a silent reinterpretation —
# so the only acceptable behaviour is a non-zero exit. These are the constructs a
# reader of the README would most plausibly reach for, which is exactly why an
# accidental silent answer here would be the most damaging kind of gap.
err_probe '.foo // 0'                      # alternative operator
err_probe '.foo?'                          # error suppression
err_probe '..'                             # recursive descent
err_probe '.a as $x | $x'                  # variable binding
err_probe 'reduce .[] as $x (0; . + $x)'   # reduce
err_probe 'foreach .[] as $x (0; .+$x; .)' # foreach
err_probe 'try .a catch 0'                 # try/catch
err_probe 'paths'
err_probe 'leaf_paths'
err_probe 'getpath(["a"])'
err_probe 'setpath(["a"];9)'
err_probe 'delpaths([["a"]])'
err_probe 'from_entries'
err_probe 'with_entries(.value += 1)'
err_probe 'group_by(.a)'
err_probe 'unique_by(.a)'
err_probe 'min_by(.a)'
err_probe 'max_by(.a)'
err_probe 'any'
err_probe 'all'
err_probe 'range(3)'
err_probe 'splits(",")'
err_probe 'sub("a";"b")'
err_probe 'gsub("(?<x>a)";"\(.x)")'
err_probe 'ascii_downcase'
err_probe 'env.HOME'
err_probe '$ENV.HOME'
err_probe 'input'
err_probe 'inputs'
err_probe '@base64'
err_probe '@csv'
err_probe '@tsv'
err_probe '@json'
err_probe 'first(.[])'
err_probe 'limit(2;.[])'
err_probe '.[] | not'
err_probe 'tostring'
err_probe 'tonumber'
err_probe '. as [$a] ?// {$a} | $a'         # optional destructuring
# Arithmetic against a whole OBJECT. jq refuses every one of these by name
# ("object and number cannot be divided"), and arb answers `null` with exit 0 —
# a silent reinterpretation of a construct that has no meaning, which SPEC §8
# rules out explicitly. This is the `select(.status == "ok")` shape again: not a
# wrong number, but an answer where there should be a refusal, so nothing in the
# output says the query did not do what it said. It is not `%`-specific — every
# arithmetic operator does it, so all five are probed rather than the one that
# happened to be under the microscope.
err_probe '. + 3'
err_probe '. - 3'
err_probe '. * 3'
err_probe '. / 3'
err_probe '. % 3'
# jq's CONSTRUCTORS and control flow. SPEC §8's out-of-subset list names builtins
# and operators but no SYNTAX form, so object construction, array construction,
# the comma operator, `if/then/else` and plain PARENTHESES were all unlisted and
# unprobed — while being the constructs a jq user reaches for soonest after the
# ones already covered. A silent answer from any of them would be the
# `select(.status == "ok")` shape again: a filter that quietly means something
# else. (`(.a + 3) * 2` refuses with `unknown verb \`(.a\``, because the body
# dispatcher routes only `.`/`select(`/`map(`/`has(` to the jq front-end — a hard
# error either way, which is what the contract requires.)
err_probe '{a: .a}'
err_probe '{"k": .a}'
err_probe '[.a, .b]'
err_probe '[.a]'
err_probe '.a, .b'
err_probe 'if .a then 1 else 2 end'
err_probe '(.a + 3) * 2'
err_probe 'empty'
err_probe 'error'
# Type/encoding builtins.
err_probe 'type'
err_probe 'tojson'
err_probe 'fromjson'
err_probe 'tostream'
err_probe 'input_line_number'
err_probe '$__loc__'
err_probe 'builtins'
err_probe 'halt'
err_probe 'debug'
# String builtins beyond the regex family already listed.
err_probe 'ltrimstr("x")'
err_probe 'rtrimstr("x")'
err_probe 'startswith("x")'
err_probe 'endswith("x")'
err_probe 'ascii_upcase'
err_probe 'explode'
err_probe 'implode'
err_probe 'join(",")'
err_probe 'test("x")'
err_probe 'capture("x")'
err_probe 'match("x")'
err_probe 'scan("x")'
# Array/object builtins that are NOT arb native verbs — the ones that are
# (`sort`, `min`, `max`, `floor`, `abs`) stay out of this list on purpose: SPEC §8
# makes a bare alphanumeric word the NATIVE verb, so accepting them is the
# documented context rule, not a jq leak.
err_probe 'reverse'
err_probe 'unique'
err_probe 'contains("x")'
err_probe 'inside([1])'
err_probe 'indices(1)'
err_probe 'flatten(1)'
err_probe 'del(.a)'
err_probe 'path(.a)'
err_probe 'walk(.)'
err_probe 'combinations'
err_probe 'transpose'
err_probe 'to_entries[]'
# The type-filter family.
err_probe 'recurse'
err_probe 'objects'
err_probe 'arrays'
err_probe 'booleans'
err_probe 'nulls'
err_probe 'scalars'
err_probe 'iterables'
# Math builtins.
err_probe 'sqrt'
err_probe 'infinite'
err_probe 'nan'
err_probe 'isnan'
err_probe 'todate'
err_probe 'now'

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

# xpath: out-of-subset location paths must be a hard error, not a guess. Same
# reasoning as err_probe above; xmllint answers all of these.
for xbad in '//a[1]' '//a[position()=1]' '//a/../span' '//a[text()="X"]' '//a/@*' \
            '//a[last()]' 'ancestor::div' '//a[@href][2]' \
            '//*' 'count(//a)' '//@href' '//a[@href and @rel]' \
            '//a[@href!="/x"]' '//a/following-sibling::span' \
            '//a[contains(text(),"X")]' '//a[not(@rel)]' 'normalize-space(//a)'; do
    if "$ARB" -e "out { in.html; $xbad }" <"$XPF" >/dev/null 2>&1; then
        fail=$((fail + 1))
        fails+=("xp!  $xbad — OUT OF SUBSET but accepted silently")
        [ "$QUIET" = 1 ] || printf 'DIFF xp!  %-24s accepted silently\n' "$xbad"
    else
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   xp!  %-24s (refused)\n' "$xbad"
    fi
done

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
# The form that does NOT: a LEADING `#` inside the braced spelling. `#` opens a
# COMMENT in arb's lexer, so `sel { #main }` lexes to an empty block, `block_text`
# reconstructs "", and the verb reports "expected a CSS selector" (exit 1).
#
# This is round 1's `sel { div.card h2 }` bug in its last unfixed corner. Round 1
# taught `sel` to rebuild a braced argument's text from the parsed commands, which
# fixed every selector whose first token survives lexing — but a leading `#` is
# eaten BEFORE parsing, so there is nothing left to rebuild. `#id` is the single
# most common selector in CSS, and `sel { CSS }` is the spelling SPEC §8 and the
# README both print, so the documented form cannot express it.
#
# Fixing it properly means either making `#` non-comment inside a block (which
# would break real comments in every `source { … }` body) or carrying the raw
# source span on `Arg::Block` (an AST change reaching the lexer, the parser and
# every block consumer). Neither is a change to make silently, so the probe stays
# RED and SPEC §8 records the limitation — the same treatment `keys` gets. It is
# reported every run rather than allowlisted away.
idw 'sel { #main }'       '//div[@id="main"]//p/text()'
rm -f "$IDF"
rm -f "$XPF"

# ── yq leg: no reference tool on this machine ───────────────────────────────
if ! command -v yq >/dev/null; then
    skip=$((skip + 1))
    yq_note="yq NOT INSTALLED — the yq leg of the superset claim is UNVERIFIED"
else
    yq_note="yq present but this harness has no yq probes yet"
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
