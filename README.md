```
            _
  __ _ _ __| |__
 / _` | '__| '_ \
| (_| | |  | |_) |
 \__,_|_|  |_.__/
```

**arb** — visualize and modify Unix pipelines. Pipe a stream in and arb spawns a
dynamic TUI (and, later, a web page) built from a declarative, Tcl/Tk-flavored
spec. It is a `jq`/`xpath`/`css`/`yq` superset, an interactive megafilter/map
over the live passthrough, and it runs on the [`fusevm`](https://github.com/MenkeTechnologies/fusevm)
bytecode VM + Cranelift JIT.

> *A TUI for every pipeline.*

## Status

Early. **Milestone 0** ships zero-config live-tail: pipe any stream in and watch
it in a full-screen TUI.

```sh
find / | arb          # live tail of the stream, with count + rate
```

With no controlling terminal (piped onward / CI), arb prints a headless summary
instead of a TUI.

The full language — spec interpretation, widgets, layout, the query superset,
interactive pipe-shaping controls, Expect-style stream reactions, web target,
actors, and a package manager for sharing dashboards — is specified in
[`SPEC.md`](SPEC.md) and lands across later milestones.

## Install

```sh
cargo install --path .
```

## License

MIT
