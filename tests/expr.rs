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
fn comparison_predicates_on_fusevm() {
    use arb::expr::eval_pred;
    assert!(eval_pred(&parse("x > 100").unwrap(), 150.0).unwrap());
    assert!(!eval_pred(&parse("x > 100").unwrap(), 50.0).unwrap());
    assert!(eval_pred(&parse("x <= 10").unwrap(), 10.0).unwrap());
    assert!(eval_pred(&parse("x == 5").unwrap(), 5.0).unwrap());
    assert!(eval_pred(&parse("x != 5").unwrap(), 6.0).unwrap());
    assert!(eval_pred(&parse("x >= 3").unwrap(), 3.0).unwrap());
}

#[test]
fn field_refs_resolve_via_closure() {
    use arb::expr::{eval_ctx, eval_pred_ctx};
    let r = |name: &str| match name {
        "amount" => 150.0,
        "fee" => 5.0,
        _ => f64::NAN,
    };
    assert_eq!(eval_ctx(&parse("amount - fee").unwrap(), 0.0, &r).unwrap(), 145.0);
    assert!(eval_pred_ctx(&parse("amount > 100").unwrap(), 0.0, &r).unwrap());
    assert!(!eval_pred_ctx(&parse("amount < fee").unwrap(), 0.0, &r).unwrap());
}

#[test]
fn rejects_trailing_garbage() {
    assert!(parse("1 + ) 2").is_err());
}

#[test]
fn logical_and_or_not_on_fusevm() {
    use arb::expr::eval_pred;
    // and: true iff both sides truthy
    assert!(eval_pred(&parse("x > 1 and x < 10").unwrap(), 5.0).unwrap());
    assert!(!eval_pred(&parse("x > 1 and x < 10").unwrap(), 50.0).unwrap());
    // or: true iff either side truthy
    assert!(eval_pred(&parse("x < 2 or x > 40").unwrap(), 1.0).unwrap());
    assert!(eval_pred(&parse("x < 2 or x > 40").unwrap(), 50.0).unwrap());
    assert!(!eval_pred(&parse("x < 2 or x > 40").unwrap(), 20.0).unwrap());
    // not: negates truthiness
    assert!(eval_pred(&parse("not x").unwrap(), 0.0).unwrap());
    assert!(!eval_pred(&parse("not x").unwrap(), 5.0).unwrap());
    // precedence: `and` binds tighter than `or`
    assert!(eval_pred(&parse("x > 100 or x > 1 and x < 3").unwrap(), 2.0).unwrap());
    assert!(!eval_pred(&parse("x > 100 or x > 1 and x < 3").unwrap(), 5.0).unwrap());
}

#[test]
fn keyword_not_confused_with_identifier_prefix() {
    // `android` must lex as a field, not `and` + `roid`.
    let e = parse("android == 1").unwrap();
    use arb::expr::eval_pred_ctx;
    assert!(eval_pred_ctx(&e, 0.0, &|n| if n == "android" { 1.0 } else { f64::NAN }).unwrap());
}
