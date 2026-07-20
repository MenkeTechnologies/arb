//! arb — visualize and modify Unix pipelines.
//!
//! Pipe a stream in and arb renders a dynamic TUI built from a declarative spec.
//! With no spec it synthesizes a full-screen `tail` of stdin. With no controlling
//! terminal on stdout (piped onward / CI) it prints the parsed spec, each source
//! pipeline, and its evaluated result instead of a TUI — correct behavior and the
//! headless-test path.
//!
//! M1/M2a interpret the declarative subset (widgets, `source` query pipelines,
//! binds). The expression layer, fusevm lowering, web target, actors, and package
//! manager are later milestones (see SPEC.md) and are not faked here.

use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use clap::Parser;

use arb::query::{self, QueryOp, QueryResult};
use arb::spec::{self, Spec};
use arb::stream::StreamState;
use arb::{cache, parser, tui};

// Cyberpunk `-h` help — help template, ASCII banner, and footer, ported from the
// `temprs` (`tp -h`) house style (`temprs/src/model/opts.rs`): `{before-help}`
// banner, cyberpunk section dividers, green `//` per-option prefixes, and a
// `── SYSTEM ──` footer. clap emits raw ANSI in these verbatim.
const HELP_TEMPLATE: &str = "
{before-help}
{about}

\x1b[33m  USAGE:\x1b[0m {usage}

\x1b[36m  ── OPTIONS ────────────────────────────────────────────\x1b[0m
{options}
\x1b[36m  ── POSITIONAL ─────────────────────────────────────────\x1b[0m
{positionals}
{after-help}";

const BANNER: &str = concat!(
    "\x1b[36m █████╗ ██████╗ ██████╗\x1b[0m\n",
    "\x1b[36m██╔══██╗██╔══██╗██╔══██╗\x1b[0m\n",
    "\x1b[35m███████║██████╔╝██████╔╝\x1b[0m\n",
    "\x1b[35m██╔══██║██╔══██╗██╔══██╗\x1b[0m\n",
    "\x1b[31m██║  ██║██║  ██║██████╔╝\x1b[0m\n",
    "\x1b[31m╚═╝  ╚═╝╚═╝  ╚═╝╚═════╝\x1b[0m\n",
    "\x1b[36m ┌──────────────────────────────────────────────────────┐\x1b[0m\n",
    "\x1b[36m │ STATUS: ONLINE  // SIGNAL: ████████░░ // v",
    env!("CARGO_PKG_VERSION"),
    "\x1b[36m      │\x1b[0m\n",
    "\x1b[36m └──────────────────────────────────────────────────────┘\x1b[0m\n",
    "\x1b[35m  >> A TUI FOR EVERY PIPELINE // FULL SPECTRUM <<\x1b[0m"
);

const AFTER: &str = concat!(
    "\x1b[36m  ── SYSTEM ─────────────────────────────────────────\x1b[0m\n",
    "\x1b[35m  v",
    env!("CARGO_PKG_VERSION"),
    " \x1b[0m// \x1b[33m(c) MenkeTechnologies\x1b[0m\n",
    "\x1b[35m  A TUI for every pipeline.\x1b[0m\n",
    "\x1b[33m  >>> PIPE IN. SHAPE THE STREAM. OWN YOUR OUTPUT. <<<\x1b[0m\n",
    "\x1b[36m ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░\x1b[0m"
);

/// arb — pipe a stream in, get a live TUI.
#[derive(Parser, Debug)]
#[command(
    name = "arb",
    version,
    about = "Visualize and modify Unix pipelines.",
    help_template = HELP_TEMPLATE,
    before_help = BANNER,
    after_help = AFTER,
)]
struct Cli {
    /// Dashboard spec file (.arb).
    #[arg(help = "\x1b[32m//\x1b[0m Dashboard spec file (.arb)")]
    spec: Option<String>,
    /// Inline spec, e.g. `-e 'gauge .g -max 100; source .g { in; count }'`.
    #[arg(
        short = 'e',
        long = "eval",
        help = "\x1b[32m//\x1b[0m Inline spec, e.g. -e 'gauge .g -max 100; source .g { in; count }'"
    )]
    eval: Option<String>,
    /// Run a preset / stdlib module by name, e.g. `-p logs` (== `import logs`).
    #[arg(
        short = 'p',
        long = "preset",
        help = "\x1b[32m//\x1b[0m Run a preset / stdlib module by name (== import NAME)"
    )]
    preset: Option<String>,
    /// List available presets (bundled stdlib + `~/.arb/lib`) and exit.
    #[arg(
        short = 'l',
        long = "list",
        help = "\x1b[32m//\x1b[0m List available presets (stdlib + ~/.arb/lib) and exit"
    )]
    list: bool,
    /// Override the spec's color theme (one of the 31 built-ins, e.g. neon-noir).
    #[arg(
        long = "theme",
        value_name = "NAME",
        help = "\x1b[32m//\x1b[0m Color theme (one of 31, e.g. neon-noir); overrides the spec"
    )]
    theme: Option<String>,
    /// List the 31 built-in color themes (with swatches) and exit.
    #[arg(
        long = "list-themes",
        help = "\x1b[32m//\x1b[0m List the 31 built-in color themes and exit"
    )]
    list_themes: bool,
    /// Save a spec as a named user preset in `~/.arb/lib`, then exit.
    /// Source is the `FILE` argument or `-e SRC`. E.g. `arb --save api dash.arb`.
    #[arg(
        long = "save",
        value_name = "NAME",
        help = "\x1b[32m//\x1b[0m Save a spec (FILE or -e SRC) as a named user preset, then exit"
    )]
    save: Option<String>,
    /// Install a shared `.arb` spec into the preset library, then exit.
    /// `arb --install dash.arb` (name = file stem, or override with `--as NAME`).
    #[arg(
        long = "install",
        value_name = "FILE",
        help = "\x1b[32m//\x1b[0m Install a .arb spec into the preset library, then exit"
    )]
    install: Option<String>,
    /// Name to install under (defaults to the file stem).
    #[arg(
        long = "as",
        value_name = "NAME",
        help = "\x1b[32m//\x1b[0m Name to install the spec under (default: file stem)"
    )]
    install_as: Option<String>,
    /// Uninstall a named preset from the library, then exit.
    #[arg(
        long = "uninstall",
        value_name = "NAME",
        help = "\x1b[32m//\x1b[0m Remove a named preset from the library, then exit"
    )]
    uninstall: Option<String>,
    /// List only installed (user library) presets, then exit.
    #[arg(
        long = "installed",
        help = "\x1b[32m//\x1b[0m List installed user-library presets, then exit"
    )]
    installed: bool,
    /// Interactive REPL — author + test specs against a sample buffer.
    #[arg(
        short = 'r',
        long = "repl",
        help = "\x1b[32m//\x1b[0m Interactive REPL — author + test specs against a sample buffer"
    )]
    repl: bool,
    /// Generate a static HTML dashboard from the spec to stdout, then exit
    /// (`arb -p logs --html > dash.html`).
    #[arg(
        long = "html",
        help = "\x1b[32m//\x1b[0m Emit a static HTML dashboard from the spec to stdout, then exit"
    )]
    html: bool,
    /// Validate the spec (parse + build) and exit 0/1 without reading stdin.
    #[arg(
        long = "check",
        help = "\x1b[32m//\x1b[0m Validate the spec (parse + build) and exit 0/1, no stdin"
    )]
    check: bool,
    /// Run the spec's in-language `test { … }` blocks headlessly and exit 0/1.
    #[arg(
        long = "test",
        help = "\x1b[32m//\x1b[0m Run the spec's `test { … }` blocks (TAP output), exit 0/1"
    )]
    test: bool,
    /// Serve the spec as a live browser dashboard (polls the stream over HTTP).
    #[arg(
        long = "serve",
        help = "\x1b[32m//\x1b[0m Serve the spec as a live browser dashboard on 127.0.0.1"
    )]
    serve: bool,
    /// Port for `--serve` (0 = OS-assigned, printed on start).
    #[arg(
        long = "port",
        value_name = "N",
        default_value_t = 8787,
        help = "\x1b[32m//\x1b[0m Port for --serve (0 = pick a free port)"
    )]
    port: u16,
    /// Run as a Language Server over stdio (JSON-RPC) for `.arb` specs.
    #[arg(
        long = "lsp",
        help = "\x1b[32m//\x1b[0m Run the LSP over stdio (diagnostics/completion/hover/…)"
    )]
    lsp: bool,
    /// Run as a Debug Adapter over stdio: step the stream, regex breakpoints,
    /// inspect the paused line / stats / controls (launch with program+input).
    #[arg(
        long = "dap",
        help = "\x1b[32m//\x1b[0m Run the DAP over stdio (step the stream, regex breakpoints)"
    )]
    dap: bool,
    /// Print the Tcl-flavored lexer token stream for the spec and exit.
    #[arg(
        long = "dump-tokens",
        help = "\x1b[32m//\x1b[0m Print the lexer token stream for the spec and exit"
    )]
    dump_tokens: bool,
    /// Print the parsed command-tree AST for the spec and exit.
    #[arg(
        long = "dump-ast",
        help = "\x1b[32m//\x1b[0m Print the parsed AST for the spec and exit"
    )]
    dump_ast: bool,
    /// Print the built spec's compiled query-pipeline op vectors and exit.
    #[arg(
        long = "dump-bytecode",
        help = "\x1b[32m//\x1b[0m Print the compiled pipeline op vectors for the spec and exit"
    )]
    dump_bytecode: bool,
    /// Print a numbered disassembly of the built spec's compiled pipelines and exit.
    #[arg(
        long = "disasm",
        help = "\x1b[32m//\x1b[0m Print a disassembly of the compiled pipelines and exit"
    )]
    disasm: bool,
    /// With an `out { … }` pipeline, emit results as JSON (array / number /
    /// object) instead of plain lines — pipe to `jq` or programs.
    #[arg(
        long = "json",
        help = "\x1b[32m//\x1b[0m With an out { } pipeline, emit JSON instead of plain lines"
    )]
    json: bool,
    /// fzf select mode: interactively filter the stream and pick one line;
    /// the chosen line is printed to stdout on Enter (`vim $(ls | arb --fzf)`).
    #[arg(
        long = "fzf",
        help = "\x1b[32m//\x1b[0m fzf mode: filter + select one line, printed to stdout on Enter"
    )]
    fzf: bool,
    /// Run a pipeline with arb as the interactive stage, e.g.
    /// `arb --fzf --run 'sudo find / | _ | grep foo'`. arb spawns the
    /// surrounding commands (each via `sh -c`, so globs/quotes work) and controls
    /// their fds: the producer's stdout feeds arb's stream, its stderr goes to a
    /// pane instead of corrupting the TUI.
    #[arg(
        long = "run",
        value_name = "PIPELINE",
        help = "\x1b[32m//\x1b[0m Run 'PROD | _ | CONS': arb spawns them, stderr -> pane"
    )]
    run: Option<String>,
    /// fzf preview: run this command for the line under the cursor (`{}` is the
    /// current line, shell-escaped) and show its output in a right pane, updated
    /// as you move. E.g. `arb --fzf --preview 'bat --color=always {}'`.
    #[arg(
        long = "preview",
        value_name = "CMD",
        help = "\x1b[32m//\x1b[0m fzf preview: run CMD on the cursor line ({}), output in a pane"
    )]
    preview: Option<String>,
    /// fzf prompt string (default `> `).
    #[arg(
        long = "prompt",
        value_name = "STR",
        help = "\x1b[32m//\x1b[0m fzf prompt string (default '> ')"
    )]
    prompt: Option<String>,
    /// fzf header line shown above the list.
    #[arg(
        long = "header",
        value_name = "STR",
        help = "\x1b[32m//\x1b[0m fzf header line shown above the list"
    )]
    header: Option<String>,

    // ── fzf-compatibility flags (honored) ───────────────────────────────────
    // So `arb --fzf` can drop in for the `fzf` binary (e.g. `ZPWR_FZF='arb --fzf'`).
    // Cosmetic fzf flags (--ansi/--border/--reverse/--preview-window/…) are stripped
    // from the args by `fzf_compat_args` before parsing; these are the ones arb acts on.
    /// fzf compat: exact substring match instead of fuzzy. (`-e` under `--fzf`
    /// is rewritten to this; arb's own `-e` is `--eval`.)
    #[arg(
        long = "exact",
        help = "\x1b[32m//\x1b[0m fzf: exact substring match (not fuzzy)"
    )]
    exact: bool,
    /// fzf compat: don't sort by score — keep input order.
    #[arg(
        long = "no-sort",
        help = "\x1b[32m//\x1b[0m fzf: keep input order (no score sort)"
    )]
    no_sort: bool,
    /// fzf compat: start with this query in the filter.
    #[arg(
        long = "query",
        value_name = "STR",
        help = "\x1b[32m//\x1b[0m fzf: initial query"
    )]
    query: Option<String>,
    /// fzf compat: enable multi-select (arb always allows Tab-marking).
    #[arg(
        short = 'm',
        long = "multi",
        help = "\x1b[32m//\x1b[0m fzf: multi-select (Tab marks)"
    )]
    multi: bool,
    /// fzf height: render inline in N rows (or `N%` of the terminal) at the
    /// bottom instead of full-screen, keeping the scrollback. E.g. `--height 40%`.
    #[arg(
        long = "height",
        value_name = "N|N%",
        help = "\x1b[32m//\x1b[0m fzf height: inline in N rows or N% (not full-screen)"
    )]
    height: Option<String>,
    /// Preview command after `--`: re-run over arb's current post-filter output
    /// whenever the filter changes; its stdout+stderr show in a pane, always in
    /// sync with the filter and never touching the terminal.
    /// E.g. `find / | arb -- grep error`.
    #[arg(
        last = true,
        help = "\x1b[32m//\x1b[0m Preview `-- CMD …`: re-run over the filtered output, shown in a pane"
    )]
    down: Vec<String>,
}

