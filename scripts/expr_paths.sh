#!/usr/bin/env bash
# expr_paths.sh — differential harness for arb's EXPRESSION engine.
#
# `scripts/jq_parity.sh` covers the query front-end (`.a.b`, `select`, xpath).
# This one covers the other half: the arithmetic/predicate language in
# `src/expr.rs` that `where`, `map` and `calc` compile to a `fusevm::Chunk`.
# It runs every probe through THREE engines and diffs all three:
#
#   interp  arb with `FUSEVM_JIT_BLOCK_THRESHOLD` set past any run's reach, so
#           every chunk stays on the fusevm interpreter.
#   native  arb with `FUSEVM_JIT_BLOCK_THRESHOLD=0`, so the Cranelift block tier
#           compiles each chunk on its first invocation and the answer comes
#           from native code.
#   jq      the reference, jq 1.8.2, for the probes that have a jq equivalent.
#
# WHY THE THIRD ENGINE MATTERS. arb's default is `block_threshold = 1`: the
# first evaluation of a chunk is interpreted and every later one is native. A
# construct the two tiers disagree about therefore answers differently for the
# FIRST row of a stream than for the rest — identical input lines producing
# different output within one run, with nothing in the output saying so. A
# single-engine harness cannot see that class at all, which is why interp/native
# are separate columns here rather than one "arb" column.
#
# WHY jq IS A LEGITIMATE REFERENCE FOR PART OF THIS. arb is an original
# language, not a port, so most of the spec language has no reference
# implementation anywhere. Its numeric core does: over a stream of numbers,
# `map EXPR` and jq's `EXPR` compute the same thing on the same IEEE doubles,
# and arb's `fmt_num` (src/query.rs:1848) renders them the same way jq -r does.
# So arithmetic, comparison, and operator PRECEDENCE are checked against a real
# installed tool; everything jq has no spelling for (`x in [a,b]`, `.control`,
# `match(…)`) is checked for interp/native agreement only, and its jq column is
# reported as SKIPPED — never as a pass.
#
# TWO AGREEING FAILURES MUST NOT READ AS A PASS. The jq leg only counts when jq
# EXITED 0 AND WROTE SOMETHING. A jq error (`cannot be divided because the
# divisor is zero`) is a SKIP with the reason printed, never a comparison — two
# engines both producing an error message is not agreement about a value.
#
#   bash scripts/expr_paths.sh          # every probe, print divergences
#   bash scripts/expr_paths.sh -q       # summary only
#
# Exit status is the number of diverging probes (0 = parity). There is no
# allowlist: every divergence is printed every run.
#
# Recorded measurement, this corpus, both binaries built from the same tree
# (`ARB=<path> bash scripts/expr_paths.sh` aims it at another build):
#   b4449e270f (before this corpus existed)   55 pass / 18 diverged / 7 skipped
#   HEAD       (after the expr.rs fixes)      71 pass /  2 diverged / 7 skipped
# The 16 closed were `or` lowering to the SUM of its operands (`map x > 1 or
# x > 2` printed `2` where both held), `in [list]` lowering to the COUNT of
# matching elements (`map x in [1,1,1]` printed `3`), and the parenthesized
# groupings `(a) and (b)` / `not (a or b)` / `(c ? t : e)`, which were parse
# errors because a `(` group re-entered the grammar at `additive`.
#
# The 2 remaining are fusevm's, not arb's, and are expected to stay until the VM
# is fixed: `Op::Div` by a zero divisor pushes `Value::Undef` (renders `0`) on
# the interpreter and IEEE `inf` in compiled code. Working around it inside arb
# would mean diverging from the op every sibling frontend shares, so the probes
# stay red and say so.

set -u
cd "$(dirname "$0")/.." || exit 2

# Overridable so the same corpus can be aimed at another build — the check that
# a new probe is not blind is to run it against the binary from BEFORE the fix
# and confirm it fails there.
ARB=${ARB:-./target/debug/arb}
QUIET=0
[ "${1:-}" = "-q" ] && QUIET=1

