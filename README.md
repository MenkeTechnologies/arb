```
            _
  __ _ _ __| |__
 / _` | '__| '_ \
| (_| | |  | |_) |
 \__,_|_|  |_.__/
```

![Rust](https://img.shields.io/badge/Rust-2021-05d9e8?style=flat-square)
![license](https://img.shields.io/badge/license-MIT-ff2a6d?style=flat-square)
![status](https://img.shields.io/badge/status-active%20%C2%B7%20in%20development-9b5de5?style=flat-square)

**arb** — visualize and modify Unix pipelines. Pipe a stream in and arb spawns a
dynamic TUI (and, later, a served web page) built from a declarative,
Tcl/Tk-flavored spec. It is a `jq`/`xpath`/`css`/`yq` superset, an interactive
megafilter/map over the live passthrough, and an original language targeting the
[`fusevm`](https://github.com/MenkeTechnologies/fusevm) bytecode VM + three-tier
Cranelift JIT — the same engine behind `zshrs`, `stryke`, `rubyrs`, and `elisp`.

> *A TUI for every pipeline.*

## What it is

A pipeline dumps text at you; arb turns it into an interface. Drop it into a
pipe and a small declarative spec describes widgets, layout, a uniform query
over any format, and interactive controls that feed back into the passthrough —
so arb sits mid-pipe and *shapes* what the downstream consumer receives, not
just displays it.

```
stdin  →  lexer  →  parser (AST)  →  Spec interp  →  ratatui TUI  (served zgui web + WS later)
                                          │
                              source query pipeline over the live stream
```

It is an **original language** in stryke's class — *not* a port. It reuses
mechanics from its siblings (fusevm embedding, the rkyv cache, the LSP/DAP stdio
shape, the package-manager ABI) but the lexer, parser, AST, lowering, and
semantics are arb's own. Standalone crate, MIT, deliberately lean —
rubyrs-scale, not stryke-scale.

## Try it

```sh
cargo install --path .

# zero-config: a live tail of stdin with a count + rate header; q / Esc / Ctrl-C quits
find / | arb

# a spec: a gauge fed by the live line count of the stream
seq 1 100000 | arb -e 'gauge .g -max 100000; source .g { in; count }'

# a filtered list: keep 5xx lines, drop health checks
tail -f access.log | arb -e 'list .l; source .l { in; match /5\d\d/; reject /health/ }'
```

With no controlling terminal on stdout (piped onward / redirected / CI), arb
prints the parsed spec and each source's evaluated result instead of a TUI.

## Design

| Piece | How |
| --- | --- |
| **Pipe-native** | Terminal-invoked, pipe-driven. No daemon; the web target spawns a local UI host on demand (like `textual serve`), not a server you run. |
| **Tcl/Tk-flavored, not Tcl** | Commands take args and verbatim `{ }` blocks; widget paths are dot-hierarchical (`.a.b.c`). No `$`, `[cmd]`, or `expr{}` substitution. |
| **One query engine** | A single vocabulary (a `jq`/`xpath`/`css`/`yq` superset) is designed to work uniformly over JSON, XML, HTML, YAML, TOML, and CSV. |
| **Megafilter/map** | Interactive controls render *and* feed `out`, so a control's path used as a value is its current state — arb filters and maps the downstream output live. |
| **fusevm-targeted** | Designed to lower the spec to fusevm bytecode and run on the shared Cranelift JIT; the M1/M2a tree is still a direct interpreter, with the lowering a later milestone. |

## Command line

| Invocation | Effect |
| --- | --- |
| `cmd \| arb` | Zero-config: a full-screen live tail of stdin. |
| `cmd \| arb FILE.arb` | Run a dashboard spec file. |
| `cmd \| arb -e SRC` | Run an inline spec. |
| `--version` / `--help` | Version / usage. |

## Status

Early. The committed tree covers:

- **M0** — zero-config live-tail TUI (ratatui) + headless summary; count/rate
  header, ring buffer, `q`/Esc/Ctrl-C quit.
- **M1** — the Tcl-flavored reader, the declarative widget/`source` interpreter
  with `.x <- in` binds, and multi-widget render of `text`/`tail`/`list`.
- **M2a** — the source query pipeline (`in`, `match`/`grep`, `reject`/`grepv`,
  `field N`, `count`, `rate`) with per-widget derived data and the `gauge`
  widget.
- **M2b** — `tally` aggregation (a `Pairs` result type) rendering into the
  `bars` and `histo` widgets.

The rest of the language — the expression layer, fusevm lowering, the full query
superset, layout, interactive pipe-shaping controls, Expect-style stream
reactions, the web target, actors, and a package manager for sharing dashboards
— is specified in [`SPEC.md`](SPEC.md) and lands across later milestones.
Nothing is faked: unrecognized widget verbs are ignored so specs stay
forward-compatible, and unbuilt features are absent, not stubbed.

## Building

arb builds as a standalone Rust crate (a lib + bin, so the language front-end is
unit-testable without a terminal):

```sh
git clone https://github.com/MenkeTechnologies/arb
cd arb
cargo build          # debug
cargo test           # tests
find / | ./target/debug/arb
```

## License

MIT — free and open source. See [`LICENSE`](LICENSE).