/// Rewrite argv so `arb --fzf` tolerates the `fzf` binary's flags (for drop-in
/// use like `ZPWR_FZF='arb --fzf'`): translate fzf's `+`-negations (`+m`→`+m`
/// disables multi, `+s`→keep order) and DROP cosmetic fzf flags arb has no
/// analog for, consuming a value for the value-taking ones. Flags arb honors
/// (`-e`, `--no-sort`, `--query`, `-m`, `--nth`, `--preview`, `--prompt`,
/// `--header`, `--height`) pass through to clap untouched.
fn fzf_compat_args(args: impl Iterator<Item = String>) -> Vec<String> {
    // Cosmetic fzf flags with no arb effect: bool (dropped) and value-taking
    // (drop the flag AND its value, whether `--flag val` or `--flag=val`).
    const DROP_BOOL: &[&str] = &[
        "--ansi",
        "--border",
        "--reverse",
        "--print-query",
        "--cycle",
        "--select-1",
        "-1",
        "--exit-0",
        "-0",
        "--sort",
        "--extended",
        "--no-mouse",
        "--filepath-word",
        "--keep-right",
    ];
    const DROP_VALUE: &[&str] = &[
        "--min-height",
        "--tiebreak",
        "--layout",
        "--info",
        "--preview-window",
        "--header-lines",
        "--with-nth",
        "--nth",
        "--bind",
        "--color",
        "--pointer",
        "--marker",
        "--border-label",
        "--tabstop",
    ];
    let argv: Vec<String> = args.collect();
    // Only rewrite fzf short-flags when in fzf mode (else `-e` stays `--eval`).
    let fzf = argv.iter().any(|a| a == "--fzf");
    let mut out = Vec::new();
    let mut it = argv.into_iter().peekable();
    while let Some(a) = it.next() {
        if fzf && a == "-e" {
            out.push("--exact".to_string());
            continue;
        }
        // fzf short flags with an attached value and no arb analog: `-nFIELDS`
        // (nth), `-dCHAR` (delimiter). Drop them (and a following value if the
        // flag stands alone, e.g. `-n 2..`).
        if fzf && (a.starts_with("-n") || a.starts_with("-d")) {
            if a.len() == 2 {
                it.next(); // `-n 2..` — consume the separate value
            }
            continue;
        }
        // fzf `+m` disables multi, `+s` disables sort (keep input order).
        match a.as_str() {
            "+m" => {
                continue; // arb is single-select unless -m is given, so +m is a no-op
            }
            "+s" => {
                out.push("--no-sort".to_string());
                continue;
            }
            _ => {}
        }
        let key = a.split('=').next().unwrap_or(&a);
        if DROP_BOOL.contains(&key) {
            continue;
        }
        if DROP_VALUE.contains(&key) {
            // `--flag=val` carries its value; `--flag val` consumes the next arg.
            if !a.contains('=') {
                it.next();
            }
            continue;
        }
        out.push(a);
    }
    out
}