[ -x "$ARB" ] || { echo "expr_paths: $ARB not built — run 'cargo build'" >&2; exit 2; }

# Past any plausible invocation count, so the block tier never fires.
NEVER=4294967295

pass=0; fail=0; skip=0
declare -a fails=()
declare -a skips=()

# arb_run THRESHOLD SPEC INPUT — one arb evaluation at a fixed JIT threshold.
arb_run() {
    printf '%s\n' "$3" | FUSEVM_JIT_BLOCK_THRESHOLD="$1" "$ARB" -e "$2" 2>&1
}

# probe LABEL INPUT ARB_PIPELINE JQ_FILTER
#   ARB_PIPELINE  the body of `out { … }`, e.g. 'in; map x * 2'
#   JQ_FILTER     the reference filter, or '-' when jq has no spelling for it
#                 (then only the interp/native halves are compared).
probe() {
    local label="$1" input="$2" pipeline="$3" jqf="$4"
    local i n j jrc shown
    # A probe feeds a multi-line stream; collapse it for the one-line report so
    # the label column stays readable.
    shown=$(printf %s "$input" | tr '\n' ',')

    i=$(arb_run "$NEVER" "out { $pipeline }" "$input")
    n=$(arb_run 0 "out { $pipeline }" "$input")

    if [ "$i" != "$n" ]; then
        fail=$((fail + 1))
        fails+=("PATH $label <= $shown"$'\n'"       interp: $(printf %s "$i" | tr '\n' '|')"$'\n'"       native: $(printf %s "$n" | tr '\n' '|')")
        [ "$QUIET" = 1 ] || printf 'DIFF path %-34s %s\n       interp: %s\n       native: %s\n' \
            "$label" "$shown" "$(printf %s "$i" | tr '\n' '|')" "$(printf %s "$n" | tr '\n' '|')"
        return
    fi

    if [ "$jqf" = "-" ]; then
        skip=$((skip + 1))
        skips+=("$label — no jq spelling; interp/native agreement only")
        [ "$QUIET" = 1 ] || printf 'ok   path %-34s %s (no jq ref)\n' "$label" "$shown"
        return
    fi

    j=$(printf '%s\n' "$input" | jq -rc "$jqf" 2>/dev/null); jrc=$?
    # The reference must have SUCCEEDED before its answer counts. A non-zero exit
    # or empty stdout means jq refused the construct (or has no value for it) —
    # comparing against that would score two failures as one pass.
    if [ "$jrc" -ne 0 ] || [ -z "$j" ]; then
        skip=$((skip + 1))
        skips+=("$label — jq exited $jrc with $( [ -z "$j" ] && echo 'no output' || echo 'output' ); not counted")
        [ "$QUIET" = 1 ] || printf 'SKIP jq   %-34s %s (jq rc=%d, no usable answer)\n' "$label" "$shown" "$jrc"
        return
    fi

    if [ "$i" = "$j" ]; then
        pass=$((pass + 1))
        [ "$QUIET" = 1 ] || printf 'ok   all  %-34s %s -> %s\n' "$label" "$shown" "$(printf %s "$i" | tr '\n' '|')"
    else
        fail=$((fail + 1))
        fails+=("JQ   $label <= $shown"$'\n'"       arb: $(printf %s "$i" | tr '\n' '|')"$'\n'"       jq : $(printf %s "$j" | tr '\n' '|')")
        [ "$QUIET" = 1 ] || printf 'DIFF jq   %-34s %s\n       arb: %s\n       jq : %s\n' \
            "$label" "$shown" "$(printf %s "$i" | tr '\n' '|')" "$(printf %s "$j" | tr '\n' '|')"
    fi
}

# map_probe INPUT ARBEXPR JQEXPR — `map EXPR` against jq's `EXPR`.
map_probe() { probe "map $2" "$1" "in; map $2" "$3"; }

