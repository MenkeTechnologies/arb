```
 █████╗ ██████╗ ██████╗
██╔══██╗██╔══██╗██╔══██╗
███████║██████╔╝██████╔╝
██╔══██║██╔══██╗██╔══██╗
██║  ██║██║  ██║██████╔╝
╚═╝  ╚═╝╚═╝  ╚═╝╚═════╝
```

![Rust](https://img.shields.io/badge/Rust-2021-05d9e8?style=flat-square)
[![Docs](https://img.shields.io/badge/docs-online-blue.svg)](https://menketechnologies.github.io/arb/)
![license](https://img.shields.io/badge/license-MIT-ff2a6d?style=flat-square)
![status](https://img.shields.io/badge/status-active%20%C2%B7%20in%20development-9b5de5?style=flat-square)

### `[A TUI FOR EVERY PIPELINE]`

> *"A pipeline dumps text at you; arb turns it into an interface."*

**arb** — visualize and modify Unix pipelines. Pipe a stream in and arb spawns a
dynamic TUI or a served web page, built from a declarative,
Tcl/Tk-flavored spec. It is a `jq`/`xpath`/`css`/`yq` superset, an interactive
megafilter/map over the live passthrough, and an original language on the
[`fusevm`](https://github.com/MenkeTechnologies/fusevm) bytecode VM + three-tier
Cranelift JIT — the same engine behind `zshrs`, `stryke`, `rubylang`, and `elisp`.

### [`Read the Docs`](https://menketechnologies.github.io/arb/) &middot; [`Engineering Report`](https://menketechnologies.github.io/arb/report.html) &middot; [`Language Spec`](SPEC.md)

---

## Table of Contents

- [\[0x00\] Overview](#0x00-overview)
- [\[0x01\] Install](#0x01-install)
- [\[0x02\] Usage](#0x02-usage)
- [\[0x03\] Design](#0x03-design)
- [\[0x04\] Query Engine](#0x04-query-engine)
- [\[0x05\] Command Line](#0x05-command-line)
- [\[0x06\] Architecture](#0x06-architecture)
- [\[0x07\] Status & Roadmap](#0x07-status--roadmap)
- [\[0x08\] Documentation](#0x08-documentation)
- [\[0xFF\] License](#0xff-license)

---

## [0x00] OVERVIEW

A pipeline dumps text at you; arb turns it into an interface. Drop it into a pipe
and a small declarative spec describes widgets, layout, a uniform query over any
format, and interactive controls that feed back into the passthrough — so arb
sits mid-pipe and *shapes* what the downstream consumer receives, not just
displays it. Highlights:

- **Pipe-native** — terminal-invoked, pipe-driven. No daemon; the web target
  spawns a local UI host on demand (like `textual serve`), not a server you run.
- **Dual target** — the same spec renders to a ratatui TUI or a served web page
  + WebSocket (`arb --serve`), the browser dashboard built from the shared
  [`zgui-core`](https://github.com/MenkeTechnologies/zgui-core) component toolkit.
- **One query engine** — a `jq`/`xpath`/`css`/`yq` superset over JSON, XML, HTML,
  YAML, TOML, and CSV.
- **Megafilter/map** — interactive controls render *and* feed `out`, so a
  control's path used as a value is its current state — arb filters and maps the
  downstream output live.
- **fzf superset + orchestrator** — `arb --fzf` is a fuzzy select mode (rank,
  smart-case, multi-select, preview); `arb 'PROD | _ | CONS'` runs a whole
  pipeline with arb as the `_` stage, hooking each command's fds so producer
  **stderr lands in a pane** instead of corrupting the TUI.
- **Runs on fusevm** — the compute core (expressions and the `calc` pipeline op)
  lowers to a `fusevm::Chunk` and executes on the fusevm VM + three-tier Cranelift
  JIT. Declarative widget/layout construction needs no VM; more of the language
  moves onto fusevm as the expression layer grows.
- **Original, not a port** — an original language in stryke's class, deliberately
  lean (rubylang-scale, not stryke-scale). It reuses mechanics from its siblings
  (fusevm embedding, the rkyv script cache, the LSP/DAP stdio shape, the
  package-manager ABI) but the lexer, parser, AST, lowering, and semantics are
  arb's own.
- **World-first = synthesis + ecosystem** — no single leg is new (Tcl/Tk, Expect,
  dasel, ratatui, `textual serve` are all prior art); the combination is: a
  pipe-native, dual-target, component-generating UI language with a shareable
  dashboard registry. No registry of installable pipeline TUIs exists today.

---

## [0x01] INSTALL

```sh
# Via Homebrew tap (bumped by each release; formula is `arb`)
brew tap MenkeTechnologies/menketech
brew install arb

# Or from source (--recurse-submodules pulls zgui-core for the web dashboard;
# without it the TUI works fully and the web target shows a one-line notice)
git clone --recurse-submodules https://github.com/MenkeTechnologies/arb
cd arb
cargo build
find / | ./target/debug/arb

# Or via crates.io (the crate is `arblang` — the name `arb` was taken;
# it still installs the `arb` binary)
cargo install arblang
```

arb builds as a standalone Rust crate — a lib + bin, so the language front-end is
unit-testable without a terminal. Run the tests with `cargo test`.

---

## [0x02] USAGE

```sh
# zero-config: a live tail of stdin with a count + rate header; q / Esc / Ctrl-C quits
find / | arb

# a spec: a gauge fed by the live line count of the stream
seq 1 100000 | arb -e 'gauge .g -max 100000; source .g { in; count }'

# a filtered list: keep 5xx lines, drop health checks
tail -f access.log | arb -e 'list .l; source .l { in; match /5\d\d/; reject /health/ }'

# pipe citizen — a viz tap/peek: arb sits mid-pipe, and with no `out { }` it
# passes the stream through untouched, so the downstream consumer still gets it
find / | arb dash.arb | stryke

# or a filter/map: an `out { }` block reshapes what flows downstream (streams
# live for per-line pipelines; `head`/consumer exit is a clean stop, not an error)
tail -f access.log | arb -e 'out { in; match /5\d\d/; field 7 }' | stryke
```

Mid-pipe, arb is either a **tap/peek** (no `out` — the stream passes through
unchanged while the spec visualizes it) or a **filter/map** (an `out { … }` block
reshapes the passthrough). The TUI renders to `/dev/tty` (like fzf), so stdout
stays a clean data channel; keys are read straight from `/dev/tty` (like vipe),
so `find / | arb` works even though stdin carries the pipe.

### Interactive filter (megafilter)

In the TUI, **type to filter** the whole dashboard live (case-insensitive
substring); `Bksp`/`Ctrl-U` edit, `Esc` clears, `Ctrl-C` quits. When piped
onward, the filter also narrows what the downstream consumer receives — the
megafilter reshapes the pipe as you type.

### Interactive map (megafilter/map)

The filter narrows; an **`out { … apply .x }`** pipeline fed by an `input` widget
**maps** — you edit a transform in the TUI and the downstream pipe updates live,
so arb is a scriptable, interactive stage in the middle of a pipeline:

```sh
tail -f access.log | arb -e 'input .x -placeholder "transform, e.g. field 7"
                             tail .t
                             source .t { in }
                             out { in; apply .x }' | downstream
#  type `field 7` → downstream receives column 7 of every line, live
#  type `grep /404/` → downstream receives only 404s; clear it → full stream again
```

The `out` map runs per line as the stream flows (never buffered — arb doesn't
block the pipe like `vipe`), re-resolving only when you edit the field. An empty
field is identity (the pipe passes through untouched until you type). A filtering
transform (`grep`/`reject`) drops lines downstream; a reducer (`count`) can't map
a single line and falls back to passthrough. The TUI stays up on `/dev/tty` while
stdout carries the mapped data.

### Key bindings — `bind C-<key> <action>`

Drive the spec's own state from the keyboard. A `bind` maps a **control key** (so
it never shadows filter typing) to an action: `set .name VALUE` writes an `input`
value — with an `out { … apply .name }` map that reshapes the live pipe on a
keystroke — or `quit`:

```sh
tail -f access.log | arb -e 'input .x
                             tail .t
                             source .t { in }
                             out { in; apply .x }
                             bind C-u set .x upper        # Ctrl-U: uppercase the pipe
                             bind C-e set .x "grep /ERROR/"  # Ctrl-E: only errors
                             bind C-r set .x ""           # Ctrl-R: reset to passthrough
                             bind C-q quit' | downstream
```

Keys are `C-<letter>` (also `c-<letter>` or `^<letter>`). `set` binds turn the
interactive map into a set of one-key presets; `quit` exits.

### Reactions — `expect /regex/ <action>`

The "react" half of Expect: when a **stream line matches** a pattern, fire an
action automatically — no keypress. Same action vocabulary as `bind` (`set` a
control, `quit`), so the stream can drive itself:

```sh
tail -f deploy.log | arb -e 'input .x
                             tail .t
                             source .t { in }
                             out { in; apply .x }
                             expect /ERROR/    set .x "grep /ERROR/"  # errors appear → pipe narrows to them
                             expect /deploy ok/ quit'                 # success line → exit
```

Patterns are checked against new lines as they arrive (on the redraw cadence; a
line that scrolls past faster than a frame on a bounded dashboard may be missed).
Combined with `bind`, a spec reacts to both the keyboard and the stream — the
basis for spawn/expect/react automation.

### fzf mode — `arb --fzf`

A fuzzy select mode: filter a stream and pick line(s), printed to stdout on
Enter. A superset of fzf's core (fuzzy match + ranking + smart-case, multi-select,
a preview pane), not a re-skin.

```sh
vim "$(git ls-files | arb --fzf)"          # single select
ls *.log | arb --fzf                        # type to fuzzy-filter, Enter picks
```

- **Fuzzy match** — pattern chars match in order (subsequence); results ranked
  best-first (contiguous runs + word-boundary starts win). **Smart-case**:
  lowercase query is case-insensitive, any uppercase makes it case-sensitive.
- **Navigate** — `↑`/`↓`, `Ctrl-J`/`Ctrl-N` down, `Ctrl-K`/`Ctrl-P` up.
- **Multi-select** — `Tab` marks lines (green `+`); Enter emits all marked.
- Matched chars highlight yellow; keeps the entire stream (no line drop), so
  marks persist and a huge `find /` stays fully selectable.

**Drop-in for `fzf`.** `arb --fzf` tolerates the `fzf` binary's flags, so you can
repoint a wrapper at it (e.g. `ZPWR_FZF='arb --fzf'`) without rewriting call
sites. Honored: `-e`/`--exact` (substring, not fuzzy), `--no-sort` (keep input
order), `--query`, `-m`/`--multi`, `--prompt`, `--header`, `--height`,
`--preview 'CMD {}'`. fzf-only flags with no arb analog (`--ansi`, `--border`,
`--reverse`, `--preview-window`, `--min-height`, `--tiebreak`, `--layout`,
`--bind`, `--nth`, `+m`/`+s`, …) are accepted and quietly ignored so the command
still runs.

**`--fzf` is a DSL spec, not a hardcoded mode.** It synthesizes a one-widget
`select` spec — so the select surface is expressible directly, and `-prompt`/
`-header` become widget opts:

```sh
git ls-files | arb --fzf                          # sugar for the spec below
git ls-files | arb -e 'select .files -prompt "pick> " -header files
                       source .files { in }'       # identical: fzf as a spec
```

A `select` widget anywhere in a spec puts the TUI in select mode, so fzf is just
one shape the DSL can build — the same DSL that builds dashboards and the
`input`/`apply` transform editor above.

**Projected candidates (`--with-nth`/`--nth`).** The select widget's `source`
pipeline transforms what's *shown and searched*, while Enter still emits the
*original* line — so you pick from a clean view and get the raw record:

```sh
ps aux | arb -e 'select .p { in; field 11 }'   # search/show the command column,
                                                # Enter emits the whole ps row
git log --oneline | arb -e 'select .c { in; grep /fix/ }'  # candidates pre-filtered
```

The projection is per-line: `field`, `upper`, `grep`, `extract`, … map a line to
its display row(s); a filtering verb drops non-matches from the list. Cross-line
verbs (`sort`, `count`) can't project and fall back to identity.

**Search a key, show the whole line (`--nth`).** A `search .name { … }` binding
derives the fuzzy-match key per line while the row still shows and emits the full
`source` display — so you keep every column in view but type against just one:

```sh
ps aux | arb -e 'select .p
                 source .p { in }              # show the whole ps row
                 search .p { in; field 11 }'   # but fuzzy-match only the command
```

`search` is pipeline-general (match a lowercased key, an extracted field, a regex
capture), not just a column index. Omit it and the search key is the display.

### Pipeline orchestrator — `arb '<PROD> | _ | <CONS>'`

arb runs a whole pipeline with `_` marking its own interactive stage, so it owns
every stage's file descriptors. The producer's **stderr goes to a pane** instead
of corrupting the TUI (the reason plain `find / | fzf` gets scribbled over by
permission errors):

```sh
arb --fzf 'sudo find / | _ | perl -pe "s|Application|APP|"'
#          └ producer ┘   │   └──────── consumer ────────┘
#          stdout→list    │   selection piped through it on Enter
#          stderr→⚠ pane  arb's interactive stage
```

Each stage is shelled out (`sh -c`, so globs/quotes work); arb wires the
fds between them. (`--run 'PIPELINE'` is the explicit-flag form.)

### Interactive editor — `input` widget + `apply` verb

fzf is one TUI. The DSL builds arbitrary ones. An `input .name` widget is a live
editable field; the `apply .name` verb splices its current value into a source
pipeline, re-evaluated every frame. That makes a **before/after transform editor**
a spec, not a mode:

```sh
printf 'alice\nbob\ncarol\n' | arb -e '
  input .q -placeholder "transform (e.g. upper)"
  list  .before
  list  .after
  source .before { in }
  source .after  { in; apply .q }'
#  type `upper` in the field → the .after pane recomputes `in; upper` live
```

`Tab` cycles focus between fields, typing edits the focused one, `Esc`/`Ctrl-U`
clear it. Any query verb (`upper`, `field N`, `grep /re/`, `commafy`, …) is valid
after `apply`, so the field drives the whole downstream pipeline interactively.

### Web dashboard — `arb --serve`

The **same spec** that drives the ratatui TUI drives a browser. `--serve` starts a
local HTTP server (std-only, no framework), serves one self-contained page, and
the page polls the live stream — so a pipeline becomes a shareable dashboard:

```sh
tail -f metrics.log | arb --serve --port 8787 -e 'gauge .rps -max 1000
                                                  source .rps { in; rate }
                                                  histo .codes
                                                  source .codes { in; field 9; tally }'
#  → arb: serving dashboard at http://127.0.0.1:8787/
```

The page is built with [`zgui-core`](https://github.com/MenkeTechnologies/zgui-core)
— the shared cyberpunk web-component toolkit, vendored as a git submodule at
`lib/zgui-core` and bundled into the binary at build time (`build.rs` →
`include_str!`, so the binary stays self-contained). It mounts `ZGui.appShell`
(splash, filter bar, ⌘K palette, settings/colorscheme) and renders each widget
with the matching component — `gauge`→`ZGui.gauge`, `chart`→`ZGui.chart`,
`spark`→`ZGui.sparkline`, `bars`/`histo`→`ZGui.statBars`, `table`→`ZGui.dataTable`,
containers/log→`ZGui.card`+`ZGui.logView`. Every widget's `source` is evaluated
server-side and pushed as structured JSON; the client feeds it to the component
handles (`.set`/`.setSeries`/`.setRows`) — never `innerHTML` with stream data, so
nothing can inject markup. the `input`/`filter` fields, `slider` (range), `check` (checkbox), and `facet`
(multi-select, with `-field` candidates computed server-side) controls render as
real form elements that `POST /set` on change, so the server re-resolves the
bound pipelines live — the browser drives the megafilter/map just like the
terminal. `--port 0` picks a free port and prints it.

> The web target needs the submodule checked out: `git submodule update --init`.
> Without it the binary still builds (the dashboard shows a one-line notice).

Updates arrive over a **WebSocket** (`/ws`) — the server pushes a frame every
250 ms, no polling lag. The handshake and framing are hand-rolled over the same
std TCP socket (SHA-1 + base64, no crypto or WebSocket dependency); if the
browser or connection can't upgrade, the client automatically falls back to
polling `/data`.

### Presets & sharing — `--save` / `--install`

A spec is a portable file, so dashboards are shareable units. Save your own,
install ones others send you, and run any of them by name from anywhere:

```sh
arb --save api -e 'gauge .g -max 1000; source .g { in; rate }'  # save your own
arb --install team-dash.arb                                     # install a shared spec
arb --install team-dash.arb --as prod                           # …under a chosen name
arb --installed                                                  # list installed presets
find / | arb -p api                                             # run one by name
arb --uninstall api                                             # remove it
```

Installed specs live in `~/.arb/lib` (override with `$ARB_LIB`); the first `#`
comment line is the description shown by `--installed`/`--list`. Install
validates the spec before adding it, so the library only holds runnable
dashboards. A shared spec is any `.arb` file today; a remote registry (install by
URL/name) plugs into the same resolver next.

---

## [0x03] DESIGN

| Piece | How |
| --- | --- |
| **Pipe-native** | Terminal-invoked, pipe-driven. No daemon; the web target spawns a local UI host on demand (like `textual serve`), not a server you run. |
| **Tcl/Tk-flavored, not Tcl** | Commands take args and verbatim `{ }` blocks; widget paths are dot-hierarchical (`.a.b.c`). No `$`, `[cmd]`, or `expr{}` substitution. |
| **One query engine** | A single vocabulary (a `jq`/`xpath`/`css`/`yq` superset) works uniformly over JSON, XML, HTML, YAML, TOML, and CSV. |
| **Megafilter/map** | Interactive controls render *and* feed `out`, so a control's path used as a value is its current state — arb filters and maps the downstream output live. |
| **Runs on fusevm** | The computational core — expressions and the `calc` pipeline op — lowers to a `fusevm::Chunk` and executes on the fusevm VM (three-tier Cranelift JIT). Declarative widget/layout construction needs no VM; more of the language moves onto fusevm as the expression layer grows. |

The full grammar — values, variables, functions, widgets, layout, controls,
Expect reactions, actors, modules, and the package manager — is in
[`SPEC.md`](SPEC.md).

---

## [0x04] QUERY ENGINE

A single query vocabulary works uniformly over every format — a `jq`/`xpath`/
`css`/`yq` superset.

| jq / xpath / css | arb |
| --- | --- |
| `.users[].name` | `field users; each; field name` |
| `.items[] \| select(.price>10)` | `field items; each; where(price>10)` |
| `{name, age}` (projection) | `pick name age` |
| `//a/@href` | `find a; attr href` |
| `div.card h2` | `sel {div.card h2}` |

The vocabulary works uniformly over line, JSON (`in.json`, nested key paths),
CSV/TSV (`in.csv`/`in.tsv`), YAML (`in.yaml`, single- or `---`-multi-doc), TOML
(`in.toml`), and HTML streams — one query engine over every format (the yq leg):
`in.yaml`/`in.toml` parse the document to JSON so every JSON verb applies. In
families:

- **Filter** — `match`/`grep`, `reject`/`grepv`, `contains`, `starts`, `ends`,
  `nonempty`, `numeric`, `over N`, `under N`, `between A B`, `has KEY`.
- **Extract / shape** — `field`, `fields N M…` (project/reorder whitespace
  columns — `fields 1 3` for columnar `ps`/`ls -l`/`df`), `pick K…` (jq
  projection), `cut`, `find TAG` + `attr NAME` + `text` (xpath/css: `//a/@href`),
  `sel {CSS}`, `keys`, `vals`, `entries`, `flatten`, `each`, `extract /re/`,
  `split D`, `substr A B`, `chars`.
- **Record edit** (jq assignment) — `set K V`, `del K`, `rename OLD NEW`,
  `default K V`, `merge`.
- **Transform** — `map EXPR`, `upper`/`lower`/`trim`/`title`, `replace`,
  `prepend`/`append`, `pad`/`lpad`, `repeat N`, `flip`, `words`, `enumerate`,
  `join`, `floor`/`ceil`, `clamp LO HI`, `delta` (consecutive differences — a
  counter's rate-of-change) / `cumsum` (running total), `sma N` (moving average)
  / `ewma A` (exponential smoothing — for noisy series feeding `spark`/`chart`),
  `commafy`, `bytes` (`1536` → `1.5 KB`), `duration` (`3661` → `1h 1m`).
- **Encode** — `b64`/`b64d`, `hex`/`unhex`, `urlenc`/`urldec`.
- **Order / dedup** — `sort`, `sort_by F`, `uniq`, `unique_by F`, `dedup`, `rev`,
  `first`/`last`/`take`/`drop`/`tailn`/`nth`/`slice`, `sample`.
- **Aggregate / reduce** — `count`, `rate`, `tally`, `count_by F`, `sum`, `min`/
  `max`, `min_by F`/`max_by F`, `avg`, `median`, `stddev`, `percentile N`
  (nearest-rank; `p50`/`p90`/`p95`/`p99` sugar — for latency tails), `product`,
  `add`, `range`, `bins`.

The expression layer — `where PRED` (filter), `map EXPR` (per-line transform),
`calc EXPR` (reduce) — lowers to a `fusevm::Chunk` and runs on the VM, with
field-aware references, compound predicates via `and`/`or`/`not`, and set/range
membership `in [a, b, c]` / `in lo..hi` (`where ms > 1000 and status in [500,
502, 503]`, `where code in 500..599`, `map bytes / 1024`, `where not healthy`, `map x != 0 ? 100 / x : 0` ternary).
Results render into `text`/`tail`/`list`/`gauge`/`bars`/`histo`/`spark`/`chart`/
`table` widgets (`table` splits whitespace columns with optional `-cols "a,b,c"`
headers; `spark` draws a unicode sparkline and `chart` a line plot of a numeric
series), arranged by `grid` — `grid .w -row R -col C` places a widget, and
`-span N` (or `-rowspan`/`-colspan`) lets one span several cells, so a main
`chart` can be wide while small gauges take a single cell. Any widget takes
`-label "…"` to set a human header (instead of the dot-path) and `-color NAME`
(`green`/`red`/`yellow`/`orange`/`magenta`/`blue`/`white`/`gray`, default `cyan`)
to tint its border and accent — both apply in the TUI and the web dashboard, so
panels read cleanly and can be status-coded (green ok, red errors). `list`/`tail`
take `-limit N` (alias `-lines N`) to cap the rows shown to the last N.

---

## [0x05] COMMAND LINE

| Invocation | Effect |
| --- | --- |
| `cmd \| arb` | Zero-config: a full-screen live tail of stdin (type to filter). |
| `cmd \| arb FILE.arb` | Run a dashboard spec file. |
| `cmd \| arb -e SRC` | Run an inline spec. |
| `cmd \| arb --fzf` | fzf select mode: fuzzy-filter + pick line(s) to stdout. |
| `cmd \| arb -- CMD…` | Preview pane: re-run CMD over the filtered output. |
| `arb '<PROD> \| _ \| <CONS>'` | Orchestrate a pipeline; `_` is arb's stage, producer stderr → pane. |
| `arb --run 'PIPELINE'` | Same, explicit flag form. |
| `arb --lsp` | Language Server over stdio for `.arb` (diagnostics, completion, hover, signatureHelp, definition/references/highlight/rename, folding, formatting, semanticTokens). |
| `arb --dap` | Debug Adapter over stdio: step the stream line-by-line, regex breakpoints, inspect the paused line / stats / controls. |
| `--version` / `--help` | Version / usage. |

---

## [0x06] ARCHITECTURE

```
stdin  →  lexer  →  parser (AST)  →  Spec interp  →  ratatui TUI  (or served web + WS)
                                          │
                              source query pipeline over the live stream
                              (calc / expressions lower to fusevm bytecode)
```

Transfers from siblings are **mechanics only** — fusevm embedding, the rkyv
script cache, the LSP/DAP stdio shape, the package-manager ABI. The language design
(lexer / parser / AST / compiler / semantics) is arb-original. The compute core
already lowers to a `fusevm::Chunk` and runs on the VM; declarative widget and
layout construction is plain Rust construction and needs no VM.

---

## [0x07] STATUS & ROADMAP

**Shipped** — the daily-driver path (pipe in → parse spec → query → render, in
the terminal or the browser) is complete:

- **Language** — the Tcl-flavored reader, the declarative widget / `source` /
  `out` interpreter, `.x <- in` binds, `fn`/lambda expressions, and `calc` /
  `where` predicates that lower to `fusevm` bytecode and run on the VM.
- **Widgets** — `text`, `tail`, `list`, `gauge`, `bars`, `histo`, `spark`,
  `chart`, `table`, `tabs`, `block`, `frame` render in the TUI; `input`/`filter`
  fields, a `slider`, a `check` toggle, a `facet` multi-select, and `select` (an
  fzf-style fuzzy picker) are interactive controls. Auto-layout by default,
  `grid` (with `-span`/`-rowspan`/`-colspan`) to override, per-widget `-color`.
- **Query superset** — the full `jq`/`xpath`/`css`/`yq` verb set in
  [SPEC §8](SPEC.md) over JSON, XML, HTML, YAML, TOML, and CSV.
- **Megafilter/map** — `out { … }` shapes the downstream passthrough, driven by
  `input`/`filter`/`facet`/`slider`/`check` controls via `apply` and control-path
  predicates: numeric `where lat < .th`, string `where match(.q)`, set
  `where level in .lv`.
- **Web target** — `arb --serve` hosts the same spec as a live browser dashboard
  built from the `zgui-core` component toolkit (`ZGui.appShell` + per-widget
  components), pushed over a hand-rolled WebSocket (RFC 6455) with a `/data`
  polling fallback; `arb --html` emits a static snapshot.
- **Reactions & events** — `expect /re/ ACTION` / `bind C-<key> ACTION` with
  actions `set`/`quit`/`beep`/`alert`/`flash`/`exec` and `{ … }` block form; Tk
  named keys (`<Enter>`/`<Esc>`/`<Tab>`/`<Key-x>`); `timeout Ns ACTION` idle
  reactions; `.w configure -k v` retune.
- **Mouse** (SGR, in the TUI) — **left-click** to focus/toggle a control, drag a
  `slider`, click a `tabs` label or an fzf row (**double-click** to pick it);
  **right-click** resets a control to its default; **middle-click** focuses only.
  The **wheel** scrolls back through a `tail`/`list`/`table`/`text`/`block`/`frame`
  and returns to the live tail. Shift/Alt/Ctrl modifier bits are decoded too;
  `bind <Click>` / `bind <Resize>` reactions fire on press/resize. Hold **Shift**
  and drag for native text selection.
- **Editor tooling** — `arb --lsp`, a full Language Server (diagnostics with
  UTF-16 columns, `completion`, `hover`, `signatureHelp`, `definition`/
  `references`/`documentHighlight`/`rename` over widget `.path` names,
  `foldingRange`, `formatting`, `semanticTokens`), and `arb --dap`, a real
  steppable debugger (each stream line is a step, regex breakpoints, the pipeline
  as the stack, `evaluate` over the paused line) — both over stdio JSON-RPC.
- **Presets & library** — 150+ bundled stdlib dashboards, `import` resolution
  (with `import X as Y` namespacing), a local preset library
  (`--save`/`--install`/`--uninstall`/`--installed`), and a registry client over
  a git index (`arb update`/`search`/`install`/`add`/`uninstall`, resolved from
  `~/.arb/pkg`; `arb publish` validates + prints PR steps).
- **fzf mode** — `arb --fzf` (rank, smart-case, multi-select, preview) and
  pipeline orchestration (`arb 'PROD | _ | CONS'`).
- **Self-sourcing specs** — `spawn CMD` (or `spawn { … }`) makes a spec launch
  its own producer: arb runs CMD via `sh -c` and feeds its stdout into the
  stream in place of stdin, so a dashboard preset needs nothing piped in
  (`arb top.arb` with `spawn top -b`); `< FILE` reads a file as the stream
  instead. One stream source per spec; a CLI `--run` producer wins if both given.
- **Zero-config sniffing** — `cmd | arb` (no spec) peeks the stream and
  auto-picks a preset by data shape (JSON→`json`/`logs`, `docker`/`top`/`k8s`
  headers, git-log, CSV→`table`); a non-blocking `poll` peek never hangs, and
  the peeked lines are replayed so nothing is lost.
- **rkyv script cache** — a re-run of the same spec skips lex+parse: the parsed
  AST is cached at `~/.arb/scripts.rkyv` (or `$ARB_CACHE`) as a zero-copy rkyv
  shard keyed by an FxHash of the source + a schema version, so a source or
  format change misses cleanly and a corrupt shard resets on its own. Same
  architecture every sibling lang ships.

**Planned** (specified in [`SPEC.md`](SPEC.md), not yet built) — the hosted
registry index behind `arb publish` (the client validates + prints PR steps
today) with native/cdylib packages and multi-version semver resolution, and the
upstream-**command** sniffing leg (producer argv) — zero-config **data-shape**
sniffing already ships. Akka-style actors are **out of scope** — dataflow /
pub-sub belong to `stryke`.

Nothing is faked: unrecognized widget verbs are ignored so specs stay
forward-compatible, and unbuilt features are absent, not stubbed.

---

## [0x08] DOCUMENTATION

- **[Read the Docs](https://menketechnologies.github.io/arb/)** — the HUD
  documentation site.
- **[Engineering Report](https://menketechnologies.github.io/arb/report.html)**
  — architecture, world-first positioning, milestones, dependency posture.
- **[`SPEC.md`](SPEC.md)** — the full language spec: grammar, widgets, query,
  controls, actors, packages.

---

## [0xFF] LICENSE

MIT — free and open source. See [`LICENSE`](LICENSE).