fn main() -> io::Result<()> {
    // Registry verbs (`arb install|add|search|update|publish|uninstall NAME`) are
    // handled before clap so a bare verb isn't mistaken for a spec file.
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = arb::pkg::dispatch(&raw) {
        std::process::exit(code);
    }

    let cli = Cli::parse_from(fzf_compat_args(std::env::args()));

    if cli.repl {
        arb::repl::run();
        return Ok(());
    }

    // Editor frontends over stdio (stdin is a pipe, so this must come before the
    // no-args REPL / stdin-tail fallbacks below).
    if cli.lsp {
        arb::lsp::run();
        return Ok(());
    }
    if cli.dap {
        arb::dap::run();
        return Ok(());
    }

    // Introspection dumps: resolve the spec source (FILE / -e / -p), run it
    // through the requested front-end stage, print, and exit — no stream read.
    if cli.dump_tokens {
        return finish_dump(dump_tokens(&cli));
    }
    if cli.dump_ast {
        return finish_dump(dump_ast(&cli));
    }
    if cli.dump_bytecode {
        return finish_dump(dump_bytecode(&cli));
    }
    if cli.disasm {
        return finish_dump(disasm(&cli));
    }

    if cli.list {
        let mut out = io::stdout().lock();
        for (name, desc) in spec::list_presets() {
            writeln!(out, "{name:<10} {desc}")?;
        }
        return Ok(());
    }

    if cli.list_themes {
        // Each theme printed with a 6-cell 256-color swatch (its c1..c6 palette).
        let mut out = io::stdout().lock();
        for (name, pal) in arb::theme::THEMES {
            let swatch: String = pal
                .iter()
                .map(|&c| format!("\x1b[48;5;{c}m  \x1b[0m"))
                .collect();
            writeln!(out, "{swatch}  {name}")?;
        }
        return Ok(());
    }

    if let Some(name) = cli.save.clone() {
        return save_preset(&name, &cli);
    }

    if cli.installed {
        let Some(dir) = spec::lib_dir() else {
            eprintln!("arb: no preset library (set HOME or ARB_LIB)");
            std::process::exit(1);
        };
        let mut out = io::stdout().lock();
        for (name, desc) in spec::list_user_presets(&dir) {
            writeln!(out, "{name:<10} {desc}")?;
        }
        return Ok(());
    }

    if let Some(file) = cli.install.clone() {
        return install_cmd(&file, cli.install_as.as_deref());
    }

    if let Some(name) = cli.uninstall.clone() {
        let Some(dir) = spec::lib_dir() else {
            eprintln!("arb: no preset library (set HOME or ARB_LIB)");
            std::process::exit(1);
        };
        match spec::uninstall_preset(&dir, &name) {
            Ok(true) => eprintln!("arb: uninstalled `{name}`"),
            Ok(false) => {
                eprintln!("arb: `{name}` is not installed");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("arb: uninstall: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // Bare `arb` on an interactive terminal — no spec/`-e`/`-p` and nothing
    // piped in — drops into the REPL rather than erroring on the stdin-tail
    // default (which needs a pipe). A piped `find / | arb` still tails.
    // A run-pipeline: `--run`, or a positional containing the `_` arb-stage marker
    // (`arb --fzf 'find / | _ | grep x'`). When the positional IS the pipeline,
    // arb runs it rather than loading it as a spec file.
    let run_pipeline: Option<String> = cli
        .run
        .clone()
        .or_else(|| cli.spec.as_ref().filter(|s| is_arb_pipeline(s)).cloned());
    let positional_pipeline = cli.run.is_none() && run_pipeline.is_some();

    let no_spec_args = cli.spec.is_none() && cli.eval.is_none() && cli.preset.is_none();
    if no_spec_args && run_pipeline.is_none() && io::stdin().is_terminal() {
        arb::repl::run();
        return Ok(());
    }

    // Zero-config sniffing: with no spec and a piped stream (not fzf), peek the
    // first lines and auto-pick the matching stdlib preset. The peeked lines are
    // replayed into the stream so nothing is lost.
    let sniff_ok = no_spec_args && run_pipeline.is_none() && !cli.fzf && !io::stdin().is_terminal();
    let mut prelude: Vec<String> = Vec::new();
    let sniffed: Option<&'static str> = if sniff_ok {
        prelude = peek_lines(SNIFF_LINES, SNIFF_DEADLINE_MS);
        let refs: Vec<&str> = prelude.iter().map(String::as_str).collect();
        arb::sniff::sniff(&refs)
    } else {
        None
    };

    let mut spec = if positional_pipeline {
        // The positional was a pipeline, not a spec file — use the zero-config
        // default (a select list under `--fzf`, otherwise a stream tail).
        spec::build(&parser::parse(default_spec_src(cli.fzf)).unwrap()).unwrap()
    } else if let Some(name) = sniffed {
        // Sniffed a producer/data shape -> its preset; fall back to the tail if
        // the preset fails to build for any reason (never break zero-config).
        parser::parse(&format!("import {name}"))
            .and_then(|c| spec::build(&c))
            .unwrap_or_else(|_| {
                spec::build(&parser::parse(default_spec_src(false)).unwrap()).unwrap()
            })
    } else {
        match load_spec(&cli) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("arb: {e}");
                std::process::exit(1);
            }
        }
    };

    // `--theme NAME` overrides the spec's theme (or sets one on a zero-config /
    // fzf spec, so `find / | arb --fzf --theme neon-noir` recolors the picker).
    if let Some(name) = &cli.theme {
        match arb::theme::by_name(name) {
            Some(p) => spec.theme = Some(p),
            None => {
                eprintln!("arb: --theme: unknown theme `{name}` (see `arb --list-themes`)");
                std::process::exit(1);
            }
        }
    }

    // Select mode is the fzf surface expressed as a widget: `--fzf` synthesizes a
    // `select` spec, and a hand-written `select .name` widget turns it on too — so
    // fzf mode is literally a one-widget DSL spec. The select widget's `-prompt`/
    // `-header` opts feed the prompt line and header when the flags weren't passed.
    let fzf_mode = cli.fzf
        || spec
            .widgets
            .iter()
            .any(|w| w.kind == spec::WidgetKind::Select);
    let (sel_prompt, sel_header) = spec
        .widgets
        .iter()
        .find(|w| w.kind == spec::WidgetKind::Select)
        .map(|w| (w.opts.get("prompt").cloned(), w.opts.get("header").cloned()))
        .unwrap_or((None, None));

    if cli.html {
        print!("{}", arb::web::render_html(&spec));
        return Ok(());
    }

    if cli.test {
        // Run the in-language `test { … }` blocks headlessly (TAP), exit 0/1.
        let report = arb::testrun::run(&spec.tests);
        print!("{}", report.text);
        std::process::exit(if report.failed == 0 { 0 } else { 1 });
    }

    if cli.check {
        println!(
            "arb: ok \u{2014} {} widget(s){}",
            spec.widgets.len(),
            if spec.out.is_some() {
                ", out pipeline"
            } else {
                ""
            }
        );
        return Ok(());
    }

    // `--run 'PROD | arb | CONS'`: arb spawns the surrounding commands and owns
    // their fds. The producer's stdout feeds the stream, its stderr an error pane.
    let (run_producer, consumer) = match &run_pipeline {
        Some(p) => {
            let (pr, co) = parse_pipeline(p);
            (Some(pr), co)
        }
        None => (None, None),
    };
    // A spec-level `spawn CMD` or `< FILE` (§7) is an input source like `--run`'s
    // producer; CLI `--run` wins, then `spawn`, then `< FILE` (as `cat -- FILE`).
    // Either way the producer's stdout feeds the stream in place of stdin.
    let producer = run_producer.or_else(|| spec.spawn.clone()).or_else(|| {
        spec.source_file
            .as_deref()
            .map(|f| format!("cat -- {}", shell_quote(f)))
    });

    // A `! CMD every Ns` poll source (§7) feeds the stream on a timer. `--run`
    // and spec `spawn`/`< file` all populate `producer` and are mutually
    // exclusive with `poll` at the spec level, so `poll` only matters when no
    // producer took over (lets CLI `--run` win, mirroring `spawn`).
    let poll = if producer.is_some() {
        None
    } else {
        spec.poll.clone()
    };

    let needs_stdin =
        fzf_mode || spec.out.is_some() || spec.widgets.iter().any(|w| w.source.is_some());
    // A producer (`--run`/`spawn`/`< file`) or a `poll` source supplies the
    // stream itself, so stdin is not required even on an interactive tty.
    if needs_stdin && producer.is_none() && poll.is_none() && io::stdin().is_terminal() {
        eprintln!("arb: spec reads stdin but nothing is piped — e.g. `find / | arb`");
        std::process::exit(2);
    }

    // fzf select mode keeps every line (no ring drop), so marks persist as the
    // stream grows. An `out { … }` reshape also needs every line — its reducers
    // (`count`/`sum`/`sort`) run over the whole accumulated stream, so a bounded
    // ring would silently drop the oldest lines and yield a wrong total. The live
    // TUI/served dashboard uses the bounded ring (only the visible tail matters).
    //
    // The retention is a large FINITE cap, not `usize::MAX`: a live producer
    // (`spawn tail -f`, a `! poll` source) is an unbounded stream, and an
    // uncapped buffer would grow until the process OOMs. This ceiling keeps every
    // line for any realistic finite input (a piped file/command that ends) — so
    // reducers stay exact — while windowing a genuinely infinite stream to its
    // most recent lines instead of exhausting memory.
    const MAX_RETAIN: usize = 2_000_000;
    let state = Arc::new(Mutex::new(if fzf_mode || spec.out.is_some() {
        StreamState::with_cap(MAX_RETAIN)
    } else {
        StreamState::new()
    }));

    // `--serve`: the same spec as a live browser dashboard. Feed the stream in the
    // background (spawned producer or stdin) and run the HTTP server (blocks).
    if cli.serve {
        if let Some(prod) = &producer {
            if let Err(e) = spawn_producer(prod, state.clone()) {
                eprintln!("arb: run: {e}");
                std::process::exit(1);
            }
        } else if let Some((cmd, dur)) = &poll {
            poll_producer(cmd, *dur, state.clone());
        } else if needs_stdin {
            let controls = Arc::new(Mutex::new(tui::Controls::default()));
            spawn_reader(state.clone(), false, controls, None, prelude.clone());
        }
        return arb::serve::serve(spec, state, cli.port);
    }

    // Interactive TUI whenever a controlling terminal is reachable (a `/dev/tty`
    // we can open — see `tui::events_available`); the TUI renders THERE, not to
    // stdout, so it runs even when stdout is piped onward. Exception: a STATIC
    // `out { … }` reshape (no live control) with a downstream consumer takes the
    // headless data path below so the consumer gets the transformed stream. But an
    // INTERACTIVE out — `input` widgets feeding `out { … apply .x }` — keeps the
    // TUI up so typing reshapes the piped stream live (the megafilter/map). With
    // no controlling tty (CI) it falls through, instead of crashing on the reader.
    let interactive_out = spec
        .widgets
        .iter()
        .any(|w| w.kind == spec::WidgetKind::Input)
        && spec
            .out
            .as_ref()
            .is_some_and(|ops| ops.iter().any(|op| matches!(op, QueryOp::Apply(_))));
    let downstream_reshape = spec.out.is_some() && !io::stdout().is_terminal() && !interactive_out;
    if tui::events_available() && !downstream_reshape {
        let controls = Arc::new(Mutex::new(tui::Controls::default()));

        // Feed the stream: a spawned producer (`--run`/`spawn`/`< file`), a
        // `! CMD every Ns` poll loop, or stdin.
        let err_pane = if spec.spawn_pty && run_pipeline.is_none() && producer.is_some() {
            // `spawn -pty CMD`: run on a PTY so the child acts interactive, and
            // keep the stdin writer so `send "…"` can drive it (Expect). The PTY
            // merges stdout+stderr, so there's no separate error pane.
            let prod = producer.as_deref().unwrap();
            match arb::pty::spawn_pty_producer(prod, state.clone()) {
                Ok(writer) => {
                    controls.lock().unwrap().pty_writer = Some(writer);
                    None
                }
                Err(e) => {
                    eprintln!("arb: spawn -pty: {e}");
                    std::process::exit(1);
                }
            }
        } else if let Some(prod) = &producer {
            match spawn_producer(prod, state.clone()) {
                Ok(es) => Some((es, "stderr".to_string())),
                Err(e) => {
                    eprintln!("arb: run: {e}");
                    std::process::exit(1);
                }
            }
        } else if let Some((cmd, dur)) = &poll {
            Some((
                poll_producer(cmd, *dur, state.clone()),
                "stderr".to_string(),
            ))
        } else {
            if needs_stdin {
                // Tee the live filtered stream to stdout (only when piped onward,
                // so arb never blocks or corrupts the terminal). fzf mode never
                // tees. The filter narrows the passthrough live — the megafilter.
                // An `out { … }` pipeline additionally MAPS each line as it flows,
                // resolving `apply .name` against live `input` values — so typing
                // in a control reshapes the downstream pipe in real time.
                let tee = !fzf_mode && !io::stdout().is_terminal();
                spawn_reader(
                    state.clone(),
                    tee,
                    controls.clone(),
                    spec.out.clone(),
                    prelude.clone(),
                );
            }
            None
        };

        // Preview pane. In fzf mode: a per-item `--preview` (run CMD on the cursor
        // line). Otherwise: `arb -- CMD` / the `| CONS` consumer, re-run over the
        // whole filtered output.
        let down_cmd: Vec<String> = match &consumer {
            Some(c) => vec!["sh".to_string(), "-c".to_string(), c.clone()],
            None => cli.down.clone(),
        };
        let down_pane = if fzf_mode {
            cli.preview.as_ref().map(|pv| {
                let pstate = Arc::new(Mutex::new(StreamState::new()));
                spawn_item_preview(pv.clone(), controls.clone(), pstate.clone());
                (pstate, "preview".to_string())
            })
        } else if down_cmd.is_empty() {
            None
        } else {
            let dstate = Arc::new(Mutex::new(StreamState::new()));
            spawn_preview(
                down_cmd.clone(),
                state.clone(),
                controls.clone(),
                dstate.clone(),
            );
            let label = consumer.clone().unwrap_or_else(|| cli.down.join(" "));
            Some((dstate, label))
        };
        {
            let mut c = controls.lock().unwrap();
            // Flags win; else fall back to the `select` widget's -prompt/-header.
            c.prompt = cli
                .prompt
                .clone()
                .or(sel_prompt.clone())
                .unwrap_or_default();
            c.header = cli
                .header
                .clone()
                .or(sel_header.clone())
                .unwrap_or_default();
            // Form mode: register every control widget (input/filter/facet/
            // slider/check) so keys edit them and `apply`/`where … .name` resolve
            // against their live values. `control_meta` carries slider/facet
            // bounds + the facet cursor, parallel to `inputs`.
            for w in spec.widgets.iter().filter(|w| w.kind.is_control()) {
                // A `sel` widget's value is its highlighted row, exposed under the
                // `.<path>.sel` accessor (SPEC §14) — so a `.ps` selection list
                // registers control `ps.sel`. Every other control is `.<path>`.
                let name = if w.kind == spec::WidgetKind::Sel {
                    format!("{}.sel", w.path.trim_start_matches('.'))
                } else {
                    w.path.trim_start_matches('.').to_string()
                };
                let opt_f =
                    |k: &str, d: f64| w.opts.get(k).map(|s| spec::parse_scalar(s)).unwrap_or(d);
                let kind = tui::control_kind(w.kind);
                let (min, max, step) = (opt_f("min", 0.0), opt_f("max", 100.0), opt_f("step", 1.0));
                // Initial value: slider = min, check = "0", others = "".
                let init = match kind {
                    tui::ControlKind::Slider => format!("{min}"),
                    tui::ControlKind::Check => "0".to_string(),
                    _ => String::new(),
                };
                let opts: Vec<String> = w
                    .opts
                    .get("opts")
                    .map(|s| {
                        s.split(',')
                            .filter(|x| !x.is_empty())
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                c.inputs.push((name, init));
                c.control_meta.push(tui::ControlMeta {
                    kind,
                    min,
                    max,
                    step,
                    opts,
                    cursor: 0,
                });
            }
            // Actor session (§15): spawn every `spawn`/`pool` ref once, held for
            // the session so `tell`/`ask` bind actions can drive them. Names are
            // validated at build, so this only fails on an internal mismatch.
            if !spec.actor_refs.is_empty() {
                match arb::actor::Session::build(&spec.actors, &spec.actor_refs) {
                    Ok(session) => c.session = session,
                    Err(e) => {
                        eprintln!("arb: {e}");
                        std::process::exit(1);
                    }
                }
            }
            // Key bindings (`bind C-<letter> …`) drive the same input values.
            c.binds = spec.binds.clone();
            // Mouse reactions (`bind <Click> …`).
            c.mouse_binds = spec.mouse_binds.clone();
            // fzf-compat: exact/no-sort match modes; `--query` seeds the filter.
            c.exact = cli.exact;
            c.no_sort = cli.no_sort;
            if let Some(q) = &cli.query {
                c.filter = q.clone();
            }
            // `-m`/`--multi` is accepted for compat; arb always allows Tab-marking.
            let _ = cli.multi;
        }
        let outcome = tui::run(
            &spec,
            state,
            controls.clone(),
            down_pane,
            err_pane,
            fzf_mode,
            cli.height.clone(),
        );
        if fzf_mode {
            // On Enter (submit) emit the selection (marked lines, or the cursor
            // line). With a `| CONS` consumer, pipe the selection through it first
            // (`find / | _ | perl -pe …` transforms the picked lines). Abort
            // (Esc/Ctrl-C) exits 130 with no output.
            let c = controls.lock().unwrap();
            if c.submit {
                let out = match &consumer {
                    Some(cons) => run_capture(&["sh".into(), "-c".into(), cons.clone()], &c.result),
                    None => c.result.clone(),
                };
                for line in out {
                    println!("{line}");
                }
            } else {
                std::process::exit(130);
            }
        }
        outcome
    } else if let Some(out_ops) = &spec.out {
        // Piped downstream: arb modifies the stream. A per-line pipeline streams
        // (so `tail -f | arb 'out {…}' | …` works live); reducers/sorts batch.
        if let Some(prod) = &producer {
            // A spec `spawn CMD`/`< file` feeds the stream here (no tty); drain
            // it, then apply `out { … }` — without this the headless path would
            // read an empty stdin and emit nothing.
            read_producer_sync(prod, &state)?;
            emit_out(out_ops, &state, cli.json)
        } else if let Some((cmd, _)) = &poll {
            // Headless `! CMD every Ns`: run CMD once and reshape it (an endless
            // poll into a reducer/emit could never terminate).
            read_producer_sync(cmd, &state)?;
            emit_out(out_ops, &state, cli.json)
        } else if needs_stdin && !cli.json && query::is_line_streamable(out_ops) {
            stream_out(out_ops)
        } else {
            if needs_stdin {
                read_stdin_sync(&state, &prelude);
            }
            emit_out(out_ops, &state, cli.json)
        }
    } else if let Some(prod) = &producer {
        // `spawn CMD`/`< file` with no `out { … }`, piped onward: forward the
        // producer's output through untouched (the passthrough twin).
        stream_producer(prod)
    } else if let Some((cmd, _)) = &poll {
        // Headless `! CMD every Ns` with no `out { … }`: forward one run.
        stream_producer(cmd)
    } else if needs_stdin {
        // A dashboard spec piped onward with no `out { … }` reshape — arb is a
        // passive tap: forward the stream through untouched so the downstream
        // consumer still receives it (`find / | arb dash.arb | stryke`). Only an
        // explicit `out { … }` changes what flows downstream.
        passthrough(&prelude)
    } else {
        dump(&spec, &state)
    }
}

/// POSIX single-quote a string so it survives `sh -c` as one literal argument
/// (used to fold `< FILE` into a `cat -- FILE` producer). Any embedded `'` is
/// closed, escaped, and reopened (`'\''`).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Run `producer` (`sh -c`) and drain its stdout into `state` synchronously,
/// blocking until the child closes stdout. The headless twin of `spawn_producer`
/// (which uses background threads) so `emit_out` sees the full output.
fn read_producer_sync(producer: &str, state: &Arc<Mutex<StreamState>>) -> io::Result<()> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(producer)
        .stdout(Stdio::piped())
        .spawn()?;
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines() {
            match line {
                Ok(l) => state.lock().unwrap().push(l),
                Err(_) => break,
            }
        }
    }
    let _ = child.wait();
    Ok(())
}

/// Run `producer` (`sh -c`) and copy its stdout to arb's stdout line by line,
/// flushing per line. Used for a headless `spawn CMD` with no `out { … }` — arb
/// forwards the spawned stream to the downstream consumer.
fn stream_producer(producer: &str) -> io::Result<()> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(producer)
        .stdout(Stdio::piped())
        .spawn()?;
    if let Some(out) = child.stdout.take() {
        let mut w = io::stdout().lock();
        for line in BufReader::new(out).lines() {
            match line {
                Ok(l) => {
                    if writeln!(w, "{l}").and_then(|()| w.flush()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }
    let _ = child.wait();
    Ok(())
}

/// arb as a transparent tap: copy every stdin line to stdout unchanged, flushing
/// per line so a live upstream (`tail -f`) reaches the downstream consumer
/// promptly. Used when a dashboard spec is piped onward with no `out { … }`.
fn passthrough(prelude: &[String]) -> io::Result<()> {
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    // Replay any sniff-peeked lines first, then the rest of stdin.
    let feed = prelude
        .iter()
        .cloned()
        .map(Ok::<_, io::Error>)
        .chain(stdin.lock().lines());
    for line in feed {
        let l = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if let Err(e) = writeln!(out, "{l}").and_then(|()| out.flush()) {
            return ok_on_broken_pipe(Err(e));
        }
    }
    Ok(())
}

/// Treat a closed downstream pipe (the consumer exited — `| head`, `| stryke`
/// quitting) as clean EOF rather than an error, like any well-behaved Unix
/// filter. Other I/O errors propagate.
fn ok_on_broken_pipe(r: io::Result<()>) -> io::Result<()> {
    match r {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

/// Write the `out { … }` pipeline's result to stdout (arb as a pipe filter).
/// Stream a per-line `out` pipeline: apply it to each line as it arrives and
/// emit results immediately with a flush, so live pipes work without buffering.
fn stream_out(ops: &[QueryOp]) -> io::Result<()> {
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if let QueryResult::Lines(ls) = query::eval(ops, std::slice::from_ref(&line), 0.0) {
            for l in ls {
                if let Err(e) = writeln!(out, "{l}") {
                    return ok_on_broken_pipe(Err(e));
                }
            }
        }
        if let Err(e) = out.flush() {
            return ok_on_broken_pipe(Err(e));
        }
    }
    Ok(())
}

fn emit_out(ops: &[QueryOp], state: &Arc<Mutex<StreamState>>, json: bool) -> io::Result<()> {
    let st = state.lock().unwrap();
    let raw: Vec<String> = st.lines.iter().cloned().collect();
    let elapsed = st.start.elapsed().as_secs_f64();
    let result = query::eval(ops, &raw, elapsed);
    let mut out = io::stdout().lock();
    if json {
        let v = match result {
            QueryResult::Lines(ls) => {
                serde_json::Value::Array(ls.into_iter().map(serde_json::Value::String).collect())
            }
            QueryResult::Scalar(s) => serde_json::Number::from_f64(s)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            QueryResult::Pairs(ps) => {
                let mut m = serde_json::Map::new();
                for (k, c) in ps {
                    m.insert(k, serde_json::Value::Number(c.into()));
                }
                serde_json::Value::Object(m)
            }
        };
        writeln!(out, "{}", serde_json::to_string(&v).unwrap_or_default())?;
        return Ok(());
    }
    match result {
        QueryResult::Lines(ls) => {
            for l in ls {
                writeln!(out, "{l}")?;
            }
        }
        QueryResult::Scalar(v) => writeln!(out, "{v}")?,
        QueryResult::Pairs(ps) => {
            for (k, c) in ps {
                writeln!(out, "{k}\t{c}")?;
            }
        }
    }
    Ok(())
}

/// Validate a spec and copy it into `~/.arb/lib/NAME.arb` so it can later be run
/// with `arb -p NAME` from anywhere.
fn save_preset(name: &str, cli: &Cli) -> io::Result<()> {
    let src = if let Some(e) = &cli.eval {
        e.clone()
    } else if let Some(f) = &cli.spec {
        std::fs::read_to_string(f)?
    } else {
        eprintln!("arb: --save needs a spec — a FILE argument or -e SRC");
        std::process::exit(2);
    };
    if let Err(e) = parser::parse(&src).and_then(|c| spec::build(&c)) {
        eprintln!("arb: --save: invalid spec: {e}");
        std::process::exit(1);
    }
    let dir = spec::lib_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no preset library (set HOME or ARB_LIB)",
        )
    })?;
    match spec::install_preset(&dir, name, &src) {
        Ok(path) => {
            eprintln!("arb: saved preset `{name}` -> {}", path.display());
            Ok(())
        }
        Err(e) => {
            eprintln!("arb: --save: {e}");
            std::process::exit(1);
        }
    }
}

/// `arb --install FILE [--as NAME]`: install a shared spec file into the preset
/// library so it can be run with `arb -p NAME` from anywhere.
fn install_cmd(file: &str, as_name: Option<&str>) -> io::Result<()> {
    let src = std::fs::read_to_string(file)
        .map_err(|e| io::Error::new(e.kind(), format!("{file}: {e}")))?;
    let name = as_name
        .map(str::to_string)
        .or_else(|| {
            std::path::Path::new(file)
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    let Some(dir) = spec::lib_dir() else {
        eprintln!("arb: no preset library (set HOME or ARB_LIB)");
        std::process::exit(1);
    };
    match spec::install_preset(&dir, &name, &src) {
        Ok(path) => {
            eprintln!("arb: installed `{name}` -> {}", path.display());
            Ok(())
        }
        Err(e) => {
            eprintln!("arb: {e}");
            std::process::exit(1);
        }
    }
}

/// Resolve the spec SOURCE for the introspection dumps: `-p NAME` -> `import NAME`,
/// `-e SRC` -> the literal, a `FILE` positional -> its contents. Errors if none is
/// given (a dump has nothing to inspect without a spec).
fn dump_src(cli: &Cli) -> Result<String, String> {
    if let Some(p) = &cli.preset {
        Ok(format!("import {p}"))
    } else if let Some(e) = &cli.eval {
        Ok(e.clone())
    } else if let Some(path) = &cli.spec {
        std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
    } else {
        Err("dump needs a spec — a FILE argument, -e SRC, or -p NAME".into())
    }
}

/// Report a dump's outcome the arb way: on error, `arb: <reason>` to stderr and
/// exit 1; on success, exit 0. The dump-flag twin of the pervasive `arb: {e}`
/// pattern, adapting the `Result<(), String>` dump fns to `main`'s `io::Result`.
fn finish_dump(r: Result<(), String>) -> io::Result<()> {
    if let Err(e) = r {
        eprintln!("arb: {e}");
        std::process::exit(1);
    }
    Ok(())
}

/// `--dump-tokens`: print the Tcl-flavored lexer token stream, one
/// `offset<TAB>Tok` per line (offset = the token's start char position in the
/// source — the same anchor the LSP uses for diagnostics).
fn dump_tokens(cli: &Cli) -> Result<(), String> {
    let src = dump_src(cli)?;
    for (tok, off) in arb::lexer::lex(&src).map_err(|e| e.to_string())? {
        println!("{off}\t{tok:?}");
    }
    Ok(())
}

/// `--dump-ast`: print the parsed command tree (the spec AST — a `Vec<Command>`).
fn dump_ast(cli: &Cli) -> Result<(), String> {
    let src = dump_src(cli)?;
    let cmds = arb::parser::parse(&src).map_err(|e| e.to_string())?;
    println!("{cmds:#?}");
    Ok(())
}

/// `--dump-bytecode`: build the spec and print its compiled query pipelines — the
/// `out { … }` pipeline and each widget's `source` pipeline as their `QueryOp` op
/// vectors. This is arb's compiled form; there is no separate fusevm program (only
/// the expression layer inside `where`/`map` lowers to fusevm), so the op vectors
/// are the bytecode the runtime evaluates. The pretty-printed twin of `--disasm`.
fn dump_bytecode(cli: &Cli) -> Result<(), String> {
    let spec = build_dump_spec(cli)?;
    if let Some(ops) = &spec.out {
        println!("== out ==\n{ops:#?}");
    }
    for w in &spec.widgets {
        match &w.source {
            Some(s) => println!("== {} ({}) ==\n{:#?}", w.path, w.kind.label(), s.pipeline),
            None => println!("== {} ({}) ==\n(no source pipeline)", w.path, w.kind.label()),
        }
    }
    Ok(())
}

/// `--disasm`: build the spec and print a flat, numbered disassembly of every
/// compiled pipeline (`out` + each widget `source`), one `NNNN  Op` per line — the
/// listing view of the same `QueryOp` op vectors `--dump-bytecode` pretty-prints.
fn disasm(cli: &Cli) -> Result<(), String> {
    let spec = build_dump_spec(cli)?;
    if let Some(ops) = &spec.out {
        println!("; arb pipeline — out");
        disasm_ops(ops);
    }
    for w in &spec.widgets {
        println!("; arb pipeline — {} ({})", w.path, w.kind.label());
        match &w.source {
            Some(s) => disasm_ops(&s.pipeline),
            None => println!("  (no source pipeline)"),
        }
    }
    Ok(())
}

/// Parse + build the spec from the resolved dump source (shared by the two
/// bytecode-level dumps), stringifying either stage's error.
fn build_dump_spec(cli: &Cli) -> Result<Spec, String> {
    let src = dump_src(cli)?;
    let cmds = parser::parse(&src).map_err(|e| e.to_string())?;
    spec::build(&cmds).map_err(|e| e.to_string())
}

/// Print one compiled pipeline as a numbered op listing.
fn disasm_ops(ops: &[QueryOp]) {
    for (i, op) in ops.iter().enumerate() {
        println!("{i:04}  {op:?}");
    }
}

fn load_spec(cli: &Cli) -> Result<Spec, String> {
    let src = if let Some(p) = &cli.preset {
        format!("import {p}")
    } else if let Some(e) = &cli.eval {
        e.clone()
    } else if let Some(path) = &cli.spec {
        std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?
    } else {
        // Zero-config default: a select list under `--fzf`, else a stream tail.
        default_spec_src(cli.fzf).to_string()
    };
    // The rkyv script cache skips lex+parse for a previously-seen spec source.
    spec::build(&cache::parse_cached(&src).map_err(String::from)?).map_err(String::from)
}

/// The synthesized spec source when the user gave no spec/`-e`/`-p`. `--fzf`
/// yields a one-widget `select` list (fzf as a DSL spec); otherwise a full-screen
/// tail of stdin. Both bind `in` so the shared stream feeds the widget.
/// How many lines to peek for zero-config sniffing, and how long to wait for
/// them — sniffing must never delay startup or hang on an idle producer.
const SNIFF_LINES: usize = 8;
const SNIFF_DEADLINE_MS: i32 = 150;

/// True if stdin (fd 0) has data ready to read within `timeout_ms` — so peeking
/// never blocks on an idle producer (e.g. `tail -f empty.log | arb`).
fn stdin_ready(timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: single valid pollfd, count 1.
    unsafe { libc::poll(&mut pfd, 1, timeout_ms) > 0 && (pfd.revents & libc::POLLIN) != 0 }
}

/// Peek up to `max` lines from stdin for sniffing, without ever blocking on an
/// idle producer. Consumed lines are returned so the caller replays them into
/// the stream (the rest stay buffered in the global stdin for the feed thread).
fn peek_lines(max: usize, deadline_ms: i32) -> Vec<String> {
    use std::io::BufRead;
    let mut out = Vec::new();
    let stdin = io::stdin();
    let mut lock = stdin.lock();
    for _ in 0..max {
        if !stdin_ready(deadline_ms) {
            break; // no data ready — don't block
        }
        let mut line = String::new();
        match lock.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => out.push(line.trim_end_matches(['\n', '\r']).to_string()),
            Err(_) => break,
        }
    }
    out
}

fn default_spec_src(fzf: bool) -> &'static str {
    if fzf {
        "select .sel\nsource .sel { in }"
    } else {
        "tail .stream\nsource .stream { in }"
    }
}

/// Read stdin into the shared stream for the TUI. When `tee`, each line is also
/// written to stdout immediately (a live passthrough for a downstream consumer),
/// so `find / | arb | consumer` feeds `consumer` continuously while the TUI
/// renders to /dev/tty — arb never blocks the pipeline. A closed consumer stops
/// the passthrough but the TUI keeps updating.
/// Whether a string is an arb pipeline: it has a bare `_` stage (the marker for
/// "arb goes here"), e.g. `sudo find / | _ | perl -pe '…'`.
fn is_arb_pipeline(s: &str) -> bool {
    s.split('|').any(|seg| seg.trim() == "_")
}

/// Split an arb pipeline on the `_` stage marker into (producer, consumer).
/// `find / | _ | grep x` -> ("find /", Some("grep x")); no `_` marker -> the
/// whole string is the producer. (Segments are shelled out per-stage; a zshrs
/// lexer can replace that later without changing arb's fd orchestration.)
fn parse_pipeline(s: &str) -> (String, Option<String>) {
    let segs: Vec<&str> = s.split('|').map(str::trim).collect();
    match segs.iter().position(|seg| *seg == "_") {
        Some(p) => {
            let producer = segs[..p].join(" | ");
            let consumer = segs[p + 1..].join(" | ");
            (
                producer,
                if consumer.is_empty() {
                    None
                } else {
                    Some(consumer)
                },
            )
        }
        None => (s.trim().to_string(), None),
    }
}

/// Drain any reader (a spawned command's stdout or stderr) line-by-line into a
/// `StreamState` — used to feed arb's stream from the producer's stdout and the
/// error pane from its stderr.
fn spawn_source_reader<R: io::Read + Send + 'static>(reader: R, state: Arc<Mutex<StreamState>>) {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(l) => state.lock().unwrap().push(l),
                Err(_) => break,
            }
        }
    });
}

