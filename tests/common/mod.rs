//! Shared harness for the differential tests that run one jq program through
//! arb's `in.json` pipeline and through the real `jq` binary.

use arb::parser::parse;
use arb::query::{eval, QueryResult};
use std::io::Write;
use std::process::{Command, Stdio};

/// Run `filter` through arb's `out { in.json; … }` pipeline over `input` lines.
pub fn arb_run(filter: &str, input: &[String]) -> Result<Vec<String>, String> {
    let src = format!("tail .x\nsource .x {{ in.json; {filter} }}");
    let cmds = parse(&src).map_err(|e| e.to_string())?;
    let spec = arb::spec::build(&cmds).map_err(|e| e.to_string())?;
    let pipeline = spec.widgets[0]
        .source
        .as_ref()
        .ok_or("no source")?
        .pipeline
        .clone();
    match eval(&pipeline, input, 1.0) {
        QueryResult::Lines(l) => Ok(l),
        QueryResult::Error(e) => Err(e),
        other => Err(format!("unexpected result shape: {other:?}")),
    }
}

/// `jq -rc filter` over the same lines; `None` when jq exits non-zero.
pub fn jq_run(filter: &str, input: &[String]) -> Option<Vec<String>> {
    let mut child = Command::new("jq")
        .args(["-rc", filter])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    // A filter that stops early closes jq's stdin; EPIPE is a normal outcome.
    let _ = stdin.write_all((input.join("\n") + "\n").as_bytes());
    drop(stdin);
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    Some(text.lines().map(str::to_string).collect())
}

/// True when the `jq` on PATH is the 1.8 line the expectations were measured
/// against; otherwise prints a SKIP notice (a different reference is a reason
/// to skip, never to pass).
pub fn reference_ok() -> bool {
    let Ok(out) = Command::new("jq").arg("--version").output() else {
        eprintln!("SKIP: no `jq` on PATH — the differential probes need the reference");
        return false;
    };
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.starts_with("jq-1.8") {
        return true;
    }
    eprintln!("SKIP: reference is `{v}`, expectations were measured against jq-1.8");
    false
}
