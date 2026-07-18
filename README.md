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
dynamic TUI (and, later, a served web page) built from a declarative,
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
- **Dual target** — the same spec renders to a ratatui TUI or, later, a served
  `zgui` web page + WebSocket, sharing the cyberpunk HUD scheme with its siblings.
- **One query engine** — a `jq`/`xpath`/`css`/`yq` superset over JSON, XML, HTML,
  YAML, TOML, and CSV.
- **Megafilter/map** — interactive controls render *and* feed `out`, so a
  control's path used as a value is its current state — arb filters and maps the
  downstream output live.
- **Runs on fusevm** — the compute core (expressions and the `calc` pipeline op)
  lowers to a `fusevm::Chunk` and executes on the fusevm VM + three-tier Cranelift
  JIT. Declarative widget/layout construction needs no VM; more of the language
  moves onto fusevm as the expression layer grows.
- **Original, not a port** — an original language in stryke's class, deliberately
  lean (rubylang-scale, not stryke-scale). It reuses mechanics from its siblings
  (fusevm embedding, the rkyv cache, the LSP/DAP stdio shape, the package-manager
  ABI) but the lexer, parser, AST, lowering, and semantics are arb's own.
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

# Or from source
git clone https://github.com/MenkeTechnologies/arb
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
reshapes the passthrough). With no controlling terminal on stdout it forwards the
stream (tap) or emits the `out` result (filter); with no stdin-reading spec it
prints the parsed spec summary.

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
- **Extract / shape** — `field`, `pick K…` (jq projection), `cut`, `find TAG` +
  `attr NAME` + `text` (xpath/css: `//a/@href`), `sel {CSS}`, `keys`, `vals`,
  `entries`, `flatten`, `each`, `extract /re/`, `split D`, `substr A B`, `chars`.
- **Record edit** (jq assignment) — `set K V`, `del K`, `rename OLD NEW`,
  `default K V`, `merge`.
- **Transform** — `map EXPR`, `upper`/`lower`/`trim`/`title`, `replace`,
  `prepend`/`append`, `pad`/`lpad`, `repeat N`, `flip`, `words`, `enumerate`,
  `join`, `floor`/`ceil`, `clamp LO HI`.
- **Encode** — `b64`/`b64d`, `hex`/`unhex`, `urlenc`/`urldec`.
- **Order / dedup** — `sort`, `sort_by F`, `uniq`, `unique_by F`, `dedup`, `rev`,
  `first`/`last`/`take`/`drop`/`tailn`/`nth`/`slice`, `sample`.
- **Aggregate / reduce** — `count`, `rate`, `tally`, `count_by F`, `sum`, `min`/
  `max`, `min_by F`/`max_by F`, `avg`, `median`, `stddev`, `p95`, `product`,
  `add`, `range`, `bins`.

The expression layer — `where PRED` (filter), `map EXPR` (per-line transform),
`calc EXPR` (reduce) — lowers to a `fusevm::Chunk` and runs on the VM, with
field-aware references and compound predicates via `and`/`or`/`not`
(`where ms > 1000 and status >= 500`, `map bytes / 1024`, `where not healthy`).
Results render into `text`/`tail`/`list`/`gauge`/`bars`/`histo` widgets, arranged
by `grid`.

---

## [0x05] COMMAND LINE

| Invocation | Effect |
| --- | --- |
| `cmd \| arb` | Zero-config: a full-screen live tail of stdin. |
| `cmd \| arb FILE.arb` | Run a dashboard spec file. |
| `cmd \| arb -e SRC` | Run an inline spec. |
| `--version` / `--help` | Version / usage. |

---

## [0x06] ARCHITECTURE

```
stdin  →  lexer  →  parser (AST)  →  Spec interp  →  ratatui TUI  (served zgui web + WS later)
                                          │
                              source query pipeline over the live stream
                              (calc / expressions lower to fusevm bytecode)
```

Transfers from siblings are **mechanics only** — fusevm embedding, the rkyv
cache, the LSP/DAP stdio shape, the package-manager ABI. The language design
(lexer / parser / AST / compiler / semantics) is arb-original. The compute core
already lowers to a `fusevm::Chunk` and runs on the VM; declarative widget and
layout construction is plain Rust construction and needs no VM.

---

## [0x07] STATUS & ROADMAP

Early. The committed tree covers:

- **M0** — zero-config live-tail TUI (ratatui) + headless summary; count/rate
  header, ring buffer, `q`/Esc/Ctrl-C quit.
- **M1** — the Tcl-flavored reader, the declarative widget/`source` interpreter
  with `.x <- in` binds, and multi-widget render of `text`/`tail`/`list`.
- **M2** *(expanding)* — the source query pipeline: `in`, `match`/`grep`,
  `reject`/`grepv`, `field N`/`field NAME`, `count`, `rate`, `tally` over line
  and JSON streams (`in.json`, nested key paths), arranged by `grid`, with
  per-widget derived data rendering into `gauge`/`bars`/`histo`; and `calc` —
  arithmetic that compiles to fusevm bytecode and runs on the VM.

The rest of the language — the rest of the expression layer (`fn` / lambdas),
the full query superset, interactive pipe-shaping controls, Expect-style stream
reactions, the web target, actors, and a package manager for sharing dashboards —
is specified in [`SPEC.md`](SPEC.md) and lands across later milestones. Nothing
is faked: unrecognized widget verbs are ignored so specs stay forward-compatible,
and unbuilt features are absent, not stubbed.

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
