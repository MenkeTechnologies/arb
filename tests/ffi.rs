//! End-to-end inline Rust FFI: a `rust { ... }` block in arb source is desugared
//! (`rust_ffi`), compiled to a cdylib via `rustc` and registered (`spec::build`
//! runs the `__rust_compile` verb), then its export is called by name from an
//! arb `where`/`map`/`calc` expression (`expr::parse` + `expr::eval`). Requires
//! `rustc` on PATH (always present in Rust CI); skips cleanly otherwise so a
//! toolchain-less environment never reports a false failure.
//!
//! arb's expression VM is numeric (f64), so this exercises the two in-scope
//! signature classes — integer-arity and float-arity. The `*const c_char`
//! string signature is intentionally out of scope for arb (no strings on its
//! expression VM).

use arb::{expr, parser, spec};

fn rustc_available() -> bool {
    std::process::Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Register a `rust { ... }` block by driving arb's real parse+build path.
fn register(block: &str) {
    let cmds = parser::parse(block).expect("rust block desugars + parses");
    spec::build(&cmds).expect("build runs __rust_compile and registers the export");
}

/// Evaluate a numeric arb expression (the calc/where/map core) with no input.
fn eval(src: &str) -> f64 {
    let e = expr::parse(src).expect("expression parses");
    expr::eval(&e, 0.0).expect("expression evaluates")
}

#[test]
fn integer_export_callable_from_expression() {
    if !rustc_available() {
        eprintln!("skipping FFI test: rustc not on PATH");
        return;
    }
    // Distinct name so this test's registry entry never collides with another's.
    register(r#"rust { pub extern "C" fn arb_ffi_add(a: i64, b: i64) -> i64 { a + b } }"#);
    assert_eq!(eval("arb_ffi_add(21, 21)"), 42.0);
    // Nested / expression-valued args resolve on the same VM before marshalling.
    assert_eq!(eval("arb_ffi_add(20 + 1, 3 * 7)"), 42.0);
}

#[test]
fn float_export_callable_from_expression() {
    if !rustc_available() {
        eprintln!("skipping FFI test: rustc not on PATH");
        return;
    }
    register(r#"rust { pub extern "C" fn arb_ffi_mulf(x: f64, y: f64, z: f64) -> f64 { x * y * z } }"#);
    assert_eq!(eval("arb_ffi_mulf(1.5, 2.0, 3.0)"), 9.0);
}

#[test]
fn unregistered_call_is_nan_not_a_panic() {
    // A call to a name no `rust { }` block exported degrades to NaN (like an
    // unresolved control), never a panic — no rustc needed for this path.
    let v = eval("arb_ffi_never_registered(1, 2)");
    assert!(v.is_nan(), "expected NaN for an unregistered FFI call, got {v}");
}
