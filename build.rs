//! Bundle the vendored `zgui-core` web toolkit (git submodule at `lib/zgui-core`)
//! into two single assets — `zgui_bundle.js` and `zgui_bundle.css` — that
//! `src/serve.rs` embeds via `include_str!` and inlines into the served
//! dashboard page. This keeps the `arb` binary self-contained (no runtime file
//! dependency) while treating the submodule as the single source of truth (never
//! a copy in the tree). If the submodule is not checked out, empty bundles are
//! emitted so a plain `cargo build` still succeeds (the web dashboard degrades).

use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let webui = Path::new("lib/zgui-core/webui");
    println!("cargo:rerun-if-changed=lib/zgui-core/webui");
    println!("cargo:rerun-if-changed=build.rs");

    let (js, css) = if webui.is_dir() {
        (bundle_js(webui), bundle_css(webui))
    } else {
        println!(
            "cargo:warning=lib/zgui-core not checked out — web dashboard will be minimal. \
             Run: git submodule update --init"
        );
        (String::new(), String::new())
    };
    fs::write(out.join("zgui_bundle.js"), js).unwrap();
    fs::write(out.join("zgui_bundle.css"), css).unwrap();
}

/// Concatenate every `webui/*.js` IIFE into one script. `util.js` and the shared
/// dependencies load first (per zgui-core CONSUMERS.md §1), then the rest
/// alphabetically. Files are joined with `\n;\n` — without the separator two
/// adjacent `(function(){…})()` IIFEs would parse as a call
/// `(…)()( …)()`, breaking every component.
fn bundle_js(webui: &Path) -> String {
    // Load-order-sensitive prefix: util provides escapeHtml; the rest are the
    // siblings other components resolve at construction time.
    const FIRST: &[&str] = &[
        "util.js",
        "fzf.js",
        "icons.js",
        "viz.js",
        "table.js",
        "popover.js",
        "context-menu.js",
        "menu.js",
        "splash.js",
        "colorscheme.js",
        "crt.js",
    ];
    let mut names: Vec<String> = fs::read_dir(webui)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".js"))
        .collect();
    names.sort();
    // Stable order: FIRST (in listed order, if present) then the remaining files.
    let mut order: Vec<String> = FIRST.iter().map(|s| s.to_string()).filter(|n| names.contains(n)).collect();
    for n in &names {
        if !order.contains(n) {
            order.push(n.clone());
        }
    }
    let mut out = String::new();
    for n in order {
        if let Ok(src) = fs::read_to_string(webui.join(&n)) {
            out.push_str("\n;\n/* ");
            out.push_str(&n);
            out.push_str(" */\n");
            out.push_str(&src);
        }
    }
    out
}

/// Inline `all.css` by resolving its `@import url("./x.css")` list in order —
/// the relative imports can't resolve inside an inlined `<style>`, so we splice
/// each referenced file in their place (cascade order preserved).
fn bundle_css(webui: &Path) -> String {
    let all = match fs::read_to_string(webui.join("all.css")) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    let mut out = String::new();
    for line in all.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("@import url(\"./") {
            if let Some(name) = rest.strip_suffix("\");") {
                if let Ok(css) = fs::read_to_string(webui.join(name)) {
                    out.push_str("\n/* ");
                    out.push_str(name);
                    out.push_str(" */\n");
                    out.push_str(&css);
                    continue;
                }
            }
        }
        // Keep any non-@import content (all.css is mostly imports + a header).
        if !t.starts_with("@import") {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}
