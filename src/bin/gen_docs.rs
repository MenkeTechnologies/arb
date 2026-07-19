//! Offline generator for `docs/reference.html` — the arb language reference,
//! rendered with the same cyberpunk HUD chrome as `docs/index.html`. Run before
//! publishing GitHub Pages:
//!
//! ```sh
//! cargo run --bin gen-docs
//! ```
//!
//! Source of truth: the LSP corpus in `arb::lsp` (`corpus()`), the exact
//! `(name, chapter, doc)` table the editor hover path renders from. The static
//! page and the language server therefore never drift — a construct is
//! documented here only if the runtime actually implements it (widgets from
//! `spec::WidgetKind`, query verbs from `build_query`, directives/actions from
//! the spec parser, input modes from the pipeline `in` markers).

use std::fmt::Write as _;

/// Display order of the reference chapters (the corpus is grouped by these but
/// stored in a different order; this puts the headline widgets first).
const CHAPTER_ORDER: &[&str] = &["Widget", "Query", "Directive", "Action", "Input"];

fn main() {
    let corpus = arb::lsp::corpus();
    // Presets are a filesystem catalog (stdlib/*.arb), enumerated at generation
    // time so the reference never goes stale when a preset is added or removed.
    let presets = read_presets();

    let page = format!("{HEAD}{}{FOOT}", build_body(corpus, &presets))
        // Stamp the current crate version so the page never falls behind
        // Cargo.toml (the meta version-sync gate compares docs/*.html to it).
        .replace("__ARB_VERSION__", env!("CARGO_PKG_VERSION"));

    let out = "docs/reference.html";
    if let Err(e) = std::fs::write(out, page) {
        eprintln!("gen-docs: cannot write {out}: {e}");
        std::process::exit(1);
    }
    println!(
        "wrote {out} ({} constructs + {} presets, {} sections)",
        corpus.len(),
        presets.len(),
        CHAPTER_ORDER.len() + 1,
    );
}

/// Read the stdlib preset catalog: for each `stdlib/*.arb`, the file stem, the
/// description from its leading `# <name> — <desc>` header comment, and a usage
/// example from the second `#` comment line (`cmd | arb -p name`). Sorted by name.
fn read_presets() -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let dir = match std::fs::read_dir("stdlib") {
        Ok(d) => d,
        Err(e) => {
            eprintln!("gen-docs: cannot read stdlib/: {e}");
            std::process::exit(1);
        }
    };
    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("arb") {
            continue;
        }
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        let mut lines = body.lines();
        let first = lines.next().unwrap_or("").trim_start_matches('#').trim();
        // Header form is `name — description`; keep only the description.
        let desc = first
            .split_once(" — ")
            .or_else(|| first.split_once(" - "))
            .map(|(_, d)| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| format!("prebuilt dashboard preset for {name}"));
        // Second comment line is the invocation example, if present.
        let example = lines
            .next()
            .map(|l| l.trim_start_matches('#').trim())
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("… | arb -p {name}"));
        out.push((name, desc, example));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Render one `<section>` per chapter, each an `<article class="doc-entry">` per
/// construct: name heading, description, and a usage example — plus a final
/// Presets section built the same way from the stdlib catalog.
fn build_body(corpus: &[(&str, &str, &str, &str)], presets: &[(String, String, String)]) -> String {
    let mut out = String::new();

    for chapter in CHAPTER_ORDER {
        let rows: Vec<_> = corpus.iter().filter(|(_, c, _, _)| c == chapter).collect();
        if rows.is_empty() {
            continue;
        }
        // The `id="ch-…"` marks this as a real reference chapter. The
        // reference-PDF pipeline keeps id-carrying sections and drops the
        // id-less ones (page chrome / link lists), so every chapter needs it.
        let _ = write!(
            out,
            "\n      <section class=\"tutorial-section\" id=\"ch-{slug}\">\n\
             \x20       <h2>{title}</h2>\n",
            slug = slugify(chapter_title(chapter)),
            title = chapter_title(chapter),
        );
        for (i, (name, _, doc, example)) in rows.iter().enumerate() {
            emit_entry(
                &mut out,
                &format!("doc-{}-{}", slugify(chapter_title(chapter)), i + 1),
                name,
                doc,
                example,
            );
        }
        out.push_str("      </section>\n");
    }

    // Presets chapter — the shipped stdlib dashboards you `import` by name.
    if !presets.is_empty() {
        out.push_str(
            "\n      <section class=\"tutorial-section\" id=\"ch-presets\">\n\
             \x20       <h2>Stdlib presets</h2>\n\
             \x20       <p class=\"tutorial-note\">Prebuilt dashboards shipped with arb; drop one in with <code>import NAME</code> or auto-selected by stream sniffing.</p>\n",
        );
        for (i, (name, desc, example)) in presets.iter().enumerate() {
            emit_entry(
                &mut out,
                &format!("doc-preset-{}", i + 1),
                name,
                desc,
                example,
            );
        }
        out.push_str("      </section>\n");
    }

    out
}

