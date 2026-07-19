# arb — SPEC

**arb** is a standalone, original language on **fusevm/JIT** for **visualizing and modifying Unix pipelines**: drop it in a pipe and it spawns a **dynamic TUI (ratatui) or served web page (zgui components)** built from a declarative spec. It is a **jq/xpath/css/yq superset**, an interactive **megafilter/map** over the live passthrough, its own **Tcl/Tk-flavored DSL**, and a **preset library / package manager** so users share dashboards — *a TUI for every pipeline*. (LSP/DAP stdio frontends ship. Actors are out of scope — dataflow/pub-sub belong to `stryke`.)

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
|>             value pipe: xs |> filter(even) |> sum
```

## 7. Pipe & sources

```
in                       stdin (lines)
in.json in.xml in.html in.yaml in.toml in.csv   parsed stream
spawn ps aux             arb launches a process; its stdout is the stream ✅
spawn { tail -f a.log; grep err }   block form → one `sh -c` string ✅
! vmstat 1 every 1s      re-run a command on a timer ✅ (headless runs it once)
< file.log               read a file as the stream ✅ (quote absolute paths: `< "/var/log/x"`)
out { … }                downstream emission (to | next), shaped by controls
send "text"              send input to spawned process (Expect) ⬜
```

`spawn CMD` (or `spawn { CMD }`) makes a spec self-sourcing: arb runs CMD via
`sh -c` and feeds its stdout into the stream in place of stdin (fire-and-forget;
the child dies with arb). `< FILE` reads a file as the stream instead (folded to
`cat -- FILE`). `! CMD every Ns` re-runs CMD on a timer, feeding each run's
output into the stream — in the TUI/served dashboard it loops in the background;
headless (piped onward) it runs CMD **once**, because a reducer/emit over an
endless timer source could never terminate. At most one stream source may be
declared (`spawn`/`< FILE`/`! …`); a CLI `--run 'PROD | _ | CONS'` producer wins
over all of them. The interactive `send`/PTY react leg + the `.ps.sel` selection
widget (§14) remain ⬜ (no PTY substrate yet).

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
flatten          flatten a JSON array, expanding one level of nested arrays
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
map(FN)           transform each
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
out { in.json; map(.price); sum }                        # jq map(), then a native verb
out { in.html; //a/@href }                               # xpath: descendant + attribute
out { in.html; //div[@class]//span/text() }              # xpath: predicate + child + text()
```

Supported jq: identity `.`, key/path `.foo.bar`, iterate `.[]`/`.foo[]`, index
`.[N]`, pipe `|`, `select(…)`, `map(…)`, and the builtins arb already has
(`keys`/`values`/`length`/`add`/`has`/`to_entries`). Supported xpath: descendant
`//tag`, child `/a/b`, chain `//a//b`, the `[@attr]` existence predicate, and the
`/@attr` / `/text()` accessors, plus a standalone `@attr` step. **Anything
outside the documented subset is a hard error** (`jq: …` / `xpath: …`) anchored to
the offending verb — never silently reinterpreted (no reduce, no arithmetic
beyond compare, no positional/value predicates, no axes, no union).

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
select .s -prompt P -header H     input .i -placeholder P    # interactive
tabs .t -tabs {a b}       block .t -title T -border  frame .f
.t configure -max 200     # reconfigure (merge opts into a declared widget)
```

Any widget takes `-color NAME` (green/red/yellow/orange/magenta/blue/white/gray,
default cyan) to tint its border and accent — same color in the TUI and web.
`select` is an interactive fuzzy picker (fzf as a one-widget spec; `source`
projects the candidate display, `search` derives a separate match key); `input`
is a live field whose value drives `apply`/`bind`/`out`.

## 10. Layout (auto by default)

```
# no grid → widgets auto-tile (vertical flow). Only add geometry to override.
grid .a -row 0 -col 0 -span 2          # -span = colspan; -rowspan/-colspan explicit
grid .b -row 1 -col 0
```

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
#          | { … }  (a block runs several in order)
```

The block form groups several clauses under one `expect`:

```
expect {
    /panic|OOM/ { alert "crash"; flash .log red }   # each clause: /re/ ACTION
    /5\d\d/     beep
    /shutdown/  quit
}
```

`spawn CMD` (SPEC §7) launches a process whose stdout feeds the stream; the
interactive `send`/PTY react leg is still ⬜ Planned.

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
⬜ Planned:
`spawn` + a widget's selection (`.ps.sel`); OSC-52 whole-widget copy is
deferred (line-granular, terminal-gated — Shift+drag is the better copy path).