/// Spawn the producer command (`sh -c`) with arb owning its fds: stdout streams
/// into `state`, stderr into a fresh `StreamState` returned for the error pane.
/// A background thread `wait()`s on the child so it is reaped (not zombied) when
/// it exits; it otherwise runs on its own until arb exits.
fn spawn_producer(
    producer: &str,
    state: Arc<Mutex<StreamState>>,
) -> io::Result<Arc<Mutex<StreamState>>> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(producer)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("no stdout"))?;
    let err = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("no stderr"))?;
    // Reap the child when it exits (a bare `drop(child)` leaves a zombie until
    // arb itself exits — the doc's "killed when arb exits" was inaccurate).
    thread::spawn(move || {
        let _ = child.wait();
    });
    spawn_source_reader(out, state);
    let err_state = Arc::new(Mutex::new(StreamState::new()));
    spawn_source_reader(err, err_state.clone());
    Ok(err_state)
}

/// Background poll source (`! CMD every Ns`, §7): re-run CMD via `sh -c` every
/// `interval`, draining each run's stdout into `state` and stderr into the
/// returned error-pane state. Per-cycle reader threads join before the sleep so
/// a chatty command's stderr can't deadlock on a full pipe buffer. Loops until
/// arb exits; the one-shot headless twin is `read_producer_sync`.
fn poll_producer(
    cmd: &str,
    interval: Duration,
    state: Arc<Mutex<StreamState>>,
) -> Arc<Mutex<StreamState>> {
    let err_state = Arc::new(Mutex::new(StreamState::new()));
    let (cmd, es) = (cmd.to_string(), err_state.clone());
    thread::spawn(move || loop {
        if let Ok(mut child) = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            let (st, et) = (state.clone(), es.clone());
            let ho = child.stdout.take().map(|o| {
                thread::spawn(move || {
                    for l in BufReader::new(o).lines().map_while(Result::ok) {
                        st.lock().unwrap().push(l);
                    }
                })
            });
            let he = child.stderr.take().map(|e| {
                thread::spawn(move || {
                    for l in BufReader::new(e).lines().map_while(Result::ok) {
                        et.lock().unwrap().push(l);
                    }
                })
            });
            if let Some(h) = ho {
                let _ = h.join();
            }
            if let Some(h) = he {
                let _ = h.join();
            }
            let _ = child.wait();
        }
        thread::sleep(interval);
    });
    err_state
}

