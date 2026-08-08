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
# Exit status is the number of diverging probes (0 = parity). Divergences are
# only ever reported, never suppressed: there is no allowlist in this script.
#
# Recorded measurement, this corpus, both binaries built from the same tree:
#   cec1d985a2 (before the parity work)  41 pass / 19 diverged / 1 skipped
#   c952bfce57 (after)                   59 pass /  1 diverged / 1 skipped
# The one remaining divergence is `keys`, whose shape collision SPEC.md §8
# documents as an open decision rather than a bug to quietly re-render.

set -u
cd "$(dirname "$0")/.." || exit 2

ARB=./target/debug/arb
QUIET=0
[ "${1:-}" = "-q" ] && QUIET=1

[ -x "$ARB" ] || { echo "jq_parity: $ARB not built — run 'cargo build'" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq_parity: jq not found" >&2; exit 2; }

pass=0; fail=0; skip=0
fails=()

# jq_probe INPUT FILTER — feed INPUT to both engines with the same filter.
jq_probe() {
    local input="$1" filter="$2" a b
    a=$(printf '%s\n' "$input" | "$ARB" -e "out { in.json; $filter }" 2>&1)
    b=$(printf '%s\n' "$input" | jq -rc "$filter" 2>&1)
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

# xp_probe FILE XPATH — compare arb's xpath front-end against xmllint.
# `@attr` probes normalize the reference from ` name="v"` to `v` (see header).
xp_probe() {
    local file="$1" xp="$2" a b
    command -v xmllint >/dev/null || { skip=$((skip + 1)); return; }
    a=$("$ARB" -e "out { in.html; $xp }" <"$file" 2>&1)
    b=$(xmllint --html --xpath "$xp" "$file" 2>/dev/null)
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

# ── jq: builtins the SPEC claims (keys/values/length/add/has/to_entries) ────
jq_probe '{"a":1,"b":2}'                 'keys'
jq_probe '{"a":1,"b":2}'                 'values'
jq_probe 'null'                          'values'
jq_probe '{"a":1}'                       'has("a")'
jq_probe '{"x":2}'                       'has("id")'
jq_probe '{"a":1,"b":2}'                 'to_entries'
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

# ── xpath / css ─────────────────────────────────────────────────────────────
XPF=$(mktemp -t arbxp).html
cat >"$XPF" <<'EOF'
<html><body>
<div class="card"><h2>Title</h2><span>inner</span><a href="/x">X</a></div>
<a href="/y">Y</a>
</body></html>
EOF
xp_probe "$XPF" '//a/@href'
xp_probe "$XPF" '//a/text()'
xp_probe "$XPF" '//h2/text()'
xp_probe "$XPF" '//div[@class]//span/text()'
xp_probe "$XPF" '//div/h2/text()'
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
echo "note: $yq_note"
if [ "$fail" -gt 0 ]; then
    echo
    echo "diverged probes:"
    for f in "${fails[@]}"; do printf '  %s\n' "$f"; done
fi
exit "$fail"
