//! `arb` — visualize and modify Unix pipelines.
//!
//! The binary is only an entry point. Everything it does lives in the library,
//! in [`arb::cli`], so a host that links arb in can run the same command line
//! without a process — see [`arb::cli::run_argv`], which the `arb` shell
//! builtin in zshrs-native dispatches to.

use std::process::ExitCode;

/// Stack for the thread that runs the command line. The jq engine evaluates
/// recursively, and a jq program may recurse as deep as it likes — jq keeps its
/// own stack on the heap, so `def f: if . < 100000 then .+1|f else . end; 0|f`
/// is fine there. The 8 MiB main-thread stack aborted the process on a few
/// thousand levels. The reservation is virtual: pages are only touched when a
/// program actually recurses that deep.
const STACK_BYTES: usize = 4 << 30;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let run = std::thread::Builder::new()
        .stack_size(STACK_BYTES)
        .spawn(move || arb::cli::run_argv(&argv));
    match run.map(std::thread::JoinHandle::join) {
        Ok(Ok(code)) => ExitCode::from(code as u8),
        // No thread (the reservation was refused) or a panic: the panic message
        // is already on stderr; run on the main thread in the first case.
        Ok(Err(_)) => ExitCode::from(101),
        Err(_) => ExitCode::from(arb::cli::run_argv(&std::env::args().collect::<Vec<_>>()) as u8),
    }
}