/// Emit one `<article class="doc-entry">`: anchored name heading, description,
/// and a `lang-arb` usage example.
fn emit_entry(out: &mut String, anchor: &str, name: &str, doc: &str, example: &str) {
    let _ = write!(
        out,
        "        <article class=\"doc-entry\" id=\"{anchor}\">\n\
         \x20         <h3><a class=\"doc-anchor\" href=\"#{anchor}\">#</a> <code>{name}</code></h3>\n\
         \x20         <p>{doc}</p>\n\
         \x20         <pre><code class=\"lang-arb\">{example}</code></pre>\n\
         \x20       </article>\n",
        anchor = anchor,
        name = html_escape(name),
        doc = html_escape(doc),
        example = html_escape(example),
    );
}

/// A readable section heading for each corpus chapter key.
fn chapter_title(chapter: &str) -> &'static str {
    match chapter {
        "Widget" => "Widgets",
        "Query" => "Query verbs",
        "Directive" => "Directives",
        "Action" => "Bind / expect actions",
        "Input" => "Input modes",
        _ => "Reference",
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Lowercase, non-alphanumeric runs collapsed to a single `-`, edges trimmed —
/// e.g. `Query verbs` -> `query-verbs`. Used for the `id="ch-…"` anchors that
/// the reference-PDF pipeline keys on.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

const HEAD: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="color-scheme" content="dark light">
  <meta name="description" content="arb — language reference. Every widget, query verb, directive, action, and input mode the current arb build implements. MIT licensed.">
  <title>arb — Language Reference</title>
  <link rel="preconnect" href="https://fonts.googleapis.com">
  <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
  <link href="https://fonts.googleapis.com/css2?family=Orbitron:wght@400;600;700;900&amp;family=Share+Tech+Mono&amp;display=swap" rel="stylesheet">
  <link rel="stylesheet" href="hud-static.css">
  <link rel="stylesheet" href="tutorial.css">
  <style>
    .tutorial-main { max-width: 68rem; }
    .file-table { width:100%;border-collapse:collapse;margin:0.6rem 0;font-size:12px; }
    .file-table th { background:var(--bg-secondary);color:var(--cyan);font-family:'Orbitron',sans-serif;font-size:10px;font-weight:700;letter-spacing:1.2px;text-transform:uppercase;text-align:left;padding:7px 10px;border:1px solid var(--border); }
    .file-table td { padding:6px 10px;border:1px solid var(--border);color:var(--text-dim);vertical-align:middle; }
    .file-table tr:hover td { background:var(--bg-hover); }
    .file-table td:first-child { font-family:'Share Tech Mono',monospace;color:var(--accent-light);font-weight:600;white-space:nowrap; }
    .file-table code { font-size:11px;color:var(--accent-light);background:var(--bg-primary);padding:1px 4px;border-radius:2px; }
    .stat-grid { display:grid;grid-template-columns:repeat(auto-fill,minmax(14rem,1fr));gap:0.75rem;margin:1.2rem 0; }
    .stat-card { border:1px solid var(--border);border-top:3px solid var(--cyan);background:var(--bg-card);padding:1rem 1.2rem;border-radius:2px;text-align:center; }
    .stat-card .stat-val { font-family:'Orbitron',sans-serif;font-size:28px;font-weight:900;color:var(--cyan);line-height:1.1;text-shadow:0 0 20px var(--cyan-glow); }
    .stat-card .stat-val.accent { color:var(--accent);text-shadow:0 0 20px var(--accent-glow); }
    .stat-card .stat-label { font-family:'Orbitron',sans-serif;font-size:9px;font-weight:700;letter-spacing:2px;text-transform:uppercase;color:var(--text-muted);margin-top:0.5rem; }
    .hub-scheme-strip { border-bottom:1px dashed var(--border);background:color-mix(in srgb, var(--bg-secondary) 85%, transparent);padding:0.55rem 1.5rem 0.65rem;position:relative; }
    .hub-scheme-strip-inner { max-width:68rem;margin:0 auto;display:flex;align-items:center;gap:0.85rem; }
    .hub-scheme-strip .hud-scheme-label { flex:0 0 auto;font-family:'Orbitron',sans-serif;font-size:9px;font-weight:700;letter-spacing:2px;text-transform:uppercase;color:var(--accent);text-align:left; }
    .hub-scheme-strip .scheme-grid { flex:1 1 auto;display:grid;grid-template-columns:repeat(5,minmax(0,1fr));gap:6px; }
    @media (max-width:720px){ .hub-scheme-strip-inner{flex-direction:column;align-items:stretch}.hub-scheme-strip .scheme-grid{grid-template-columns:repeat(2,minmax(0,1fr))} }
    .docs-build-line { margin:0.35rem 0 0;font-family:'Share Tech Mono',ui-monospace,monospace;font-size:11px;color:var(--text-dim);letter-spacing:0.03em;max-width:46rem;opacity:0.75; }
  </style>
</head>
<body>
  <div class="app tutorial-app" id="docsApp">
    <div class="crt-scanline" id="crtH" aria-hidden="true"></div>
    <div class="crt-scanline-v" id="crtV" aria-hidden="true"></div>

    <header class="tutorial-header">
      <div class="tutorial-header-inner">
        <div>
          <h1 class="tutorial-brand">// ARB — LANGUAGE REFERENCE</h1>
          <nav class="tutorial-crumbs" aria-label="Breadcrumb">
            <a href="index.html">Docs</a>
            <span class="sep">/</span>
            <span class="current">Language Reference</span>
            <span class="sep">/</span>
            <a href="https://github.com/MenkeTechnologies/arb" target="_blank" rel="noopener noreferrer">GitHub</a>
          </nav>
          <p class="docs-build-line">arb v__ARB_VERSION__ · pipe → dynamic TUI/web · original language on fusevm + Cranelift JIT · jq/xpath/css/yq superset · Tcl/Tk-flavored DSL · MIT · in active development</p>
        </div>
        <div class="tutorial-toolbar">
          <button type="button" class="btn btn-secondary" id="btnTheme" title="Toggle light/dark">Theme</button>
          <button type="button" class="btn btn-secondary active" id="btnCrt" title="CRT scanline overlay">CRT</button>
          <button type="button" class="btn btn-secondary active" id="btnNeon" title="Neon border pulse">Neon</button>
          <a class="btn btn-secondary" href="index.html">Docs</a>
          <a class="btn btn-secondary" href="report.html">Report</a>
          <a class="btn btn-secondary" href="https://github.com/MenkeTechnologies/arb" target="_blank" rel="noopener noreferrer">GitHub</a>
        </div>
      </div>
    </header>

    <div class="hub-scheme-strip">
      <div class="hub-scheme-strip-inner">
        <span class="hud-scheme-label">// Color scheme</span>
        <div class="scheme-grid" id="hudSchemeGrid"></div>
      </div>
    </div>

    <main class="tutorial-main">
      <h2 class="tutorial-title"><span class="step-hash">&gt;_</span>LANGUAGE REFERENCE</h2>
      <p class="tutorial-subtitle">Every widget, query verb, directive, action, and input mode the current arb build implements. This page is generated from the language-server corpus (<code>src/lsp.rs</code>) by the <code>gen-docs</code> binary, so it stays in sync with what the runtime and editor tooling actually know about. The full grammar is in <a href="https://github.com/MenkeTechnologies/arb/blob/main/SPEC.md">SPEC.md</a>.</p>
"#;

const FOOT: &str = r#"
      <section class="tutorial-section">
        <h2>More</h2>
        <ul>
          <li><strong>Docs</strong> — <a href="index.html">index.html</a> (overview, architecture, examples)</li>
          <li><strong>Engineering report</strong> — <a href="report.html">report.html</a> (architecture, positioning, milestones)</li>
          <li><strong>Language spec</strong> — <a href="https://github.com/MenkeTechnologies/arb/blob/main/SPEC.md">SPEC.md</a> (full grammar)</li>
          <li><strong>Source</strong> — <a href="https://github.com/MenkeTechnologies/arb">github.com/MenkeTechnologies/arb</a></li>
        </ul>
      </section>
    </main>

  </div>

  <script src="hud-theme.js"></script>
</body>
</html>
"#;
