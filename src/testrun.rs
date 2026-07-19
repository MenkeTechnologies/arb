//! In-language test runner for `arb --test`. A spec's `test "NAME" { given …;
//! run { … }; want … }` blocks are evaluated headlessly — each `run` pipeline is
//! applied to its `given` lines via the same [`crate::query::eval`] the widgets
//! use, and the flattened output is compared to `want`. Output is TAP-flavored
//! so it drops into any TAP consumer; `run` returns the pass/fail tally so the
//! CLI can exit 0 (all passed) or 1 (any failed).

use crate::query::{self, QueryResult};
use crate::spec::TestCase;

/// The outcome of running a spec's tests.
pub struct Report {
    pub passed: usize,
    pub failed: usize,
    pub text: String,
}

/// Flatten a [`QueryResult`] to the output lines a `want` clause is compared
/// against: a scalar renders like a control value, pairs as `key\tvalue`.
pub fn result_to_lines(r: QueryResult) -> Vec<String> {
    match r {
        QueryResult::Lines(v) => v,
        QueryResult::Scalar(s) => vec![query::fmt_scalar(s)],
        QueryResult::Pairs(p) => p.iter().map(|(k, v)| format!("{k}\t{v}")).collect(),
    }
}

/// Run every test case, producing a TAP report + the pass/fail tally.
pub fn run(tests: &[TestCase]) -> Report {
    let mut text = format!("1..{}\n", tests.len());
    let (mut passed, mut failed) = (0, 0);
    for (i, t) in tests.iter().enumerate() {
        let got = result_to_lines(query::eval(&t.ops, &t.given, 0.0));
        if got == t.want {
            passed += 1;
            text.push_str(&format!("ok {} - {}\n", i + 1, t.name));
        } else {
            failed += 1;
            text.push_str(&format!("not ok {} - {}\n", i + 1, t.name));
            // Diagnostic block (YAML-ish, TAP-compatible): the mismatch.
            text.push_str("  ---\n");
            text.push_str(&format!("  want: {:?}\n", t.want));
            text.push_str(&format!("  got:  {:?}\n", got));
            text.push_str("  ---\n");
        }
    }
    text.push_str(&format!("# {passed} passed, {failed} failed\n"));
    Report {
        passed,
        failed,
        text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;
    use crate::spec::build;

    fn tests_of(src: &str) -> Vec<TestCase> {
        build(&parse(src).unwrap()).unwrap().tests
    }

    #[test]
    fn passing_and_failing_cases_are_tallied() {
        let src = "\
test \"keeps 5xx\" {\n\
  given \"200 ok\" \"503 down\" \"404 x\"\n\
  run { in; match /5\\d\\d/ }\n\
  want \"503 down\"\n\
}\n\
test \"wrong on purpose\" {\n\
  given \"a\" \"b\"\n\
  run { in; count }\n\
  want \"3\"\n\
}\n";
        let r = run(&tests_of(src));
        assert_eq!((r.passed, r.failed), (1, 1));
        assert!(r.text.contains("ok 1 - keeps 5xx"));
        assert!(r.text.contains("not ok 2 - wrong on purpose"));
        assert!(r.text.contains("want: [\"3\"]"));
        assert!(r.text.contains("got:  [\"2\"]"));
    }

    #[test]
    fn scalar_and_pairs_results_flatten() {
        // A reducer (count -> scalar) compares against a single want line.
        let count =
            tests_of("test \"n\" { given \"a\" \"b\" \"c\"; run { in; count }; want \"3\" }");
        assert_eq!(run(&count).failed, 0);
        // A tally (-> pairs) flattens to key\tvalue lines.
        let tally = tests_of(
            "test \"t\" { given \"a\" \"a\" \"b\"; run { in; tally }; want \"a\t2\" \"b\t1\" }",
        );
        assert_eq!(run(&tally).failed, 0);
    }

    #[test]
    fn jq_and_xpath_pipelines_are_testable() {
        let jq = tests_of(
            "test \"jq path\" { given \"{\\\"a\\\":{\\\"b\\\":7}}\"; run { in.json; .a.b }; want \"7\" }",
        );
        assert_eq!(run(&jq).failed, 0);
        let xp = tests_of(
            "test \"xpath\" { given \"<a href=\\\"x\\\">1</a>\"; run { in.html; //a/@href }; want \"x\" }",
        );
        assert_eq!(run(&xp).failed, 0);
    }
}
