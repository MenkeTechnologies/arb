//! arb wiring for inline Rust FFI (`rust { ... }` blocks).
//!
//! fusevm does the heavy lifting: [`fusevm::RustSugar`] rewrites the block at the
//! source level, and [`fusevm::ffi`] compiles it to a cached cdylib, `dlopen`s it,
//! and marshals calls. This module only supplies arb's Tcl-flavored
//! [`fusevm::RustSugar`] config and the desugar entry the parser calls before
//! lexing. The emitted `__rust_compile` command is handled at spec-build time in
//! [`crate::spec`]; a registered export is then reachable as a `name(args)` call
//! in the numeric expression layer ([`crate::expr`]).

use fusevm::RustSugar;

/// Emit the arb command a `rust { ... }` block desugars to: a `__rust_compile`
/// verb whose first arg is the base64-encoded block body (a `"..."` string, so
/// the base64 `+`/`/` never trip arb's regex/xpath lexing) and whose second is
/// the source line. `spec::build_into` runs it, registering the block's exports.
fn emit(b64: &str, line: usize) -> String {
    format!("__rust_compile \"{b64}\" {line}")
}

/// arb desugar config: Tcl-flavored `#` line comments, no block comments, and a
/// newline/`;` command boundary. A bare `rust` verb doesn't exist in arb, so
/// `rust {` at a command boundary only ever matches an intended FFI block —
/// never ordinary `{ ... }` command-body usage.
pub const SUGAR: RustSugar = RustSugar {
    keyword: "rust",
    line_comments: &["#"],
    block_comment: None,
    newline_boundary: true,
    emit,
};

/// Rewrite every top-level `rust { ... }` block in arb source into a
/// `__rust_compile "<b64>" LINE` command, before lexing. A no-op when the source
/// contains no `rust` keyword (fusevm's scanner fast-paths on that).
pub fn desugar(src: &str) -> String {
    SUGAR.desugar(src)
}

#[cfg(test)]
mod tests {
    #[test]
    fn desugars_rust_block_at_line_start() {
        let src = "rust { pub extern \"C\" fn add(a: i64, b: i64) -> i64 { a + b } }\ngauge .g -max 42\n";
        let out = super::desugar(src);
        assert!(out.contains("__rust_compile"), "no builtin command: {out}");
        assert!(!out.contains("pub extern"), "Rust body leaked: {out}");
        assert!(out.contains("gauge .g -max 42"), "trailing spec dropped: {out}");
    }

    #[test]
    fn leaves_ordinary_arb_untouched() {
        // `{ ... }` command bodies must not be mistaken for a `rust` block.
        let src = "source .g { in; count }\ngauge .g -max 8\n";
        assert_eq!(super::desugar(src), src);
    }
}
