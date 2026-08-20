//! `arb` — visualize and modify Unix pipelines.
//!
//! The binary is only an entry point. Everything it does lives in the library,
//! in [`arb::cli`], so a host that links arb in can run the same command line
//! without a process — see [`arb::cli::run_argv`], which the `arb` shell
//! builtin in zshrs-native dispatches to.

use std::process::ExitCode;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    ExitCode::from(arb::cli::run_argv(&argv) as u8)
}