# bool_probe INPUT ARBEXPR JQEXPR — a predicate used as a VALUE. arb renders a
# boolean as 1/0 (it is an f64 all the way down) and jq renders it as true/false,
# so the REFERENCE side is wrapped in `if … then 1 else 0 end`. That is a
# rendering normalization applied to jq only; it cannot mask a value difference,
# because a wrong boolean still comes out as the other digit.
bool_probe() { probe "map $2" "$1" "in; map $2" "if ($3) then 1 else 0 end"; }

# where_probe INPUT ARBPRED JQPRED — the filter half. jq's `select` drops a row
# the same way arb's `where` does, so the surviving stream is directly
# comparable with no normalization at all.
where_probe() {
    # `-` means "jq has no spelling for this predicate" and must reach `probe`
    # unwrapped; wrapping it as `select(-)` would make jq fail and the probe
    # would be filed under "jq refused" instead of "no reference exists".
    if [ "$3" = "-" ]; then
        probe "where $2" "$1" "in; where $2" -
    else
        probe "where $2" "$1" "in; where $2" "select($3)"
    fi
}

NUMS=$'1\n2\n3\n4\n5\n6\n7\n8\n9\n10'
MIX=$'-4\n-1\n0\n1\n2\n7\n100'

# ── arithmetic: every operator, against jq ──────────────────────────────────
map_probe "$NUMS" 'x + 1'            '. + 1'
map_probe "$NUMS" 'x - 3'            '. - 3'
map_probe "$NUMS" 'x * 7'            '. * 7'
map_probe "$NUMS" 'x / 4'            '. / 4'
map_probe "$NUMS" 'x % 3'            '. % 3'
map_probe "$MIX"  'x * x'            '. * .'
map_probe "$MIX"  '-x'               '-.'
map_probe "$MIX"  '0 - x'            '0 - .'
map_probe "$NUMS" 'x / 3'            '. / 3'
map_probe "$MIX"  'x * 0.1'          '. * 0.1'
map_probe "$NUMS" 'x * 1000000000'   '. * 1000000000'

# ── precedence and grouping ─────────────────────────────────────────────────
# The parenthesized forms are the ones a reader writes first. Until the `(`
# branch of `primary_inner` recursed into a full expression they were parse
# errors, so these are load-bearing rather than decorative.
map_probe "$NUMS" 'x + 2 * 3'        '. + 2 * 3'
map_probe "$NUMS" '(x + 2) * 3'      '(. + 2) * 3'
map_probe "$NUMS" 'x * 2 + 3'        '. * 2 + 3'
map_probe "$NUMS" 'x - 2 - 1'        '. - 2 - 1'
map_probe "$NUMS" '(x - 2) - 1'      '(. - 2) - 1'
map_probe "$NUMS" 'x - (2 - 1)'      '. - (2 - 1)'
map_probe "$NUMS" '((x))'            '.'
map_probe "$NUMS" '(x + 1) * (x + 2)' '(. + 1) * (. + 2)'
map_probe "$NUMS" '100 / (x + 1)'    '100 / (. + 1)'

# ── comparisons as VALUES ───────────────────────────────────────────────────
# `map` prints the expression's value, so a comparison's numeric rendering is
# observable and must be 0/1 — not a count, not a sum.
bool_probe "$NUMS" 'x > 5'           '. > 5'
bool_probe "$NUMS" 'x >= 5'          '. >= 5'
bool_probe "$NUMS" 'x < 5'           '. < 5'
bool_probe "$NUMS" 'x <= 5'          '. <= 5'
bool_probe "$NUMS" 'x == 5'          '. == 5'
bool_probe "$NUMS" 'x != 5'          '. != 5'
bool_probe "$MIX"  'x > 0'           '. > 0'