## 15. Actors — Akka-style concurrency

> **❌ Out of scope (not built, not planned).** Dataflow / actors / pub-sub belong
> to `stryke`; arb stays in the UI-generation lane to avoid duplication. The
> sketch below is retained as design rationale only — see §21.

```
actor worker(state) {
    on job(x) { reply heavy(x) }
    on reset  { state = 0 }
}
w    = spawn worker(0)               # ref
send w job(payload)                  # tell
r    = ask  w job(payload)           # ask (await reply)
pool p = spawn worker * 8            # supervised pool

source .out { in | via(p, x => heavy(x)) }   # fan stream across pool (parallel)
supervise p { on crash { restart } }
```

## 16. Targets

```
target tui                 # default
target web -port 8080      # served page + WS
theme cyberpunk            # shared HUD scheme (matches sibling apps)
set refresh 250            # ms redraw throttle
```

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

All SPEC modules now have code (script-package registry included; native/cdylib
packages remain future work). (Actors are out of scope — §21.)

## 21. Milestones

Status: ✅ shipped · 🟡 partial · ⬜ planned · ❌ out of scope.

0. ✅ **Walking skeleton** — `echo hi | arb -e 'text .t <- in'`: lex→parse→lower→fusevm→one ratatui widget from stdin.
1. ✅ Core widgets + auto-layout + `source`/query basics.
2. ✅ Presets/imports + stdlib (logs/http/json/table/top/metrics) + module namespacing `import X as Y` (prefixes widget paths, `apply`, control refs, `set`/`flash` targets).
3. ✅ Interactive controls + `out` passthrough shaping (megafilter/map): `input`/`apply`, the `filter`/`facet`/`slider`/`check` control widgets (interactive in both the TUI and the served web dashboard, incl. dynamic `-field` facet candidates), and control-path predicates — numeric `where lat < .th`, string `where match(.q)`, and set `where level in .lv`.
4. ✅ Expect reactions + events/bind — `expect /re/ ACTION` and the multi-clause `expect { /re/ ACTION; … }` block, `bind C-<key> ACTION` with actions `set`/`quit`/`beep`/`alert`/`flash`/`exec` and `{ … }` block form; Tk named keys `<Enter>`/`<Esc>`/`<Tab>`/`<Key-x>`; `timeout Ns ACTION` idle reactions; `spawn CMD` process input source. *(interactive `send`/PTY react + `.ps.sel`: ⬜)*
5. ✅ Web target — `arb --serve` HTTP + WebSocket live dashboard rendered with the `zgui-core` component toolkit (appShell + per-widget components); `arb --html` static export.
6. ❌ Actors — out of scope: dataflow / actors / pub-sub belong to stryke; arb stays in the UI-generation lane (no duplication).
7. ✅ Package manager — local preset library (`--save`/`--install`/`--uninstall`/`--installed`) + a networked registry over a git index hosted on GitHub (`arb update`/`search`/`install`/`add`/`uninstall`/`publish`, `~/.arb/pkg` resolver tier, transitive `[deps]` with semver constraint-checking). `arb publish` upserts the package's entry into the index and pushes it (default registry `github.com/MenkeTechnologies/arb-registry`). *(native/cdylib packages + multi-version semver resolution: ⬜)*
8. ✅ LSP/DAP — `arb --lsp` ships a full server: diagnostics (real source ranges, UTF-16 columns), `documentSymbol`, `hover`, `completion` (CORPUS verbs + dot-context `.path` names + widget `-flags`), `signatureHelp`, `definition`/`references`/`documentHighlight`/`rename` over widget `.path` names, `foldingRange`, `formatting`, and `semanticTokens/full`. `arb --dap` is a real steppable debugger over the stream model: each incoming line is a step, breakpoints are regex predicates (a `SourceBreakpoint.condition`, or unconditional = single-step), the stack trace is the query-pipeline stages, scopes expose the matched line + stream stats + control values, and `evaluate` runs arb's real expression evaluator against the paused line. The `program` (spec) and `input` (data file) come from the `launch` request since DAP owns stdio; `stepIn`/`stepOut` collapse to `next` (a stream has no call nesting). Diagnostics anchor to the offending verb even when nested inside a `source`/`out` body (not the enclosing directive). *(per-token argument precision — squiggle the `/re/` itself, not its verb — ⬜)*