fn spawn_reader(
    state: Arc<Mutex<StreamState>>,
    tee: bool,
    controls: Arc<Mutex<tui::Controls>>,
    out_ops: Option<Vec<QueryOp>>,
    prelude: Vec<String>,
) {
    thread::spawn(move || {
        // Only hold the stdout lock when actually teeing. Otherwise (e.g. --fzf,
        // which emits its selection from `main` after the TUI exits) this thread
        // would keep stdout locked for its whole life and deadlock `main`'s final
        // `println!` — the process would never terminate after selection.
        let mut out = if tee { Some(io::stdout().lock()) } else { None };
        let mut downstream_open = tee;
        // Megafilter/map cache: re-resolve the `out` pipeline only when the live
        // `input` values change (resolving parses the input text — cheap, but not
        // worth doing per line on a fast stream). `resolved` maps each line; it is
        // only used per-line when line-streamable (a reducer like `count` can't
        // map a single line, so we fall back to passthrough for it).
        let map = out_ops.is_some();
        let mut last_inputs: Option<Vec<(String, String)>> = None;
        let mut resolved: Vec<QueryOp> = Vec::new();
        let mut resolved_ok = false;
        // Replay any sniff-peeked lines first, then the rest of stdin.
        let feed = prelude
            .into_iter()
            .map(Ok::<_, io::Error>)
            .chain(io::stdin().lock().lines());
        for line in feed {
            let l = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if let Some(o) = out.as_mut() {
                // One lock: snapshot the live filter and input values together.
                let (filter, inputs) = {
                    let c = controls.lock().unwrap();
                    (c.filter.clone(), c.inputs.clone())
                };
                // Megafilter: only lines matching the live filter flow downstream.
                if tui::filter_matches(&l, &filter) && downstream_open {
                    if map {
                        if last_inputs.as_ref() != Some(&inputs) {
                            let imap: std::collections::HashMap<String, String> =
                                inputs.iter().cloned().collect();
                            resolved =
                                spec::resolve_pipeline(out_ops.as_deref().unwrap_or(&[]), &imap);
                            resolved_ok = query::is_line_streamable(&resolved);
                            last_inputs = Some(inputs);
                        }
                        // Map the line through the resolved pipeline (identity when
                        // it resolved to nothing); non-streamable → raw passthrough.
                        let outs = if resolved_ok {
                            tui::project_line(&resolved, &l)
                        } else {
                            vec![l.clone()]
                        };
                        for ol in outs {
                            if writeln!(o, "{ol}").and_then(|()| o.flush()).is_err() {
                                downstream_open = false;
                                break;
                            }
                        }
                    } else if writeln!(o, "{l}").and_then(|()| o.flush()).is_err() {
                        downstream_open = false;
                    }
                }
            }
            state.lock().unwrap().push(l);
        }
    });
}

