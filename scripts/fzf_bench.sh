#!/usr/bin/env bash
# fzf_bench.sh — differential + timing harness: arb's finder vs fzf and skim.
#
# README claims `arb --fzf` is a SUPERSET of fzf's core and that `--filter`
# scores "across cores". Both halves of that are contracts, and this script is
# how they are checked — in the shape of scripts/jq_parity.sh: run one corpus of
# probes through EVERY engine and byte-diff stdout, then time the same
# invocations against each other.
#
#   bash scripts/fzf_bench.sh          # parity probes + timings
#   bash scripts/fzf_bench.sh -q       # parity only, summary line
#   bash scripts/fzf_bench.sh -p       # parity only (skip timings, no hyperfine)
#
# References
#   fzf   — invoked as `fzf --filter STR`. That is the non-interactive matcher:
#           same scorer, same tiebreak, same sort as its picker, printed
#           best-first. arb's `--filter` is the same contract (src/cli.rs
#           run_filter), so the comparison is BYTE-EXACT — output ORDER is part
#           of what parity means here, not just the match set.
#   skim  — invoked as `sk --filter STR`. skim implements its own scorer, so its
#           tie ORDER legitimately differs from fzf's; its probes therefore
#           compare the sorted match SET. That relaxation is stated per probe in
#           the report, applies to skim only, and cannot hide a selection
#           difference — a line either matches or it does not.
#   A missing binary SKIPS its probes and the run says so, never passing
#   silently, and so does a probe whose REFERENCE refuses the invocation (a flag
#   it does not define, e.g. skim has no `--algo=v1` and no `--ignore-case`):
#   with no oracle there is nothing to compare, and charging arb for it would be
#   scoring a refusal the reference never made. A reference that ANSWERS and
#   disagrees is always a divergence, whichever engine is wrong — the exit status
#   counts those too. There is no allowlist in this script.
#
# Recorded measurement, this corpus shape, arb 0.1.16 / fzf 0.74.3 / sk 4.6.0:
#   39 pass / 1 diverged / 2 skipped. The two skips are skim flags that do not
#   exist (`--algo=v1`, `--ignore-case`); the divergence is skim's own defect —
#   `sk --filter cargo --tac` emits DUPLICATES (8,103 lines, 2,123 unique, where
#   its non-`--tac` run answers 8,053 distinct). arb and fzf agree byte-for-byte
#   on that same probe, so the harness is reporting a reference bug, not arb's.
#
# Corpus. Timings are meaningless on a toy input and parity is meaningless on a
# uniform one, so the corpus is real filesystem paths — long, repetitive,
# adversarial for a fuzzy scorer — collected from whichever of the candidate
# roots exist on this machine, then shuffled with a FIXED seed so two runs on
# one machine see identical input. Set $ARB_BENCH_CORPUS to a file to use your
# own. Size is reported, never assumed.
#
# Exit status is the number of diverging probes (0 = parity).
set -uo pipefail

ARB=${ARB:-arb}
FZF=${FZF:-fzf}
SK=${SK:-sk}
QUIET=0
PARITY_ONLY=0
for a in "$@"; do
  case "$a" in
    -q) QUIET=1 ;;
    -p) PARITY_ONLY=1 ;;
    -h|--help) perl -ne 'print if s/^# ?//' "$0" | head -40; exit 0 ;;
    *) echo "fzf_bench.sh: unknown arg: $a" >&2; exit 2 ;;
  esac
done

have() { command -v "$1" >/dev/null 2>&1; }

have "$ARB" || { echo "fzf_bench.sh: no '$ARB' on PATH (build it, or set \$ARB)" >&2; exit 2; }
HAVE_FZF=0; have "$FZF" && HAVE_FZF=1
HAVE_SK=0;  have "$SK"  && HAVE_SK=1

TMP=$(mktemp -d) || exit 2
trap 'rm -rf "$TMP"' EXIT

