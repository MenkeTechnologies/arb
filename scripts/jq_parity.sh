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
#   3e19965cd4 (THIS corpus, before)          250 pass / 94 diverged / 1 skipped
#   HEAD       (THIS corpus, after)           343 pass /  1 diverged / 1 skipped
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
MIN_PROBES=344

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