/// `arb -- CMD` preview: re-run the command chain over arb's CURRENT post-filter
/// output whenever the filter or the stream changes, capturing its stdout+stderr
/// into `dstate` for the pane. The chain always sees the filtered lines, and the
/// pane stays in sync with the filter — no stale pre-filter accumulation, and the
/// command's output never touches the terminal.
fn spawn_preview(
    cmd: Vec<String>,
    state: Arc<Mutex<StreamState>>,
    controls: Arc<Mutex<tui::Controls>>,
    dstate: Arc<Mutex<StreamState>>,
) {
    thread::spawn(move || {
        let mut last: Option<(String, u64)> = None;
        loop {
            thread::sleep(Duration::from_millis(250));
            let (filter, quit) = {
                let c = controls.lock().unwrap();
                (c.filter.clone(), c.quit)
            };
            if quit {
                break;
            }
            // Re-run only when the filter or the line count changed.
            let key = (filter.clone(), state.lock().unwrap().total);
            if last.as_ref() == Some(&key) {
                continue;
            }
            last = Some(key);
            let input: Vec<String> = {
                let s = state.lock().unwrap();
                s.lines
                    .iter()
                    .filter(|l| tui::filter_matches(l, &filter))
                    .cloned()
                    .collect()
            };
            let output = run_capture(&cmd, &input);
            let mut d = dstate.lock().unwrap();
            *d = StreamState::new();
            for l in output {
                d.push(l);
            }
        }
    });
}

