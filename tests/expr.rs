//! Expression -> fusevm bytecode -> VM execution tests. These prove arb's
//! computational core actually runs on the fusevm VM.

use arb::expr::{eval, parse};

#[test]
fn precedence_on_fusevm() {
    assert_eq!(eval(&parse("2 + 3 * 4").unwrap(), 0.0).unwrap(), 14.0);
}

#[test]
fn division_is_float() {
    assert_eq!(eval(&parse("x / 4").unwrap(), 10.0).unwrap(), 2.5);
}

#[test]
fn parens_and_unary_minus() {
    assert_eq!(eval(&parse("-(1 + 2) * 3").unwrap(), 0.0).unwrap(), -9.0);
}

#[test]
fn modulo() {
    assert_eq!(eval(&parse("x % 3").unwrap(), 10.0).unwrap(), 1.0);
}

#[test]
fn uses_x_directly() {
    assert_eq!(eval(&parse("x").unwrap(), 42.0).unwrap(), 42.0);
}

#[test]
fn rejects_trailing_garbage() {
    assert!(parse("1 + ) 2").is_err());
}