# ── logical combinators as VALUES ───────────────────────────────────────────
# `or` used to lower to the SUM of its two normalized operands, so a row where
# both sides held printed `2`. Truthiness hid it from `where`; `map` shows it.
bool_probe "$NUMS" 'x > 1 or x > 2'      '. > 1 or . > 2'
bool_probe "$NUMS" 'x > 5 or x < 3'      '. > 5 or . < 3'
bool_probe "$NUMS" 'x > 1 and x > 2'     '. > 1 and . > 2'
bool_probe "$NUMS" 'x > 5 and x < 8'     '. > 5 and . < 8'
bool_probe "$NUMS" 'not x > 5'           '. <= 5'
bool_probe "$NUMS" '(x > 1) and (x < 5)' '(. > 1) and (. < 5)'
bool_probe "$NUMS" '(x > 8) or (x < 2)'  '(. > 8) or (. < 2)'
bool_probe "$NUMS" 'not (x > 1 or x > 2)' '(. > 1 or . > 2) | not'
bool_probe "$NUMS" 'not (x > 5 and x < 8)' '(. > 5 and . < 8) | not'
# Nested two deep, both associations — the shape the fleet's control-flow bugs
# hide in.
bool_probe "$NUMS" 'x > 2 and (x < 5 or x > 8)'  '. > 2 and (. < 5 or . > 8)'
bool_probe "$NUMS" '(x > 2 and x < 5) or x > 8'  '(. > 2 and . < 5) or . > 8'
bool_probe "$NUMS" 'x > 2 or x < 5 or x > 8'     '. > 2 or . < 5 or . > 8'
bool_probe "$NUMS" 'x > 2 and x < 9 and x != 5'  '. > 2 and . < 9 and . != 5'
bool_probe "$NUMS" 'not not x > 5'               '. > 5'

# ── the ternary: only the taken branch may run ──────────────────────────────
map_probe "$NUMS" 'x > 5 ? 100 : 0'          'if . > 5 then 100 else 0 end'
map_probe "$NUMS" '(x > 5 ? 100 : 0)'        'if . > 5 then 100 else 0 end'
map_probe "$NUMS" 'x > 5 ? x * 2 : x + 1'    'if . > 5 then . * 2 else . + 1 end'
map_probe "$MIX"  'x != 0 ? 100 / x : -1'    'if . != 0 then 100 / . else -1 end'
# Nested in both positions.
map_probe "$NUMS" 'x > 5 ? (x > 8 ? 3 : 2) : 1'  'if . > 5 then (if . > 8 then 3 else 2 end) else 1 end'
map_probe "$NUMS" 'x > 5 ? 1 : (x > 2 ? 2 : 3)'  'if . > 5 then 1 else (if . > 2 then 2 else 3 end) end'
map_probe "$NUMS" 'x > 5 ? 1 : x > 2 ? 2 : 3'    'if . > 5 then 1 else (if . > 2 then 2 else 3 end) end'
# A ternary whose condition is itself a logical combination.
map_probe "$NUMS" 'x > 2 and x < 8 ? 1 : 0'      'if . > 2 and . < 8 then 1 else 0 end'
map_probe "$NUMS" 'x > 8 or x < 2 ? 1 : 0'       'if . > 8 or . < 2 then 1 else 0 end'

# ── the same predicate through `where` instead of `map` ─────────────────────
# The two surfaces read the same chunk through different fusevm entry points
# (`eval_pred_ctx` reads `is_truthy`, `eval_ctx` reads `to_float`), so a node
# can be right in one and wrong in the other.
where_probe "$NUMS" 'x > 5'                  '. > 5'
where_probe "$NUMS" 'x > 1 and x < 5'        '. > 1 and . < 5'
where_probe "$NUMS" 'x > 8 or x < 2'         '. > 8 or . < 2'
where_probe "$NUMS" '(x > 1) and (x < 5)'    '(. > 1) and (. < 5)'
where_probe "$NUMS" 'not x > 5'              '. <= 5'
where_probe "$NUMS" 'not (x > 2 and x < 8)'  '(. > 2 and . < 8) | not'
where_probe "$NUMS" 'x > 2 and (x < 5 or x > 8)' '. > 2 and (. < 5 or . > 8)'
where_probe "$MIX"  'x != 0'                 '. != 0'
where_probe "$NUMS" 'x % 2 == 0'             '. % 2 == 0'