/// Single-quote a string for safe `sh -c` substitution of `{}` (a line may hold
/// spaces or shell metacharacters — e.g. a path). `'` -> `'\''`.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// fzf `--preview`: run `template` (with `{}` replaced by the line under the
/// cursor) whenever the cursor moves, capturing its output into `pstate` for the
/// preview pane. Debounced; skips re-running on an unchanged line.
fn spawn_item_preview(
    template: String,
    controls: Arc<Mutex<tui::Controls>>,
    pstate: Arc<Mutex<StreamState>>,
) {
    thread::spawn(move || {
        let mut last = String::from("\u{0}");
        loop {
            thread::sleep(Duration::from_millis(120));
            let (cur, quit) = {
                let c = controls.lock().unwrap();
                (c.current.clone(), c.quit)
            };
            if quit {
                break;
            }
            if cur == last {
                continue;
            }
            last = cur.clone();
            if cur.is_empty() {
                continue;
            }
            let cmd = template.replace("{}", &shell_escape(&cur));
            let output = run_capture(&["sh".to_string(), "-c".to_string(), cmd], &[]);
            let mut p = pstate.lock().unwrap();
            *p = StreamState::new();
            for l in output {
                p.push(l);
            }
        }
    });
}

/// Run `cmd`, feeding `input` on its stdin, and collect its stdout then stderr as
/// lines. A spawn/exec error becomes a single diagnostic line so it shows in the
/// pane rather than crashing arb.
fn run_capture(cmd: &[String], input: &[String]) -> Vec<String> {
    let child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return vec![format!("arb: {}: {e}", cmd[0])],
    };
    if let Some(mut si) = child.stdin.take() {
        for l in input {
            if writeln!(si, "{l}").is_err() {
                break; // consumer closed its stdin early (e.g. `head`)
            }
        }
    }
    match child.wait_with_output() {
        Ok(o) => {
            let mut v: Vec<String> = String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(String::from)
                .collect();
            v.extend(String::from_utf8_lossy(&o.stderr).lines().map(String::from));
            v
        }
        Err(e) => vec![format!("arb: {}: {e}", cmd[0])],
    }
}

