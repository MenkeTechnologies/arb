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

pub mod actor;
pub mod algo;
pub mod ast;
pub mod banner;
pub mod cache;
/// `cli` submodule: the command line, callable as a library.
pub mod cli;
pub mod dap;
pub mod err;
pub mod expr;
pub mod fzf;
/// `hosted` submodule: running arb inside another process.
pub mod hosted;
pub mod jq;
pub mod jqval;
pub mod lexer;
pub mod lsp;
pub mod parser;
pub mod pkg;
pub mod pty;
pub mod query;
pub mod repl;
pub mod rust_ffi;
pub mod serve;
pub mod sniff;
pub mod spec;
pub mod stream;
pub mod testrun;
pub mod theme;
pub mod tiers;
pub mod tui;
pub mod web;
pub mod xpath;