# ---------------------------------------------------------------- corpus ----
CORPUS=${ARB_BENCH_CORPUS:-}
if [[ -z $CORPUS ]]; then
  CORPUS="$TMP/corpus.txt"
  roots=()
  for r in /opt/homebrew/Cellar "$HOME/.cargo" "$HOME/.rustup" /usr/lib /usr/share /usr/local; do
    [[ -d $r ]] && roots+=("$r")
  done
  [[ ${#roots[@]} -eq 0 ]] && { echo "fzf_bench.sh: no corpus roots on this machine; set \$ARB_BENCH_CORPUS" >&2; exit 2; }
  find "${roots[@]}" -type f 2>/dev/null |
    perl -MList::Util=shuffle -e 'srand(42); print shuffle <STDIN>' > "$CORPUS"
fi
LINES=$(wc -l < "$CORPUS" | tr -d ' ')
BYTES=$(wc -c < "$CORPUS" | tr -d ' ')
[[ $LINES -gt 0 ]] || { echo "fzf_bench.sh: corpus is empty" >&2; exit 2; }

# A parity corpus small enough that every probe runs both engines quickly; the
# timing corpus is the whole thing.
SMALL="$TMP/small.txt"
head -n 20000 "$CORPUS" > "$SMALL"
SMALL_LINES=$(wc -l < "$SMALL" | tr -d ' ')

pass=0; diverged=0; skipped=0

# fzf_probe QUERY [EXTRA_FLAGS…] — arb must be BYTE-identical to fzf, order
# included. skim must return the same match SET (see the reference note above).
fzf_probe() {
  local q=$1; shift
  local flags=("$@")
  "$ARB" --filter "$q" "${flags[@]}" < "$SMALL" > "$TMP/a.out" 2>"$TMP/a.err"
  local n_arb; n_arb=$(wc -l < "$TMP/a.out" | tr -d ' ')

  # A reference exit of 1 is "no line matched" — a real answer, and a probe
  # worth making. An exit of 2 or more is a usage error: the reference does not
  # DEFINE that flag (skim has no `--algo=v1`, no `--ignore-case`), so there is
  # no oracle, the probe SKIPS, and the reason is printed. Charging arb for a
  # refusal the reference never made would be scoring the wrong thing.
  if [[ $HAVE_FZF -eq 1 ]]; then
    local rc_f=0
    "$FZF" --filter "$q" "${flags[@]}" < "$SMALL" > "$TMP/f.out" 2>"$TMP/f.err" || rc_f=$?
    if [[ $rc_f -ge 2 ]]; then
      skipped=$((skipped+1))
      printf '  skip  fzf  no oracle   %-28s %s %s\n' "$q" "${flags[*]}" "$(head -1 "$TMP/f.err")"
    elif cmp -s "$TMP/a.out" "$TMP/f.out"; then
      pass=$((pass+1))
      [[ $QUIET -eq 1 ]] || printf '  ok    fzf  byte-exact  %-28s %6s matches  %s\n' "$q" "$n_arb" "${flags[*]}"
    else
      diverged=$((diverged+1))
      printf '  DIFF  fzf  %-28s arb=%s fzf=%s %s\n' "$q" "$n_arb" "$(wc -l < "$TMP/f.out" | tr -d ' ')" "${flags[*]}"
      diff <(head -5 "$TMP/a.out") <(head -5 "$TMP/f.out") | head -12 | perl -pe 's/^/        /'
    fi
  else
    skipped=$((skipped+1))
  fi

  if [[ $HAVE_SK -eq 1 ]]; then
    local rc_s=0
    "$SK" --filter "$q" "${flags[@]}" < "$SMALL" > "$TMP/s.raw" 2>"$TMP/s.err" || rc_s=$?
    sort "$TMP/s.raw" > "$TMP/s.out"
    sort "$TMP/a.out" > "$TMP/as.out"
    if [[ $rc_s -ge 2 ]]; then
      skipped=$((skipped+1))
      printf '  skip  skim no oracle   %-28s %s %s\n' "$q" "${flags[*]}" "$(head -1 "$TMP/s.err")"
    elif cmp -s "$TMP/as.out" "$TMP/s.out"; then
      pass=$((pass+1))
      [[ $QUIET -eq 1 ]] || printf '  ok    skim set-equal   %-28s %6s matches  %s\n' "$q" "$n_arb" "${flags[*]}"
    else
      diverged=$((diverged+1))
      # Print skim's UNIQUE count next to its line count: when a reference emits
      # the same line twice, the set comparison alone reads as a mystery and the
      # duplication IS the finding.
      printf '  DIFF  skim %-28s arb=%s sk=%s (uniq %s) %s\n' "$q" "$n_arb" \
        "$(wc -l < "$TMP/s.raw" | tr -d ' ')" "$(sort -u "$TMP/s.raw" | wc -l | tr -d ' ')" "${flags[*]}"
    fi
  else
    skipped=$((skipped+1))
  fi
}

echo "corpus: $LINES lines, $BYTES bytes (parity probes on the first $SMALL_LINES)"
printf 'arb:  %s\n' "$("$ARB" --version 2>/dev/null | head -1)"
[[ $HAVE_FZF -eq 1 ]] && printf 'fzf:  %s\n' "$("$FZF" --version 2>/dev/null | head -1)" || echo 'fzf:  ABSENT — its probes SKIP'
[[ $HAVE_SK  -eq 1 ]] && printf 'skim: %s\n' "$("$SK"  --version 2>/dev/null | head -1)" || echo 'skim: ABSENT — its probes SKIP'
echo

echo '== parity =='
# Plain fuzzy terms, across hit rates: near-everything, moderate, rare, none.
fzf_probe 'rs'
fzf_probe 'cargo'
fzf_probe 'libcore'
fzf_probe 'zqxjvw'
# The extended query language (SPEC/README table): every sigil, and the
# compositions — exact, boundary-exact, prefix, suffix, whole line, negation,
# OR sets, and multi-term conjunction.
fzf_probe "'cargo"
fzf_probe '^/opt'
fzf_probe '.rs$'
fzf_probe '!zsh rs'
fzf_probe 'src | doc'
fzf_probe 'libc rs$'
fzf_probe '!^/opt cargo'
fzf_probe '^/usr | ^/opt'
# Match-mode and ordering flags: the ones that change what the scorer does, not
# how the picker looks.
fzf_probe 'cargo' --exact
fzf_probe 'cargo' --no-sort
fzf_probe 'cargo' --tac
fzf_probe 'rs' --algo=v1
fzf_probe 'lib' --tiebreak=length
fzf_probe 'lib' --tiebreak=end
fzf_probe 'lib' --scheme=path
fzf_probe 'CARGO' --ignore-case
fzf_probe 'Cargo'          # smart-case: an uppercase char makes it sensitive
echo

# --------------------------------------------------------------- timing ----
if [[ $PARITY_ONLY -eq 0 && $QUIET -eq 0 ]]; then
  echo '== timing =='
  if have hyperfine; then
    # Query shapes that exercise different regimes: a dense fuzzy hit rate, a
    # sparse one, an exact term, a multi-term extended query, and a query that
    # matches NOTHING — the last one is the floor, where every engine is doing
    # little but reading stdin and the spread collapses to I/O.
    for q in 'cargo' 'libcore' "'cargo" '^/opt .rs$ !zsh' 'zqxjvw'; do
      echo "-- ${LINES}-line corpus, query '$q'"
      # Double-quote the query inside the command hyperfine hands to `sh -c`:
      # single quotes would break on a query that CONTAINS one (`'cargo`, the
      # exact-match sigil), and a broken command under `-i` times a failed exec
      # in microseconds instead of reporting it.
      cmds=(-n arb "$ARB --filter \"$q\" < '$CORPUS' > /dev/null")
      [[ $HAVE_FZF -eq 1 ]] && cmds+=(-n fzf "$FZF --filter \"$q\" < '$CORPUS' > /dev/null")
      [[ $HAVE_SK  -eq 1 ]] && cmds+=(-n skim "$SK --filter \"$q\" < '$CORPUS' > /dev/null")
      hyperfine -i --warmup 2 --runs 8 --style basic "${cmds[@]}" 2>&1 |
        grep -E 'Benchmark|Time |times faster'
    done
    # Process startup, measured with no input at all: the fixed cost every
    # invocation pays before a single line is scored. It is the one axis where
    # arb does not win, and leaving it out would make the table dishonest.
    echo '-- startup floor, empty stdin'
    cmds=(-n arb "$ARB --filter x < /dev/null > /dev/null")
    [[ $HAVE_FZF -eq 1 ]] && cmds+=(-n fzf "$FZF --filter x < /dev/null > /dev/null")
    [[ $HAVE_SK  -eq 1 ]] && cmds+=(-n skim "$SK --filter x < /dev/null > /dev/null")
    hyperfine -i --warmup 5 --runs 30 --style basic "${cmds[@]}" 2>&1 |
      grep -E 'Benchmark|Time |times faster'
  else
    echo '  hyperfine ABSENT — timings SKIPPED (brew install hyperfine)'
  fi
  echo
fi

echo "== summary: $pass pass / $diverged diverged / $skipped skipped =="
exit "$diverged"