fn read_stdin_sync(state: &Arc<Mutex<StreamState>>, prelude: &[String]) {
    for l in prelude {
        state.lock().unwrap().push(l.clone());
    }
    for line in io::stdin().lock().lines() {
        match line {
            Ok(l) => state.lock().unwrap().push(l),
            Err(_) => break,
        }
    }
}

fn dump(spec: &Spec, state: &Arc<Mutex<StreamState>>) -> io::Result<()> {
    let st = state.lock().unwrap();
    let raw: Vec<String> = st.lines.iter().cloned().collect();
    let elapsed = st.start.elapsed().as_secs_f64();
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "arb: spec — {} widget(s) (no terminal; render skipped)",
        spec.widgets.len()
    )?;
    for w in &spec.widgets {
        let (src, res) = match &w.source {
            Some(s) => {
                let r = match query::eval(&s.pipeline, &raw, elapsed) {
                    QueryResult::Scalar(v) => format!("= {v}"),
                    QueryResult::Lines(ls) => format!("-> {} line(s)", ls.len()),
                    QueryResult::Pairs(p) => format!("-> {} group(s)", p.len()),
                };
                (format!("stdin[{} op]", s.pipeline.len()), r)
            }
            None => ("-".to_string(), String::new()),
        };
        writeln!(
            out,
            "  {:<10} {:<6} source={:<12} {:<16} opts={:?}",
            w.path,
            w.kind.label(),
            src,
            res,
            w.opts
        )?;
    }
    writeln!(out, "stream: {} lines", st.total)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::fzf_compat_args;

    fn run(args: &[&str]) -> Vec<String> {
        fzf_compat_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn strips_cosmetic_fzf_flags_keeps_honored() {
        let out = run(&[
            "arb",
            "--fzf",
            "--ansi",
            "--border",
            "--reverse",
            "--preview-window",
            "down:3",
            "--min-height",
            "15",
            "--no-sort",
            "--query",
            "foo",
        ]);
        // Cosmetic bool + value flags gone; honored ones remain.
        assert!(!out
            .iter()
            .any(|a| a == "--ansi" || a == "--border" || a == "--reverse"));
        assert!(!out.iter().any(|a| a == "--preview-window" || a == "down:3"));
        assert!(!out.iter().any(|a| a == "--min-height" || a == "15"));
        assert!(out.iter().any(|a| a == "--no-sort"));
        assert_eq!(
            out.iter().position(|a| a == "--query").map(|i| &out[i + 1]),
            Some(&"foo".to_string())
        );
        assert!(out.iter().any(|a| a == "--fzf"));
    }

    #[test]
    fn translates_fzf_plus_and_exact_shorthands() {
        // `+m` drops (arb is single-select by default); `+s` -> --no-sort.
        let out = run(&["arb", "--fzf", "+m", "+s", "-e"]);
        assert!(!out.iter().any(|a| a == "+m" || a == "+s"));
        assert!(out.iter().any(|a| a == "--no-sort"));
        assert!(out.iter().any(|a| a == "--exact"));
        // Outside fzf mode, `-e` is left alone (it's arb's --eval).
        let out2 = run(&["arb", "-e", "gauge .g"]);
        assert!(out2.iter().any(|a| a == "-e"));
        assert!(!out2.iter().any(|a| a == "--exact"));
    }

    #[test]
    fn drops_fzf_short_value_flags() {
        // `-n2..,..` (attached) and `-n 2..` (separate) and `-d:` are dropped.
        let out = run(&["arb", "--fzf", "-n2..,..", "-d", ":", "--query", "q"]);
        assert!(!out
            .iter()
            .any(|a| a.starts_with("-n") || a.starts_with("-d")));
        assert!(!out.iter().any(|a| a == "2.." || a == ":"));
        assert_eq!(
            out.iter().position(|a| a == "--query").map(|i| &out[i + 1]),
            Some(&"q".to_string())
        );
    }

    #[test]
    fn preview_window_equals_form_drops_cleanly() {
        // `--flag=val` carries its value, so nothing after it should be consumed.
        let out = run(&["arb", "--fzf", "--preview-window=right:50%", "--query", "q"]);
        assert!(!out.iter().any(|a| a.starts_with("--preview-window")));
        assert_eq!(
            out.iter().position(|a| a == "--query").map(|i| &out[i + 1]),
            Some(&"q".to_string())
        );
    }
}
