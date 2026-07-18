//! arb — visualize and modify Unix pipelines.
//!
//! The crate is a lib + bin: the language front-end (lexer/parser/interpreter)
//! lives here so it is unit-testable without a terminal; the `arb` binary
//! (`src/main.rs`) wires it to stdin and the ratatui render loop.
//!
//! M1 scope: the Tcl-flavored reader, the declarative widget/`source` subset,
//! and rendering `text`/`tail`/`list` widgets fed from stdin. The expression
//! layer, fusevm lowering, query verbs, and the rest of SPEC.md are later
//! milestones — not stubbed here as if present.

pub mod ast;
pub mod lexer;
pub mod parser;
pub mod query;
pub mod spec;
pub mod stream;
pub mod tui;