# ── `in [list]` — jq's own `IN` builtin is the reference ────────────────────
# arb's `left in [a,b,c]` is jq's `IN(a;b;c)`: SPEC §8 says so, and jq 1.8.2
# ships `IN` as a builtin, so this is a real second implementation computing the
# answer independently — not a restatement of what arb does.
#
# The reason it is worth a reference at all: `in [list]` lowered to the SUM of
# the per-element `==` results, i.e. the COUNT of matches. A list with a repeated
# element (`x in [1,1,1]`) therefore answered `3` out of a `map`, and BOTH tiers
# agreed on `3` — so the interp/native column alone could never have seen it.
# That is the whole "a clean number measures the generator's reach" trap: a probe
# with no independent oracle is only a consistency check.
map_probe "$NUMS" 'x in [1,2,3]'      'if IN(1,2,3) then 1 else 0 end'
map_probe "$NUMS" 'x in [1,1,1]'      'if IN(1,1,1) then 1 else 0 end'
map_probe "$NUMS" 'x in [5]'          'if IN(5) then 1 else 0 end'
map_probe "$NUMS" 'x in [1,5,5,9]'    'if IN(1,5,5,9) then 1 else 0 end'
map_probe "$NUMS" 'x in [1,2] or x in [9,10]' 'if (IN(1,2) or IN(9,10)) then 1 else 0 end'
map_probe "$NUMS" 'not x in [1,2,3]'  'if (IN(1,2,3)|not) then 1 else 0 end'
where_probe "$NUMS" 'x in [2,4,6]'    'IN(2,4,6)'
where_probe "$NUMS" 'x in [1,1,1]'    'IN(1,1,1)'
where_probe "$NUMS" 'x in [1,5,5,9]'  'IN(1,5,5,9)'
# `lo..hi` has no jq builtin. Its SPEC definition is the inclusive `lo <= v <=
# hi`, so the reference is that pair of compares evaluated by jq's own
# arithmetic — a translation of the documented rule, stated here rather than
# hidden, not a second reading of arb's lowering.
map_probe "$NUMS" 'x in 3..7'         'if (. >= 3 and . <= 7) then 1 else 0 end'
map_probe "$MIX"  'x in -2..2'        'if (. >= -2 and . <= 2) then 1 else 0 end'
where_probe "$NUMS" 'x in 3..7'       '. >= 3 and . <= 7'

# ── degenerate / boundary values on both tiers ──────────────────────────────
# Not compared to jq: jq REFUSES a zero divisor rather than answering, so the
# `-` reference marks these interp/native-only by construction rather than
# letting a jq error masquerade as agreement.
map_probe "$NUMS" 'x / 0'             -
map_probe "$NUMS" 'x % 0'             -
map_probe "$NUMS" '0 / x'             -
map_probe $'abc\ndef'  'x + 1'        -
map_probe $'abc\ndef'  'x > 1'        -
map_probe $'abc\ndef'  'x in [1,2]'   -
map_probe $'abc\ndef'  'not x'        -
map_probe "$NUMS" 'nosuchfield + 1'   -
map_probe "$NUMS" 'x > 5 ? 1 / 0 : 2' -

echo
echo "── expr_paths summary ──────────────────────────────"
printf 'pass %d   diverged %d   skipped %d\n' "$pass" "$fail" "$skip"
if [ "$skip" -gt 0 ]; then
    echo
    echo "skipped (reference gave no usable answer — NOT counted as passing):"
    for s in "${skips[@]}"; do printf '  %s\n' "$s"; done
fi
if [ "$fail" -gt 0 ]; then
    echo
    echo "diverged probes:"
    for f in "${fails[@]}"; do printf '  %s\n' "$f"; done
fi
exit "$fail"
