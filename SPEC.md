# arb — SPEC

**arb** is a standalone, original language on **fusevm/JIT** for **visualizing and modifying Unix pipelines**: drop it in a pipe and it spawns a **dynamic TUI (ratatui) or served web page (zgui components)** built from a declarative spec. It is a **jq/xpath/css/yq superset**, an interactive **megafilter/map** over the live passthrough, its own **Tcl/Tk-flavored DSL**, and a **preset library / package manager** so users share dashboards — *a TUI for every pipeline*. (LSP/DAP stdio frontends ship. Akka-style actors ship: `actor NAME(state) { on MSG { … } }` + a `via NAME * N` pipeline op that fans the stream across a supervised worker pool in parallel — see §15.)

Original language (stryke's class), **not a port**. MIT, standalone crate, lean (rubyrs-scale, not stryke-scale).

---

## 0. Positioning

- **World-first = the synthesis + ecosystem**, not any single leg. Prior art per leg: Tcl'88, Tk'88, Expect'90 (spawn/react), dasel (unified query), ratatui (TUI), Streamlit/Textual-serve (served web UI), filt (interactive pipe grep — *single filter box, filter-only, TUI-only; not comparable*). No tool is a pipe-native, dual-target (terminal+web), component-generating UI language with a shareable dashboard registry.
- **Not a server-only thing**: terminal-invoked, pipe-driven. Web target spawns a local UI host (like `textual serve`), not a daemon.

## 1. Invocation

```sh
cmd | arb dash.arb            # TUI (ratatui)
cmd | arb -t web dash.arb     # served zgui page + WS live update
cmd | arb -p http             # preset (== import http)
cmd | arb                     # zero-config: sniff input/upstream cmd → auto preset
cmd | arb -e 'text .t <- in'  # inline
producer | arb dash.arb | consumer   # mid-pipe: controls shape downstream output
arb -l                        # list presets/packages
arb save dash.arb as api      # register a user preset
```

**fzf drop-in.** `arb --fzf` resolves its options the way the `fzf` binary does —
`$FZF_DEFAULT_OPTS_FILE`, then `$FZF_DEFAULT_OPTS`, then the command line, later
winning — and honors the presentation set (`--layout`/`--reverse`, `--border`,
`--info`, `--color`, `--pointer`, `--marker`, `--ellipsis`, `--scrollbar`,
`--scroll-off`, `--cycle`, `--tac`, `--tiebreak`, `--bind`, `--min-height`,
`--ansi`) on top of the matching and I/O set (`--exact`, `--no-sort`/`--sort`,
`-q`/`--query`, `-i`/`--ignore-case`, `+i`/`--no-ignore-case`, `--smart-case`,
`--multi`, `--prompt`, `--header`, `--header-lines`, `--height`, `--preview`,
`-f`/`--filter`, `-d`/`--delimiter`, `-n`/`--nth`, `--with-nth`,
`--print-query`, `--expect`, `--with-shell`). Child processes (a `--preview`
command) start with `$SHELL -c` like fzf's, not `sh -c`. A repeated option takes
its LAST value, as fzf does — `$FZF_DEFAULT_OPTS` and the call site both setting `--prompt` is the
normal case, not an error. The whole fzf 0.74 option surface parses, including
every `--no-X` negation, which cancels an earlier `--X` rather than being
dropped — `--preview CMD --no-preview` leaves no preview — and an
optional-value option consumes the next word only under fzf's own rule for it
(a word that isn't a flag for `--border`, a number for `--gap`). Matching
itself is fzf's algorithm, ported from `src/algo/algo.go` into `src/algo.rs`
(`FuzzyMatchV2` + `ExactMatchNaive`), so rankings are identical line for line —
verifiable with `--filter`, which prints the ranked matches without a UI.
`--height 100%` is fzf's full-screen form and runs on the alternate screen, so
quitting restores the terminal untouched; a smaller `--height` draws below the
cursor and scrolls the terminal to make room, sized like fzf's (percentages get
the `--min-height=10+` floor); arb places that viewport itself rather than
asking crossterm, whose cursor query writes to stdout — arb's data channel — and
never returns when stdout is a pipe. The picker it draws — layout, separator,
pointer/marker glyphs, palette, scrollbar, ellipsis, scroll margin, ranking —
matches what `fzf` draws for the same environment, so `ZPWR_FZF='arb --fzf'`
changes nothing on screen. Flags with no arb analog are accepted and ignored, and
a `--bind` naming an unimplemented action is skipped rather than half-applied.

## 2. Lexical (Tcl-flavored, NOT Tcl)

```
verb arg arg { block }     # command + args; {} is a verbatim block
# comment                    (no $, no [cmd] subst, no expr{})
;                          # optional stmt separator (newline also separates)
.a.b.c                     # widget path (dot hierarchy, Tk-style)
```

## 3. Values

```
42  3.14  "s"  's'  true  false  nil
1s 500ms 2m        # durations
1kb 4mb            # sizes
/re/  /re/i        # regex
[1,2,3]            # list
{a:1, b:2}         # map
```

## 4. Variables (Python/Swift-lite)

```
max = 100          # immutable
var n = 0          # mutable
n = n + 1
```

## 5. Functions & lambdas

```
fn pct(v, m): v / m * 100          # single-expr body
fn norm(xs) { hi = max(xs); map(x => x/hi, xs) }   # block; last expr returns
dbl = x => x * 2                   # lambda
add = (a, b) => a + b
pct(50, 200)                       # call
```

## 6. Operators

```
+ - * / %      arithmetic
== != < <= > >= comparison
and or not     logical
+              string concat
x matches /re/ regex test
x in [..]      membership
a..b           range
( … )          grouping — a full expression, including a comparison, a
               logical, or a `?:` (`(x > 1) and (x < 5)`, `not (a or b)`)
|>             value pipe: xs |> filter(even) |> sum
```

Every value is an f64, so a predicate has a NUMBER for its value: `1` when it
holds and `0` when it does not, for `==`/`<`/`and`/`or`/`not`/`in` alike. That
matters wherever an expression's value is printed rather than tested —
`map x > 1 or x > 2` emits `1`, never a count of how many sides held.

A number literal may carry an exponent (`1e17`, `2e+2`, `1.5e-6`, `1E3`), which
is also how arb prints one back.

### How a computed number prints

A computed value renders the way `jq -r` renders a computed double: the shortest
digits that round-trip and, among those, the ones NEAREST the value, positional
up to the point where jq switches, then exponential with a signed
two-digit-minimum exponent.

```
1e15 * 1   -> 1000000000000000       1e16 * 1  -> 1e+16
1.5e16 * 1 -> 15000000000000000      1.5e17 * 1 -> 1.5e+17
0.0001 * 1 -> 0.0001                 0.00001 * 1 -> 1e-05
1e18 / 3   -> 333333333333333300     2e-9 * 2  -> 4e-09
```

The switch is on the DIGIT COUNT, not the magnitude — `1e16` is exponential and
`1.5e16`, one significant digit longer, is not.

Shortest and shortest-AND-CLOSEST are different rules, and the difference shows.
Where neighbouring doubles sit further apart than one unit in the last decimal
place, several decimals of the shortest length parse back to the same double, and
only one of them is nearest. `191510495617760.12 * 1` prints those digits because
they are the nearest; `…13` round-trips just as well and is what stopping at the
first round-tripping candidate gives. The ties bunch around 1e14..1e17, seldom
enough that a pool spread evenly across the exponent range almost never lands on
one.

A result with no finite value follows jq as well: an infinity CLAMPS to ±DBL_MAX
(`1e308 * 2` -> `1.7976931348623157e+308`) and a NaN prints as `null`
(`1e308 * 2 * 0`). Neither has a spelling in JSON.

One deliberate difference from jq remains:

- **There is no literal preservation.** A number jq never computes on is held as
  a DECIMAL rather than a double and reprinted from that, so `.n` on
  `{"n":1e17}` gives `1E+17` — and unary `-.` preserves it too, printing `-1E-7`
  where the computed path gives `-1e-07`. It is not the source text being echoed:
  `1e17`, `1E17` and `1e+17` all come back as `1E+17`, decNumber's canonical
  form.

  Matching it is not a formatter change. That decimal holds values no double can
  — `{"n":1e400}` reprints as `1E+400`, and a 30-digit integer keeps all 30
  digits, where an f64 overflows and rounds respectively. Following jq here would
  mean carrying an arbitrary-precision decimal beside every number, which is
  exactly the "every value is an f64" rule above. So arb prints the double, and
  `scripts/expr_paths.sh` reports the difference rather than hiding it.

### `%` takes a remainder, it does not truncate

`%` is the f64 remainder of the two operands: `5.5 % 3` is `2.5`. jq truncates
both operands to integers first and answers `2`. The two agree on every integer
input, which is why both harnesses scored parity for `%` until their corpora grew
a fractional value. The probes for it are red, on purpose, and stay that way
rather than being reworded to match either side.

For arb's own expressions (`map x % 3`) the remainder is the settled rule: every
value here is an f64, and truncating the operands would be the one place the
language stopped believing that.

What is NOT settled is the jq context. `%` also reaches the jq front-end — a
leading `.` routes the whole body there — and lowers to the same fusevm op, so
`out { in.json; .n % 3 }` on `{"n":5.5}` answers `2.5` where jq answers `2`. That
front-end promises jq's answers, and arb already settles this exact kind of
collision by CONTEXT rather than by picking one meaning globally: `. | keys` is
jq's sorted array while bare `keys` is the native verb, and the same split covers
`flatten` and `to_entries`/`entries`. Whether `%` should join them is open;
`scripts/jq_parity.sh` probes it so the difference is reported either way.

## 7. Pipe & sources

```
in                       stdin (lines)
in.json in.xml in.html in.yaml in.toml in.csv   parsed stream
spawn ps aux             arb launches a process; its stdout is the stream ✅
spawn { tail -f a.log; grep err }   block form → one `sh -c` string ✅
! vmstat 1 every 1s      re-run a command on a timer ✅ (headless runs it once)
< file.log               read a file as the stream ✅ (quote absolute paths: `< "/var/log/x"`)
out { … }                downstream emission (to | next), shaped by controls
spawn -pty CMD           run CMD on a pseudo-terminal (acts interactive) ✅
send "text"              write to a `spawn -pty` child's stdin (Expect) ✅
```

`spawn CMD` (or `spawn { CMD }`) makes a spec self-sourcing: arb runs CMD via
`sh -c` and feeds its stdout into the stream in place of stdin (fire-and-forget;
the child dies with arb). `< FILE` reads a file as the stream instead (folded to
`cat -- FILE`). `! CMD every Ns` re-runs CMD on a timer, feeding each run's
output into the stream — in the TUI/served dashboard it loops in the background;
headless (piped onward) it runs CMD **once**, because a reducer/emit over an
endless timer source could never terminate. At most one stream source may be
declared (`spawn`/`< FILE`/`! …`); a CLI `--run 'PROD | _ | CONS'` producer wins
over all of them. **`spawn -pty CMD`** runs the spawn command on a pseudo-terminal
so it behaves as if interactive, and keeps a writer to its stdin so a **`send
"text"`** action (a bind/expect action, like `set`/`beep`/`exec`) can drive it —
Expect-style automation, e.g. `expect { /password:/ send "hunter2\n" }`. The
`send` write happens in the TUI (headless falls back to a plain pipe). The
`sel` selection widget (§9) exposes a widget's highlighted row as `.<path>.sel`
for a `send`/`where`/`tell` to consume.

## 8. Query — jq/xpath/css/yq superset (uniform over all formats)

```
field NAME        key (jq .name); field a b c = a.b.c; field N = Nth ws column
fields N M …     project/reorder whitespace columns (1-based): fields 1 3 -> cols 1 and 3
each              iterate (jq [])
find TAG          recursive descent (xpath //)
attr NAME         attribute (xpath @, css)
sel { CSS }       CSS selector (html)
where(PRED)       filter (jq select)
pick a b c        project object to keys (jq {a,b,c}); keeps listed order
b64             base64-encode each line
b64d            base64-decode each line (invalid passes through)
hex             lowercase hex-encode each line (byte-wise)
unhex           hex-decode each line to UTF-8 (invalid passes through)
urlenc          percent-encode each line
urldec          percent-decode each line
extract /re/    first regex match per line (capture group 1 if any); no-match dropped
split DELIM     explode each line by DELIM into one line per part
substr A B      character substring [A,B) 0-based, clamped
chars           explode each line into one char per line
title           title-case each line
repeat N        repeat each line's content N times
set K V         set json object key K to string V
del K           remove json object key K (jq del)
rename OLD NEW  rename json object key OLD to NEW
default K V     set json object key K to V only if absent
merge           merge all json objects into one (later keys win)
floor           floor each numeric line
ceil            ceil each numeric line
clamp LO HI     clamp each numeric line into [LO,HI]
abs             absolute value of each numeric line
round           round each numeric line to the nearest integer
commafy         thousands-group each numeric line (1234567 -> 1,234,567)
bytes           humanize a byte count, 1024-based (1536 -> 1.5 KB)
duration        humanize seconds as the two largest units (3661 -> 1h 1m)

delta           consecutive differences of the numeric series (n -> n-1) — a counter's rate-of-change
cumsum          running (cumulative) total of the numeric series
sma N           trailing simple moving average, window N (length-preserving; smooths a noisy series)
ewma A          exponentially-weighted moving average, smoothing factor A in (0,1] (s0=x0)

median          median of numeric lines (reducer)
stddev          population standard deviation (reducer)
percentile N    Nth percentile 0-100, nearest-rank (reducer); p50 p90 p95 p99 are sugar
range           max minus min of numeric lines (reducer)
product         product of numeric lines (reducer)
bins N          bucket numeric lines into N equal-width bins -> (label -> count) pairs

apply .name     splice an `input .name` widget's live value in as a sub-pipeline (megafilter/map)

sort_by F   stable-sort json records by field F (numeric when all values parse, else lexical; non-objects last)
unique_by F   keep first JSON record per distinct value of field F (dedup by F)
count_by F    count json records grouped by field F (value -> count, count desc)
min_by F      return the JSON record whose numeric field F is smallest
max_by F     emit the record with the largest numeric field F (reducer)
has KEY          keep only JSON-object lines that contain key KEY
entries          jq to_entries: emit {"key":k,"value":v} per key of each JSON object line
flatten          flatten a JSON array to its leaves, ALL levels (jq flatten), one leaf per line
add               jq add: sum a numeric JSON-array line, concat a string array, [] -> ""
over N          keep numeric lines strictly greater than N (drops non-numeric)
under N            keep numeric lines strictly less than N
between A B   keep numeric lines x with A <= x <= B (inclusive), drop the rest
enumerate         prefix each line with its 1-based index and a tab
words                split each line on whitespace into one word per line (flatten)
dedup                collapse adjacent duplicate lines to one (classic uniq)
tailn N       keep the last N lines (complement of take)
pad N            right-pad each line with spaces to a minimum width N (no truncation)
lpad N          left-pad each line with spaces to minimum width N
grepf FIELD /re/   keep lines whose FIELD (json key or 1-based ws column) matches /re/
flip            reverse the characters of each line (Unicode scalar reversal)

keys  vals        jq keys/values
map(FN)           jq map: `[.[] | FN]` — returns one ARRAY line, scoped per input
count sum min max avg tally    aggregates
sort sort_by(FN) group_by(FN) uniq
over N  under N   numeric threshold
index N  slice A B  positional
```

| jq/xpath/css | arb |
|---|---|
| `.users[].name` | `field users; each; field name` |
| `.items[] \| select(.price>10)` | `field items; each; where(price>10)` |
| `//a/@href` | `find a; attr href` |
| `div.card h2` | `sel {div.card h2}` |

**Literal front-ends.** The right column is arb's native form, but inside a
`source { … }` / `out { … }` body you may write the **jq** or **xpath** literal
directly — it compiles to the same ops:

```
out { in.json; .users[] | select(.age >= 18) | .name }   # jq: path, iterate, filter
out { in.json; map(.price); add }                        # jq map() -> one array, then a native verb
out { in.html; //a/@href }                               # xpath: descendant + attribute
out { in.html; //div[@class]//span/text() }              # xpath: predicate + child + text()
```

**The whole jq language.** arb implements jq, not a subset of it: identity `.`,
key/path `.foo.bar`, iterate `.[]`/`.foo[]`, index `.[N]` (and negative `.[-1]`),
slice `.[a:b]`, pipe `|`, the comma operator `.a, .b`, object and array
construction (`{a: .a}`, `[.a, .b]`), parentheses, `if`/`then`/`elif`/`else`,
`try`/`catch` and the `?` suppressor, the `//` alternative, `..` recursive
descent, `as` bindings including array/object destructuring and the `?//`
alternative, `reduce` and `foreach`, `label`/`break`, `def` with filter and value
parameters (and recursion), `$ENV`/`$__loc__`, string interpolation `\(…)` and
every `@format`, the whole assignment family (`=`, `|=`, `+=`, `-=`, `*=`, `/=`,
`%=`, `//=`), path expressions (`path`, `paths`, `getpath`, `setpath`,
`delpaths`, `del`, `pick`, `to_entries`/`from_entries`/`with_entries`), the regex
family (`test`/`match`/`capture`/`scan`/`split`/`splits`/`sub`/`gsub`), the
stream builtins (`tostream`/`fromstream`/`truncate_stream`, `input`/`inputs`,
`limit`/`first`/`last`/`nth`/`until`/`while`/`repeat`/`isempty`), and the
date and libm surfaces.

That is not a list to be taken on trust. `scripts/jq_parity.sh` carries a
`superset_probe` that compares jq's own `builtins` output against arb's and fails
on any `name/arity` jq defines and arb does not; it currently reports none
missing. On its first run it reported 44, which is how `JOIN`, `bsearch`, `skip`,
`toboolean`, `trimstr`, `format` and the libm names from `acosh` to `yn` came to
be implemented. Every other construct is byte-diffed against `jq -rc` on the same
input, and a construct applied to the WRONG TYPE must be refused by both engines
— with jq's refusal checked, so a refusal arb invented alone is never scored as
parity.

Two engines sit behind that. A jq literal that maps onto arb's line-stream ops
(a path, an iterate, a `select`, a `map`) is translated to them, which is what
keeps arb's own promises about a non-JSON line. Everything else compiles to a
program on arb's jq engine, whose value model is jq's: object keys keep INSERTION
order (`keys_unsorted` and `to_entries` expose it), and a number keeps the source
LITERAL it was read with until arithmetic touches it, printed in decNumber's
canonical form — so `1.50` stays `1.50`, `1e2` prints as `1E+2`, and
`12345678901234567890` round-trips.

Names resolve at COMPILE time, as jq's do. A word that is neither an arb verb nor
a jq builtin is still arb's `unknown verb`, not a jq runtime failure — a typo'd
arb verb is far likelier than a jq program, and that diagnostic is what points at
the real mistake.

Supported xpath: descendant `//tag`, child `/a/b`, chain `//a//b`, the
`[@attr]` existence, `[@attr='v']`/`[@attr="v"]` equality (either quote) and
`[contains(@attr,'v')]` substring predicates, union `//a|//b`, and the `/@attr` /
`/text()` accessors, plus a standalone `@attr` step, which is arb's line-stream
continuation (`//a; @href`) rather than XPath's `attribute::` axis from the
document node. **Anything outside the documented xpath subset is a hard error**
(`xpath: …`) anchored to the offending verb — never silently reinterpreted (no
positional/text predicates, no axes, no `*` wildcard step).

Where a jq builtin shares a spelling with an arb NATIVE verb (`sort`, `min`,
`max`, `floor`, `abs`, `keys`, `length`, `add`, `flatten`, `first`, `last`,
`split`, `join`, `del`, `index`, `contains`, `range`, `match`, `repeat`, …) the
BARE word is the native verb and the CALL spelling is jq's — `sort_by v` is
arb's, `sort_by(.v)` is jq's, and `. | sort` reaches jq's. That is the context
rule below, not a jq leak.

**`#` inside `sel { … }` is an ID, not a comment.** `#` opens a comment to
end-of-line wherever a command is expected, and `sel`'s brace used to be re-lexed
as commands — so `sel { #main }`, a spelling the docs advertise, lexed to an
EMPTY block and reported `sel: expected a CSS selector` (exit 1) instead of
selecting the element. `sel` is the one brace in the language whose contents are
not commands (they are a CSS selector), so it is re-lexed with the comment rule
off and `sel { #main }`, `sel { #main p }` and the selector list
`sel { #main p, #two }` all select what xmllint's equivalent XPath selects. Every
other brace holds commands and keeps `#` as a comment.

**Two measured deviations remain, neither of them silent.** A YAML number keeps
its value but not its LITERAL, so `ratio: 1.50` prints as `1.5` where
`yq -o=json` prints `1.50`; the JSON path does preserve it. And the bare `keys`
spelling is the native verb, described in full below. Both are probed and
reported as divergences every run rather than allowlisted.

**One deviation runs the other way.** For an integer above 2^53, jq's own
arithmetic loses up to an ULP: `jq` answers `true` to
`(-516424571754902561 + 0) == -516424571754902500` when the correctly-rounded
double is `…600`. arb reads every number with Rust's correctly-rounded parser and
prints the shortest decimal that round-trips, so it answers `…600` and differs
from the reference by being right. It is not "fixed" to match, and the generated
sweep in `tests/jqlang.rs` states the tolerance explicitly: the computed path may
differ from jq ONLY above 2^53, and byte-matches everywhere else.

**jq expression bodies are jq VALUES.** A `select(…)` predicate, a `map(…)` body
and a bare arithmetic stage evaluate over JSON values, not over arb's f64
expression language (§6):

* a comparison yields a boolean — `map(. > 1)` is `[false,true,true]`, not `[0,1,1]`;
* `select` uses jq truthiness, where only `false` and `null` are falsy, so
  `select(.a)` keeps `0`, `""`, `[]` and `{}`;
* `==` compares type as well as value (`1` is not `"1"`), and an absent key is
  `null`, so `select(.b == null)` is how "field missing" is spelled;
* `+` is overloaded per type — number add, string concat, array concat, object
  merge, and `null` as the identity on either side — with `-` on arrays (set
  difference), `*` on objects (recursive merge) and on a string (repetition),
  and `/` on two strings (split);
* `%` truncates both operands to integers first, as jq's does. Arb's own
  `map x % 3` keeps §6's f64 remainder; the jq context follows jq, which is the
  same context gating that separates jq `to_entries` from native `entries`;
* nested field paths work inside a body (`select(.a.b > 1)`, `map(.a.b)`), and a
  path may continue THROUGH an iterate (`.users[].name`, `.a[].b.c`);
* a top-level JSON STRING renders RAW, the way `jq -r` prints one: a line reading
  `"hello"` is `hello`, and `"a\"b"` is `a"b`. That holds for the filters that
  emit their input line unchanged — identity `.`, `select(…)` and `values` — as
  well as for the ones that already rendered (`.a`, `.[]`, a slice). Other types
  keep the source line, which is also what jq does with a number it never computed
  (`1.50` stays `1.50`);
* a TYPE mismatch is a hard error, not an answer. `null | .[]`, `3 | .a`,
  `"hello" | .[1]`, `true | length`, `{"a":1} | . + 3`, `.n / 0` and `.n % 0` all
  refuse with `jq: …` on stderr and a non-zero exit, matching what `jq` itself
  does on the same input (exit 5, as jq uses).

Those type rules apply to a line that PARSES as JSON. arb's stream is TEXT and
`jq` has no reading of a non-JSON line at all (it refuses the whole input), so
arb keeps its line-stream behaviour there rather than inventing one: a path
yields `null`, an iterate/slice passes the line through, and an EXPRESSION sees
the line as jq's string — `. * 2` over a line reading `abc` is `abcabc`.

A compare may test strings as well as numbers — `select(.status == "ok")`, and the
ordered forms too (`select(.s < "abd")`), which follow jq's total order
(`null < false < true < numbers < strings < arrays < objects`) — and a key that is
not a bare identifier is reachable through the bracket form, `.["a b"]`. A
subscript keeps its type: `.["0"]` is an object key and `.[0]` is an array index,
so `[1,2] | .["0"]` refuses rather than reading the first element. Path results
follow jq on absent data: an explicit null, a missing key and an out-of-range
index all render as `null`, not as an empty line. (Native `field NAME` is
unchanged — it still yields "" on a miss and still falls back to logfmt, because
it runs over plain-text streams as well as JSON.)

**Where a jq literal and a native verb share a spelling.** arb's pipeline is a
LINE stream, so a jq filter returning one array is emitted as one compact array
line, while the native verb of the same family emits a line per element. The two
spellings are kept distinct wherever they differ:

| spelling | shape | note |
|---|---|---|
| `to_entries` (jq) | one `[{"key":…,"value":…},…]` line | matches jq |
| `entries` (native) | one line per key | SPEC §8, unchanged |
| `. \| keys` (jq) | one `["a","b"]` line | matches jq |
| `keys` (native verb) | one line per key | **does not match jq**, which returns one array |
| `. \| add` (jq) | folds from `null`, so `[]` is `null` | matches jq |
| `add` (native verb) | `[] -> ""`, string-joins a mixed array | SPEC §8, unchanged |
| `. \| length` (jq) | refuses a boolean | matches jq |
| `length` (native verb) | falls back to the raw line's char count | runs over plain text too |

The rule is CONTEXT, not spelling: a body command that begins with a jq literal
(`.`, `select(`, `map(`, `has(`) is handed to the jq front-end whole, and inside
it every builtin answers as jq does. A bare alphanumeric word is a native verb.
So `. | keys` is jq's sorted array while `keys` on its own is arb's line-per-key
verb — the same gating that already separates jq `to_entries` from native
`entries`, applied to the one spelling that carries both meanings.

`keys` is therefore resolved in jq context and **deliberately left as the native
verb in the bare spelling**, which is where `scripts/jq_parity.sh` reports a
divergence every run. Three things decide it, and they point the same way:

* Making the bare word mean jq's array would break a shipped preset.
  `stdlib/json.arb` runs `keys; tally` over the line-per-key shape and pins it
  with its own in-language test (`arb --test stdlib/json.arb`).
* `keys` is not a lone wart. The bare word is the native verb for EVERY shared
  spelling — `sort`, `min`, `max`, `floor`, `abs`, `add`, `length`, `entries` all
  behave this way, and the jq front-end refuses `sort`/`min`/`max`/`floor`/`abs`
  by name rather than answering as jq. Special-casing `keys` alone would make it
  the single exception to a rule that currently holds without exception.
* Nothing is unreachable. `. | keys` already gives jq's sorted array, so the jq
  meaning has a spelling; only the DEFAULT for the bare word is at issue.

So the divergence is a naming collision, not a missing capability, and the cost of
"fixing" it is a broken preset plus an inconsistent context rule. A distinct
native spelling would retire it, and none exists yet; until then the probe stays
red rather than being reworded into a pass.

### In-language unit tests (`arb --test`)

A spec can carry its own tests: a `test "NAME" { … }` block feeds sample lines
through a query pipeline and asserts the output. `arb --test spec.arb` runs every
block headlessly and exits 0 (all passed) / 1 (any failed), with
[TAP](https://testanything.org/) output — so a dashboard's transforms are
regression-tested in CI, in the same language they're written in.

```
test "keeps 5xx, drops others" {
    given "200 ok" "503 down" "404 x" "500 err"   # input lines (one per arg)
    run { in; match /5\d\d/ }                     # the pipeline (reuses source/out grammar)
    want "503 down" "500 err"                      # expected output lines
}
test "counts errors" {
    given "e" "e" "ok" "e"
    run { in; match /e/; count }                  # a reducer → one scalar line
    want "3"
}
test "jq path" { given "{\"u\":{\"name\":\"bob\"}}"; run { in.json; .u.name }; want "bob" }
```

`run { … }` is the ordinary `source`/`out` body — native verbs and the jq/xpath
literal front-ends are all testable. The output is flattened for comparison: a
scalar renders as one line, `tally`/`count_by` pairs as `key\tvalue`. Test blocks
are ignored by every mode except `--test` (they don't render a widget).

## 9. Widgets ("Tk" register)

```
text .t -label L          tail .t -label L        table .t -cols "a,b,c"
list .t                   gauge .t -label L -max N spark .t
bars .t -label L          histo .t                chart .t
linegauge .t -max N       scatter .t              sparkline .t   # bars / scatter
map .t -res high           calendar .t             # world map / month calendar
logview .t                 heatmap .t              treemap .t     # log / grid / rects
gantt .t                   diff .t                 # time spans / +- coloring
logo .t                    clear .t                rule .t        # splash / spacer / divider
select .s -prompt P -header H     input .i -placeholder P    # interactive
sel .ps                   # per-widget selection list -> .ps.sel (§14)
tabs .t -tabs {a b}       block .t -title T -border  frame .f
.t configure -max 200     # reconfigure (merge opts into a declared widget)
```

Any widget takes `-color NAME` (green/red/yellow/orange/magenta/blue/white/gray,
default cyan) to tint its border and accent — same color in the TUI and web.
`select` is an interactive fuzzy picker (fzf as a one-widget spec; `source`
projects the candidate display, `search` derives a separate match key); `input`
is a live field whose value drives `apply`/`bind`/`out`. The chart family:
`linegauge` (a compact one-line `gauge`), `scatter` (braille scatter of a numeric
series), `sparkline` (a block-bar sparkline, the fixed-height counterpart to
`spark`'s braille line), `map` (a braille world map plotting `lon lat` points from
the stream — geo scatter), and `calendar` (a month grid highlighting days that
appear as `YYYY-MM-DD` in the stream). Scrollable list widgets (`tail`/`list`/
`block`/`frame`) draw a scrollbar when content overflows. `sel` is an in-dashboard
selection list over its own `source` — Up/Down (or a click) move a cursor and its
highlighted row is published as the control value `.<path>.sel`, readable from
`where`/`apply`/`tell`/`send` (the per-widget-named-source selection accessor).

arb wires the full ratatui data-widget set — `Paragraph`, `List`, `Table`,
`Tabs`, `Gauge`, `LineGauge`, `BarChart`, `Sparkline`, `Chart`, `Canvas`
(scatter/map), `Calendar`, `Scrollbar`, `Block` — leaving only the non-data
utilities (`Clear`, `RatatuiLogo`) unused.

## 10. Layout (auto by default)

```
# no grid → widgets auto-tile (vertical flow). Only add geometry to override.
layout horizontal                      # auto-tile in a row instead of a column
grid .a -row 0 -col 0 -span 2          # -span = colspan; -rowspan/-colspan explicit
grid .b -row 1 -col 0

# Proportional tracks (Tk `grid`-style, ratatui Constraints under the hood):
rows "1 2 1"                            # 3 rows; the middle is 2× tall (weights)
cols "20c * 2*"                          # col 0 fixed 20 cells, col 1 weight 1, col 2 weight 2
gap  1                                  # 1 blank cell between every row/column
```

A **track** is `N` (a proportional weight → `Fill`; the common case), `Nc` (a fixed cell count → `Length`), `N%` (a percentage of the
axis → `Percentage`), or `N*` / `*` (a proportional weight → `Fill`; bare `*` is
weight 1). `rows`/`cols` size the grid's tracks (unset = equal weights); a shorter
spec sizes the leading tracks and the rest fill. `gap N` inserts blank cells
between tracks. Without any `grid` cell, widgets auto-tile in the `layout`
direction (`vertical` default, `horizontal` for a row), sized by the flow-axis
track spec when given. The **served web dashboard uses a responsive CSS grid** —
`rows`/`cols`/`gap`/`layout` shape the terminal TUI.

## 11. Binding

```
source .t { in.json; each; where(is5xx); count; every 1s }   # stream → widget via query
.g <- cpu_pct                     # reactive: widget follows a signal
.t <- now() every 1s              # sampled bind
```

## 12. Interactive controls — megafilter/map the passthrough

Controls render AND feed `out`. A control's path used as a value = its current state.

```
filter .q                 # text box  → .q string
facet  .lv -field level   # facets    → .lv selected set
slider .th -field lat -min 0 -max 5s   # → .th value
check  .on -label live     select .k -opts {a b c}

out {
    where(match(.q))            # filter by text box
    where(level in .lv)         # filter by facet selection
    where(lat < .th)            # filter by slider
    map(x => pick(x, {ts msg})) # MAP the passthrough, not just filter
}
```

## 13. Expect — stream reactions

A matching stream line fires an action (space-form args, per §2 — not paren calls):

```
expect /5\d\d/ { alert "5xx"; flash .log red }   # regex → a block of actions
expect /panic|OOM/ beep                          # …or a single action
expect /down/ exec "notify-send arb"
timeout 5s alert "stream idle"           # fire when no new line for 5s (Ns/Nms/Nm)
# actions: set .name V | quit | beep | alert MSG | flash .w COLOR | exec CMD
#          | send "text" (to a spawn -pty child) | { … } (a block, run in order)
```

The block form groups several clauses under one `expect`:

```
expect {
    /panic|OOM/ { alert "crash"; flash .log red }   # each clause: /re/ ACTION
    /5\d\d/     beep
    /shutdown/  quit
}
```

`spawn CMD` (SPEC §7) launches a process whose stdout feeds the stream;
`spawn -pty CMD` runs it on a pseudo-terminal and a `send "text"` action writes
to its stdin, so an `expect { /re/ send "…" }` clause automates it (Expect).

## 14. Events — bind (Tk)

```
bind C-q quit                       # a control key → an action (any §13 action)
bind <Enter> quit                   # Tk named keys: <Enter> <Esc> <Tab> <Key-x>
bind <Click> beep                   # any mouse press → an action
bind <Resize> { alert resized }     # terminal size change → an action
bind C-r { alert reloaded; beep }   # block form
```

**Mouse** (SGR reporting, enabled on the TUI's `/dev/tty`): **left-click** a
control to focus/toggle it (checkbox, facet option), click-drag a `slider`,
click a `tabs` label to select it, click an fzf row to move the cursor
(**double-click** a row to pick it, like Enter). **Right-click** a control
resets it to its empty/default (slider→min, checkbox→off, text/facet→cleared).
**Middle-click** focuses without acting. The **wheel** scrolls: over a
scrollable widget (`tail`, `list`, `text`, `table`, `block`, `frame`) it banks
older rows and walks back toward the live tail; elsewhere it moves the focused
`facet` cursor; in fzf mode it moves the selection. The raw button byte also
carries **Shift/Alt/Ctrl** modifier bits (`mouse_shift`/`mouse_alt`/`mouse_ctrl`).
Everything is decoded from the raw tty byte stream and hit-tested against the
rendered widget rects. To copy text, hold **Shift** and drag for your terminal's
native char-precise selection (arb captures the mouse for its widgets).
**Selection accessor** — a `sel` widget (a per-widget selection list over its own
`source`) publishes its highlighted row as the control value `.<path>.sel`, so a
`ps aux | arb` dashboard with `sel .ps` + `source .ps { in }` exposes `.ps.sel` =
the current process row. Move the cursor with Up/Down (or click a row); the value
updates live and is readable anywhere a control is — `where match(.ps.sel)`,
`apply`, `tell w job(.ps.sel)`, or a `send` to a PTY child. ⬜ OSC-52 whole-widget
copy is deferred (line-granular, terminal-gated — Shift+drag is the better path).

## 15. Actors — Akka-style concurrency

An **actor** is a named behavior with a single scalar `state` and one handler per
message. Handlers run arb expressions (the same fusevm-lowered core as
`map`/`where`/`calc`) over `state`, the message parameters, and any locals they
assign; `reply EXPR` sends a value back to an `ask`/`via` caller.

```
actor worker(state) {
    on job(x) {
        state = state + 1        # per-worker running count
        reply x * x              # value sent back to ask / via
    }
    on reset { state = 0 }
}
```

Handlers are separated like every other verb — a newline or `;` (a `{ … }` block
does **not** end a command, so two handlers on one line is an error).

**Runtime** — one OS thread per actor, each blocking on an `mpsc` mailbox:

| Operation | Meaning |
| --- | --- |
| *spawn* | start an actor with an initial `state`; returns a mailbox ref |
| *send* (tell) | post a message, fire-and-forget |
| *ask* | post a message with a one-shot reply channel, block for `reply` |
| *pool* | a supervised round-robin pool of N copies; a worker whose thread dies is respawned on the next dispatch |

**Pipeline fan-out (`via`)** is the stream-facing consumer — it fans the passthrough
across a pool in parallel, each line's scalar becoming a `job(x)` ask whose reply
is the output line, order preserved:

```
source .out { in; via worker * 8 }   # 8-worker pool
out       { in; via worker }          # default: one worker per hardware thread
```

A pure-map handler (`reply x * x`) is deterministic regardless of pool width. A
handler that mutates `state` is partitioned across workers, so with `N > 1` a line
sees only the state of the worker that handled it (documented non-determinism —
use `via worker * 1` for a single sequential accumulator).

**Session refs + event-driven messaging** — spawn long-lived actors/pools in the
spec and drive them from key/stream/timer events:

```
actor worker(state) { on job(x) { state = state + 1; reply x * x } }

spawn w = worker(0)                # a session actor ref
pool  p = worker * 8              # a supervised session pool
supervise p { on crash { stop } }  # fail-stop (default is restart = respawn)

input .out                                   # a control widget to show a reply
bind C-t tell w job(5)                        # tell: fire-and-forget on a keypress
bind C-a ask .out w job(.th)                  # ask: reply written into `.out`
expect /error/ tell w job(1)                  # drive an actor from a stream match
```

| Form | Meaning |
| --- | --- |
| `spawn NAME = ACTOR(init)` | a single session actor, initial `state` = `init` |
| `pool NAME = ACTOR * N` | a supervised N-worker pool (`* N` optional → one per thread) |
| `supervise NAME { on crash { restart \| stop } }` | crash policy; `restart` (default) respawns a dead worker, `stop` is fail-stop |
| `tell REF MSG(args)` | action: post a message, fire-and-forget |
| `ask .CTRL REF MSG(args)` | action: ask, write the reply into control `.CTRL` |

`args` are arb expressions evaluated when the action fires, with live control
substitution (`.th` → that input's current value). `spawn NAME = …` is
disambiguated from the `spawn CMD` process source by the `=`. The message tell is
`tell` (not `send`) because `send` is the Expect PTY action (§14). Session refs
run only in the interactive TUI; the served web target renders widgets but does
not fire bind/expect actions.

The runtime lives in `src/actor.rs`: declaration + handler compiler + one
`mpsc`-mailbox thread per actor (`spawn`/`send`/`ask`/`pool`, plus `Session` for
the named session refs). `via` is `QueryOp::Via`, evaluated by `actor::run_via`
(rayon fan-out over the pool). `heavy(x)`-style user functions are not part of
arb's expression grammar — handler bodies use arb expressions directly.

## 16. Targets

```
target tui                 # default
target web -port 8080      # served page + WS
theme neon-noir            # one of 31 built-in palettes (arb --list-themes)
theme custom 201 231 93 219 57 53   # a custom 6-index (256-color) palette
set refresh 250            # ms redraw throttle
```

**Themes** — `theme NAME` sets the active color palette from the 31 built-ins
(the storageshower HUD palettes shared with the sibling `iftoprs`/`htoprs` apps:
`neon-sprawl`, `acid-rain`, `neon-noir`, `blade-runner`, `night-city`,
`megacorp`, `zaibatsu`, `iftopcolor`, … — `arb --list-themes` prints them with
swatches). Each is a 6-color palette `(primary, accent, alt, mid, dim, bg)` of
256-color terminal indices; `theme custom c1 c2 c3 c4 c5 c6` supplies your own.
With a theme active the whole TUI recolors from the palette as one system — a
widget with no `-color` takes a slot chosen by its kind (value gauges → accent,
bars → alt, series/plots → mid, text/containers → primary) so a dashboard is
multi-colored like the iftop/htop HUD rather than monochrome, and a themed fzf
picker maps the palette onto fzf's own colour slots (rows → primary, matches →
accent, cursor bar → bg, counter/header/separator → dim). `arb --fzf` starts from
**fzf's palette**, not the arb theme, unless a theme is asked for explicitly
(`--theme NAME`, a spec `theme` directive, or the live `Ctrl-T` chooser) — the
drop-in has to look like `fzf`. An explicit `-color <slot>` (`accent`/`primary`/`alt`/`mid`/`dim`/
`bg`) resolves through the palette too. The fixed semantic names (`-color green`
/`red`/…) remain theme-independent explicit overrides.

**A theme is always active by default** (matching the sibling `iftoprs`/`htoprs`
apps, which default to `neon-sprawl`), so every dashboard — including the stdlib
presets, which set no `-color` — is themed out of the box. Resolution precedence:
`--theme NAME` (per-run) → the spec's own `theme` directive → the `~/.arb/config.toml`
`[ui] theme` global default → the baked `neon-sprawl`. Set the global default with
**`arb --set-theme NAME`** (persists to the config), preview all 31 with
`arb --list-themes`, and opt out to the classic cyan look with `theme off` (or
`--theme off`). At runtime, **`Ctrl-T`** cycles the theme live in every mode
(dashboard, form, and the fzf picker) and saves the choice to `~/.arb` — a
control key, because a bare letter would be swallowed by the megafilter / a text
input. **`Ctrl-G`** toggles a help overlay of the global keys plus the spec's
`bind`s. The served web dashboard keeps zgui-core's own colorscheme picker.

Ships today as `arb --serve --port N`: a std-only HTTP server renders the same
spec as a live browser dashboard, pushing widget data over a WebSocket (hand-rolled
handshake, no dependency) with automatic fallback to polling `/data`. The page is
built from **[`zgui-core`](https://github.com/MenkeTechnologies/zgui-core)** — the
shared cyberpunk web-component toolkit — vendored as a git submodule at
`lib/zgui-core` and bundled into the binary at build time (`build.rs` →
`include_str!`). The page mounts `ZGui.appShell` (splash, filter bar, ⌘K palette,
settings/colorscheme) and renders each widget with the matching component:
`gauge`→`ZGui.gauge`, `chart`→`ZGui.chart`, `spark`→`ZGui.sparkline`,
`bars`/`histo`→`ZGui.statBars`, `table`→`ZGui.dataTable`, containers/log →
`ZGui.card`+`ZGui.logView`, fed live from `/data`. `input` widgets render as
editable fields that `POST /set?name=..&value=..` on change; the server holds a
live input store and re-resolves each widget's pipeline against it every frame,
so a typed field reshapes the browser dashboard just like the TUI megafilter.

## 17. Modules & presets (presets = stdlib script imports)

```
import http                # stdlib or user module by name
import "./mylib.arb"       # file
import gauges as g         # namespaced

# resolution: local → ~/.arb/lib/NAME → installed pkg (~/.arb/pkg/NAME) → stdlib
```

- **Dashboard module (preset):** top-level widget/source/layout stmts. `arb -p http` == `import http`.
- **Component module (library):** exports `fn`s that build widget-groups: `g.cpu(.c)`.

Both are just `.arb` files. Compose: `import http; gauge .mine …; grid .mine -row 2`.

## 18. Package manager (ported from stryke `[ffi.exports]`/`load_cdylib` + znative ABI)

`arb.toml`:
```toml
[package]
name = "arb-k8s"
version = "0.1.0"
license = "MIT"

[deps]
arb-http = "0.2"

[exports]                     # ← stryke.toml [ffi.exports]
modules = ["pods", "nodes"]   # .arb dashboards + components

[exports.native]              # ← znative / load_cdylib
widgets = ["flamegraph"]      # cdylib: new widgets / formats / actors
formats = ["protobuf"]
```

Kinds: **script packages** (pure `.arb`) and **native packages** (`cargo add arb-native`, ship cdylib, stable versioned ABI).

```sh
arb install arb-k8s   arb add arb-http   arb publish   arb search k8s   arb update
```

Distribution: native → crates.io (like fusevm/znative); script → git index + GitHub repos (like the stryke-* family).

**Ships today** (std-only, `git` subprocess as transport — no in-process TLS):
`arb update` clones/pulls the index repo into `~/.arb/registry`; `arb search Q`
greps its `index.json`; `arb install NAME` / `arb add NAME` `git clone`s the
package into `~/.arb/pkg/NAME` and validates its `arb.toml` + entry module before
keeping it (rolled back on failure); `arb uninstall NAME`. A package's `[deps]`
are resolved recursively from the same index, with each dep's version-constraint
**checked** against the index version (`semver`), a visited-set cycle guard,
skip-already-installed, and full rollback of the run if any dep fails or a
constraint is unsatisfiable. The module resolver reads `~/.arb/pkg` as the §17
`pkg` tier, so `import NAME` finds an installed package. **`arb publish
[GIT_URL]`** registers the package for real: it validates it, fast-forward-pulls
the index clone, upserts the package's `{repo, version, desc}` entry into
`index.json`, commits, and pushes to the index remote (default
[`github.com/MenkeTechnologies/arb-registry`](https://github.com/MenkeTechnologies/arb-registry)).
With write access the entry lands directly; without it the commit stays local and
arb prints the fork+PR flow — it never falsely claims a push succeeded.
`GIT_URL` defaults to the package repo's `origin` remote. A package declaring
`[exports.native]` is rejected (native/cdylib loading isn't built — never
installed with an inert native half). Full multi-version semver *resolution*
(one index ref per name today, like the crates.io index tip) and native/cdylib
packages remain future work.

## 19. Ecosystem — "a TUI for every pipeline"

Community publishes `arb-<tool>` packages. `cmd | arb` sniffs the upstream command (or data shape) → resolves the matching package → renders. Every common pipeline (docker/kubectl/psql/nginx/git/systemctl/…) gets a shared, installable dashboard. No registry of shareable pipeline TUIs exists today — this is the world-first ecosystem leg.

**Ships today** — zero-config **data-shape sniffing**: `cmd | arb` (no spec, piped) peeks the first stream lines (via a non-blocking `poll`, so it never delays startup or hangs on an idle producer) and auto-selects the matching stdlib preset — JSON object streams → `json`/`logs`/`nginx`, tool headers → `docker`/`top`/`k8s`, git-log → `git`, CSV/TSV → `table` — replaying the peeked lines so nothing is lost, and falling back to the plain tail on no match. The **upstream-command** leg (identifying the producer process by argv) is deferred: the data-shape leg dominates it cross-platform (macOS pipe-peer matching needs fragile FFI), and covers every motivating producer via its header/shape.

## 20. Architecture (fusevm frontend, original — mechanics ported, semantics fresh)

Deps (rubyrs-lean): `fusevm{jit}`, `ratatui`+`crossterm`, `clap`, `regex`, `rayon`; the served web dashboard is **std-only** (hand-rolled HTTP + RFC 6455 WebSocket, no async runtime) and renders with the vendored `zgui-core` toolkit (git submodule `lib/zgui-core`, bundled by `build.rs`); REPL: `reedline`+`nu-ansi-term`+`libc`+`toml`; parsers: `serde_json`/`serde_yaml`/`toml` + `scraper` (HTML/CSS) + `base64`/`percent-encoding`.

Actual tree:

```
src/lexer.rs     Tcl-flavored reader
src/parser.rs    command + block grammar → AST
src/ast.rs       AST types (Command / Arg)
src/spec.rs      spec interpreter: widgets, source/out pipelines, query-verb
                 parse, import resolution, preset library
src/query.rs     jq/xpath/css/yq engine (pipeline eval over every format)
src/actor.rs     actor system (§15): actor/on/reply parse + handler compiler, mpsc-mailbox threads (spawn/send/ask/pool), `via` parallel stream fan-out
src/theme.rs     31 built-in color palettes (storageshower, shared with iftoprs/htoprs) + custom palette; theme-aware color resolution
src/expr.rs      expression layer: fn/lambdas/operators → fusevm::Chunk on the VM
src/stream.rs    stdin ring buffer + stream stats
src/tui.rs       ratatui backend: render, event loop, fzf mode
src/serve.rs     live web server + WebSocket push; renders via zgui-core (appShell + components)
src/web.rs       static HTML snapshot export (--html)
build.rs         bundle lib/zgui-core/webui/*.js + all.css -> one JS/CSS asset, embedded in serve.rs
lib/zgui-core/   git submodule: the shared cyberpunk web-component toolkit (window.ZGui.*)
src/repl.rs      interactive REPL (--repl)
src/pkg.rs       registry client (install/search/update/publish) over a git index
src/lsp.rs       Language Server over stdio (--lsp): diagnostics/symbols/hover/completion/signatureHelp/definition/references/highlight/rename/folding/formatting/semanticTokens
src/dap.rs       Debug Adapter over stdio (--dap): step the stream, regex breakpoints, inspect the paused line/stats/controls
src/cache.rs     rkyv script cache (~/.arb/scripts.rkyv): outer zero-copy rkyv shard, inner bincode AST blob, FxHash+schema key — skips lex+parse for a seen spec
src/banner.rs    startup/help art
src/main.rs      CLI (clap) + dispatch
src/lib.rs       crate root
```

The compute core (expressions, `calc`, `where`) lowers to a `fusevm::Chunk` and
runs on the VM; declarative widget/layout construction is plain Rust and needs no
VM. Language design (lexer/parser/ast/interp/semantics) is arb-original.

A chunk can execute on either of two fusevm tiers — the interpreter, or native
code from the Cranelift block JIT once the chunk has been invoked past
`block_threshold` (default 1). `scripts/expr_paths.sh` runs the expression
corpus through both, pinned via `FUSEVM_JIT_BLOCK_THRESHOLD`, because a
construct the tiers disagree about answers differently for the first row of a
stream than for the rest. One such disagreement is open and is fusevm's, not
arb's: `Op::Div` by a zero divisor yields `Value::Undef` (prints `0`) on the
interpreter and IEEE infinity in compiled code, so `map x / 0` over a stream
prints `0` and then `1.7976931348623157e+308` (the infinity, clamped on output
as above), and `where x / 0 > 1` keeps a different subset of identical lines. arb
does not paper over it locally — the op is shared with every sibling frontend.
The clamp does not hide it either: `Value::Undef` never reaches the non-finite
branch, so clamping moves only the compiled side and the two tiers still differ.

All SPEC modules now have code (script-package registry + the actor system
included; native/cdylib packages remain future work).

## 21. Milestones

Status: ✅ shipped · 🟡 partial · ⬜ planned · ❌ out of scope.

0. ✅ **Walking skeleton** — `echo hi | arb -e 'text .t <- in'`: lex→parse→lower→fusevm→one ratatui widget from stdin.
1. ✅ Core widgets + auto-layout + `source`/query basics.
2. ✅ Presets/imports + stdlib (logs/http/json/table/top/metrics) + module namespacing `import X as Y` (prefixes widget paths, `apply`, control refs, `set`/`flash` targets).
3. ✅ Interactive controls + `out` passthrough shaping (megafilter/map): `input`/`apply`, the `filter`/`facet`/`slider`/`check` control widgets (interactive in both the TUI and the served web dashboard, incl. dynamic `-field` facet candidates), and control-path predicates — numeric `where lat < .th`, string `where match(.q)`, and set `where level in .lv`.
4. ✅ Expect reactions + events/bind — `expect /re/ ACTION` and the multi-clause `expect { /re/ ACTION; … }` block, `bind C-<key> ACTION` with actions `set`/`quit`/`beep`/`alert`/`flash`/`exec` and `{ … }` block form; Tk named keys `<Enter>`/`<Esc>`/`<Tab>`/`<Key-x>`; `timeout Ns ACTION` idle reactions; `spawn CMD` process input source, `spawn -pty CMD` + the `send "text"` action (Expect-style automation of a PTY child). The `sel` selection widget publishes a widget's highlighted row as `.<path>.sel` for `where`/`apply`/`tell`/`send` to consume (§9).
5. ✅ Web target — `arb --serve` HTTP + WebSocket live dashboard rendered with the `zgui-core` component toolkit (appShell + per-widget components); `arb --html` static export.
6. ✅ Actors — Akka-style message-passing (§15): `actor NAME(state) { on MSG(p) { … reply EXPR } }` declarations compiled to `ActorDef`; a real runtime of one `mpsc`-mailbox OS thread per actor with *spawn* / *send* (tell) / *ask* (await reply) / supervised round-robin *pool* (respawns a dead worker); handler bodies run arb expressions (fusevm) over `state` + params + locals. Two surfaces: the `via NAME * N` pipeline op fans the stream across a pool in parallel (rayon), order preserved; and the session-ref surface — top-level `spawn NAME = ACTOR(init)` / `pool NAME = ACTOR * N` bindings, a `supervise NAME { on crash { restart \| stop } }` crash policy, and the `tell REF MSG(args)` / `ask .CTRL REF MSG(args)` bind/expect actions that drive them (interactive TUI).
7. ✅ Package manager — local preset library (`--save`/`--install`/`--uninstall`/`--installed`) + a networked registry over a git index hosted on GitHub (`arb update`/`search`/`install`/`add`/`uninstall`/`publish`, `~/.arb/pkg` resolver tier, transitive `[deps]` with semver constraint-checking). `arb publish` upserts the package's entry into the index and pushes it (default registry `github.com/MenkeTechnologies/arb-registry`). *(native/cdylib packages + multi-version semver resolution: ⬜)*
8. ✅ LSP/DAP — `arb --lsp` ships a full server: diagnostics (real source ranges, UTF-16 columns), `documentSymbol`, `hover`, `completion` (CORPUS verbs + dot-context `.path` names + widget `-flags`), `signatureHelp`, `definition`/`references`/`documentHighlight`/`rename` over widget `.path` names, `foldingRange`, `formatting`, and `semanticTokens/full`. `arb --dap` is a real steppable debugger over the stream model: each incoming line is a step, breakpoints are regex predicates (a `SourceBreakpoint.condition`, or unconditional = single-step), function breakpoints (`setFunctionBreakpoints`) name a query VERB and stop when the paused line reaches that stage — a name arb has no verb for comes back `verified: false` rather than being silently accepted — the stack trace is the query-pipeline stages, scopes expose the matched line + stream stats + control values, and `evaluate` runs arb's real expression evaluator against the paused line. The `program` (spec) and `input` (data file) come from the `launch` request since DAP owns stdio; `stepIn`/`stepOut` collapse to `next` (a stream has no call nesting). Diagnostics anchor to the offending verb even when nested inside a `source`/`out` body (not the enclosing directive). *(per-token argument precision — squiggle the `/re/` itself, not its verb — ⬜)*
