//! jq's date builtins, transcribed from jq 1.8.2's builtin.c (`f_strptime`,
//! `jv2tm`, `f_mktime`, `f_gmtime`, `f_strftime`). Every expectation was measured
//! against `jq -rc` 1.8.2; they are pinned here so CI needs no jq. Only UTC
//! paths are probed, so the results do not depend on the machine's zone.

use arb::parser::parse;
use arb::query::{eval, QueryResult};

/// `out { in.json; FILTER }` over one `null` line: the output lines, or the
/// refusal text.
fn run(filter: &str) -> Result<Vec<String>, String> {
    let src = format!("tail .x\nsource .x {{ in.json; {filter} }}");
    let cmds = parse(&src).map_err(|e| e.to_string())?;
    let spec = arb::spec::build(&cmds).map_err(|e| e.to_string())?;
    let ops = spec.widgets[0].source.as_ref().unwrap().pipeline.clone();
    match eval(&ops, &["null".to_string()], 1.0) {
        QueryResult::Lines(l) => Ok(l),
        QueryResult::Error(e) => Err(e),
        other => Err(format!("unexpected result shape: {other:?}")),
    }
}

#[track_caller]
fn same(filter: &str, want: &[&str]) {
    assert_eq!(
        run(filter),
        Ok(want.iter().map(|s| s.to_string()).collect()),
        "{filter}"
    );
}

/// Fields the format does not set stay as jq zeroes them, wday/yday come from
/// Gauss's formula and the month table, and whitespace-led leftover input is
/// appended as a string.
#[test]
fn strptime_keeps_unset_fields_and_leftover_input() {
    same(
        r#""10:30" | strptime("%H:%M")"#,
        &["[1900,0,0,10,30,0,6,-1]"],
    );
    same(r#""2024" | strptime("%Y")"#, &["[2024,0,0,0,0,0,0,-1]"]);
    same(
        r#""2024-02" | strptime("%Y-%m")"#,
        &["[2024,1,0,0,0,0,3,30]"],
    );
    same(
        r#""2024 x" | strptime("%Y")"#,
        &[r#"[2024,0,0,0,0,0,0,-1," x"]"#],
    );
    same(
        r#""2024-03-01" | strptime("%Y-%m-%d")"#,
        &["[2024,2,1,0,0,0,5,60]"],
    );
    same(
        r#""2024 x" | try strptime("%Yx") catch ."#,
        &[r#"date "2024 x" does not match format "%Yx""#],
    );
}

/// `jv2tm` takes up to eight fields (missing ones are 0), and refuses a
/// non-number. (Where `timegm` itself fails is the libc's call, so it is not
/// pinned here.)
#[test]
fn mktime_reads_short_arrays_and_refuses_what_jq_refuses() {
    same("[2024,2,15] | mktime", &["1710460800"]);
    same("[2024] | mktime", &["1703980800"]);
    same(
        r#"[2024,2,15,10,0,"x"] | try mktime catch ."#,
        &["mktime requires parsed datetime inputs"],
    );
}

/// `gmtime` truncates the seconds and takes the fraction against the floor;
/// `strftime`'s `%s` is the UTC epoch.
#[test]
fn gmtime_truncates_and_strftime_s_is_utc() {
    same("-1.5 | gmtime", &["[1969,11,31,23,59,59.5,3,364]"]);
    same(
        r#"1710496800 | strftime("%s %Z %z")"#,
        &["1710496800 UTC +0000"],
    );
    same(
        r#"[2024,14,40,10,0,0,0,0] | strftime("%Y-%m-%d")"#,
        &["2025-04-09"],
    );
    same(
        r#"1e20 | try gmtime catch ."#,
        &["error converting number of seconds since epoch to datetime"],
    );
}

/// `nth/2` refuses a negative index with jq 1.8's own wording.
#[test]
fn nth_negative_index_wording() {
    same(
        "try nth(-1; 1,2) catch .",
        &["nth doesn't support negative indices"],
    );
}
