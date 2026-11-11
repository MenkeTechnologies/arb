//! Running an arb spec from a host program, without a process or a terminal.
//!
//! [`run_program`] is `printf %s INPUT | arb -e SRC | cat`: the same headless
//! path the binary takes when its stdin and stdout are both pipes and no
//! terminal is reachable (`cli::run_headless`). An `out { … }` pipeline
//! reshapes the input, a spec with no `out` passes the input through, and a
//! spec that reads no stream prints its widget summary.

use std::io::{ErrorKind, Write};

/// Stack for the thread a run executes on: jq programs recurse as deep as
/// they like (see the `arb` binary's `main`, which reserves the same). The
/// reservation is virtual; pages are only touched by a program that recurses.
const STACK_BYTES: usize = 4 << 30;

/// Run `src` as a complete program with `input` as its stdin and return
/// everything it wrote to stdout.
///
/// `src` is spec text, as `arb -e SRC` takes it. Each call builds the spec
/// from scratch on a fresh thread, so nothing carries over between calls and
/// concurrent calls write to their own buffers. A run that ends with a nonzero
/// status (a spec error, a jq error, `halt_error`, a panic) returns `Err` with
/// what arb wrote to stderr, or `exit status N` when it wrote nothing; a run
/// that ends with status 0, including jq's `halt`, returns `Ok` with the
/// output produced so far.
pub fn run_program(src: &str, input: &str) -> Result<String, String> {
    let (src, input) = (src.to_owned(), input.to_owned());
    let run = move || run_here(&src, &input);
    std::thread::Builder::new()
        .stack_size(STACK_BYTES)
        .spawn(run)
        .map_err(|e| format!("arb: cannot start the run thread: {e}"))?
        .join()
        .unwrap_or_else(|_| Err("arb: run panicked".to_string()))
}

/// One run on the current thread. [`crate::hosted::run`] turns arb's `exit`
/// calls and panics into a status instead of ending the process.
fn run_here(src: &str, input: &str) -> Result<String, String> {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = crate::hosted::run(|| {
        match crate::cli::run_headless(src, input.as_bytes(), &mut out, &mut err) {
            Ok(()) => 0,
            Err(e) if e.kind() == ErrorKind::BrokenPipe => 0,
            Err(e) => {
                let _ = writeln!(err, "arb: {e}");
                1
            }
        }
    });
    if status == 0 {
        return Ok(String::from_utf8_lossy(&out).into_owned());
    }
    let msg = String::from_utf8_lossy(&err).trim_end().to_string();
    Err(if msg.is_empty() {
        format!("exit status {status}")
    } else {
        msg
    })
}
