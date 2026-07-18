# arb — SPEC

**arb** is a standalone, original language on **fusevm/JIT** for **visualizing and modifying Unix pipelines**: drop it in a pipe and it spawns a **dynamic TUI (ratatui) or web page (zgui)** built from a declarative spec. It is a **jq/xpath/css/yq superset**, an interactive **megafilter/map** over the live passthrough, has **Akka-style actor concurrency**, its own **Tcl/Tk-flavored DSL**, **LSP/DAP + rkyv** reused from sibling frontends, and a **package manager** so users share dashboards — *a TUI for every pipeline*.

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
spawn { ps aux }         arb launches a process (Expect)
! vmstat 1 every 1s      repeated command source
< file.log               file
out { … }                downstream emission (to | next), shaped by controls
send "text"              send input to spawned process (Expect)
```

## 8. Query — jq/xpath/css/yq superset (uniform over all formats)

```
field NAME        key (jq .name); field a b c = a.b.c
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

## 9. Widgets ("Tk" register)

```
text .t -label L          tail .t -label L        table .t -cols {a b c}
list .t                   gauge .t -label L -max N spark .t -label L -window N
bars .t -label L          histo .t                chart .t -kind line
tabs .t -tabs {a b}       block .t -title T -border  frame .f
.t configure -max 200     # live reconfigure
```

## 10. Layout (auto by default)

```
# no pack/grid → widgets auto-tile (grid flow). Only add geometry to override.
pack .top -side top|bottom|left|right -size 40%
grid .a -row 0 -col 0 -span 2
rows { .a; .b }   cols { .a; .b }
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

```
expect {
    /5\d\d/      { alert("5xx"); flash .log red }
    /panic|OOM/  { beep; exec("notify-send arb") }
    timeout 5s   { alert("idle") }
}
# actions: alert(s) flash .w color beep exec(cmd) spawn{…} + any statement
```

## 14. Events — bind (Tk)

```
bind <Key-q>       { quit }
bind .ps <Enter>   { spawn("kubectl describe " + .ps.sel) }
bind .ps <Click>   { … }     bind <Resize> { … }
```

## 15. Actors — Akka-style concurrency

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
target web -port 8080      # served zgui page + WS
theme cyberpunk            # shared HUD scheme (matches sibling apps)
set refresh 250            # ms redraw throttle
```

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

## 19. Ecosystem — "a TUI for every pipeline"

Community publishes `arb-<tool>` packages. `cmd | arb` sniffs the upstream command (or data shape) → resolves the matching package → renders. Every common pipeline (docker/kubectl/psql/nginx/git/systemctl/…) gets a shared, installable dashboard. No registry of shareable pipeline TUIs exists today — this is the world-first ecosystem leg.

## 20. Architecture (fusevm frontend, original — mechanics ported, semantics fresh)

Deps (rubyrs-lean): `fusevm{jit,jit-disk-cache,aot}`, `rkyv`/`bincode`/`memmap2`, `clap`, `ratatui`+`crossterm`; web (feature-gated): async http+ws; parsers: serde_json/quick-xml/serde_yaml/toml/csv + html.

```
src/lexer.rs     Tcl-flavored reader           (original)
src/parser.rs    widget cmds + expr grammar     (original)
src/ast.rs
src/compiler.rs  AST → fusevm::Chunk            (original lowering)
src/host.rs      extension ops: ratatui/zgui/query/actors (Value::Obj heap)
src/query.rs     jq/xpath/css/yq engine
src/tui.rs       ratatui backend
src/web.rs       zgui codegen + WS server (feature: web)
src/actor.rs     Akka-style runtime (feature: actors)
src/module.rs    import/resolution
src/pkg.rs       package manager (arb.toml, install/publish)
src/cache.rs     rkyv bytecode cache ~/.arb/    (pattern ported from rubyrs)
src/lsp.rs src/dap.rs   stdio JSON-RPC          (shape ported)
src/cli.rs src/main.rs src/repl.rs src/banner.rs
```

Transfers from siblings = **mechanics only** (fusevm embedding, rkyv cache, lsp/dap stdio, pkg ABI). Language design (lexer/parser/ast/compiler/semantics) is arb-original.

## 21. Milestones

0. **Walking skeleton** — `echo hi | arb -e 'text .t <- in'`: lex→parse→lower→fusevm→one ratatui widget from stdin.
1. Core widgets + auto-layout + `source`/query basics.
2. Presets/imports/modules + stdlib (logs/http/json/table/top/metrics).
3. Interactive controls + `out` passthrough shaping (megafilter/map).
4. Expect reactions + events/bind.
5. Web target (zgui codegen + WS).
6. Actors.
7. Package manager + registry + ecosystem.
8. LSP/DAP.
