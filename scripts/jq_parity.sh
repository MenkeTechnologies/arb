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
#   err_probe            — an OUT-of-subset construct must EXIT NON-ZERO. Passing
#                          one silently is the worse failure of the two: a wrong
#                          answer that looks like an answer. (The earlier
#                          `select(.status == "ok")` bug was exactly this — the
#                          quotes were dropped in reconstruction and the filter
#                          matched nothing instead of erroring.)
#
# Exit status is the number of diverging probes (0 = parity). Divergences are
# only ever reported, never suppressed: there is no allowlist in this script.
#
# Recorded measurement, this corpus, both binaries built from the same tree:
#   cec1d985a2 (before the parity work)   41 pass / 19 diverged / 1 skipped
#   c952bfce57 (after that wave)          59 pass /  1 diverged / 1 skipped
#   879e61a823 (the previous corpus, before)  128 pass / 26 diverged / 1 skipped
#   77d4244243 (the previous corpus, after)   153 pass /  1 diverged / 1 skipped
#   aac6d4eefa (THIS corpus, before)          158 pass / 15 diverged / 1 skipped
#   HEAD       (THIS corpus, after)           172 pass /  1 diverged / 1 skipped
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
# The 1 remaining is the bare `keys` SPELLING COLLISION and is expected to stay:
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
# The floor the probe count must clear. `xp_probe` SKIPS silently when xmllint
# is missing, so without this a machine with no xmllint drops 24 probes and
# still reports a clean run. Raise it when the corpus grows; never lower it.
MIN_PROBES=173

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
    echo "jq_parity: no xmllint — the 24 xpath probes would SKIP and the run" >&2
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

# xpath: out-of-subset location paths must be a hard error, not a guess. Same
# reasoning as err_probe above; xmllint answers all of these.
for xbad in '//a[1]' '//a[position()=1]' '//a/../span' '//a[text()="X"]' '//a/@*' \
            '//a[last()]' 'ancestor::div' '//a[@href][2]'; do
    if "$ARB" -e "out { in.html; $xbad }" <"$XPF" >/dev/null 2>&1; then
        fail=$((fail + 1))
        fails+=("xp!  $xbad — OUT OF SUBSET but accepted silently")
        [ "$QUIET" = 1 ] || printf 'DIFF xp!  %-24s accepted silently\n' "$xbad"
    else
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   xp!  %-24s (refused)\n' "$xbad"
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
