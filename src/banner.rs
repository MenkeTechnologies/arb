//! The REPL banner (shown only for the interactive `arb --repl` / bare-TTY
//! launch, never for a piped render or the headless dump path).

use nu_ansi_term::Color;

/// ANSI-Shadow wordmark, matching the house cyberpunk style shared with the
/// sibling frontends (rubylang / strykelang / vimlrs).
pub const WORDMARK: &str = r#"
 █████╗ ██████╗ ██████╗
██╔══██╗██╔══██╗██╔══██╗
███████║██████╔╝██████╔╝
██╔══██║██╔══██╗██╔══██╗
██║  ██║██║  ██║██████╔╝
╚═╝  ╚═╝╚═╝  ╚═╝╚═════╝
"#;

/// Print the banner + a one-line subtitle for the interactive REPL.
pub fn print_banner() {
    let v = env!("CARGO_PKG_VERSION");
    println!("{}", Color::Cyan.bold().paint(WORDMARK));
    println!(
        "{} {}  {}",
        Color::Purple.bold().paint("arb"),
        Color::White.paint(format!("v{v}")),
        Color::DarkGray.paint("visualize + modify Unix pipelines — a TUI for every pipeline")
    );
}
