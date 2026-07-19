//! Package registry client (`arb install|search|update|publish|uninstall`) — a
//! std-only, no-async, no-TLS client over a **git index + GitHub repos** model
//! (SPEC §18, like the `stryke-*` family). All network I/O goes through the
//! `git` subprocess (the honest std-only transport — `std` has no TLS client).
//!
//! `arb update` clones/pulls the index repo into `~/.arb/registry`; `arb search`
//! greps its `index.json`; `arb install NAME` looks the name up and
//! `git clone`s the package repo into `~/.arb/pkg/NAME`, validating its
//! `arb.toml` + entry module before keeping it. The module resolver
//! (`spec::resolve_module`) reads `~/.arb/pkg` as the SPEC §17 `pkg` tier.
//!
//! `arb publish [REPO_URL]` validates the package, then registers it for real:
//! it fast-forward-pulls the index clone, upserts the package's
//! `{repo, version, desc}` entry into `index.json`, commits, and pushes to the
//! index remote (the same `git` transport). With write access the entry lands
//! directly; without it the commit stays local and arb prints the fork+PR flow —
//! it never falsely claims a push succeeded.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// `~/.arb/pkg` (or `$ARB_PKG`): where installed script packages live.
pub fn pkg_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("ARB_PKG") {
        return Some(PathBuf::from(d));
    }
    std::env::var_os("HOME").map(|h| Path::new(&h).join(".arb/pkg"))
}

/// `~/.arb/registry` (or `$ARB_REGISTRY`): the local clone of the index repo.
pub fn registry_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("ARB_REGISTRY") {
        return Some(PathBuf::from(d));
    }
    std::env::var_os("HOME").map(|h| Path::new(&h).join(".arb/registry"))
}

/// The index repo URL (`$ARB_REGISTRY_URL` or the default community registry).
pub fn registry_url() -> String {
    std::env::var("ARB_REGISTRY_URL")
        .unwrap_or_else(|_| "https://github.com/MenkeTechnologies/arb-registry".to_string())
}

/// One registry entry: where to fetch a package and what it is.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexEntry {
    pub repo: String,
    pub version: String,
    pub desc: String,
}

pub type Index = BTreeMap<String, IndexEntry>;

/// Parse `index.json` — `{ "NAME": { "repo", "version", "desc" }, … }`.
pub fn parse_index(s: &str) -> Result<Index, String> {
    let v: serde_json::Value =
        serde_json::from_str(s).map_err(|e| format!("registry index: {e}"))?;
    let obj = v
        .as_object()
        .ok_or("registry index: expected a JSON object")?;
    let mut idx = Index::new();
    for (name, entry) in obj {
        let get = |k: &str| {
            entry
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        idx.insert(
            name.clone(),
            IndexEntry {
                repo: get("repo"),
                version: get("version"),
                desc: get("desc"),
            },
        );
    }
    Ok(idx)
}

/// Case-insensitive substring search over package name + description.
pub fn search_index<'a>(idx: &'a Index, q: &str) -> Vec<(&'a String, &'a IndexEntry)> {
    let q = q.to_lowercase();
    idx.iter()
        .filter(|(name, e)| name.to_lowercase().contains(&q) || e.desc.to_lowercase().contains(&q))
        .collect()
}

/// A parsed `arb.toml` package manifest (SPEC §18).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub license: String,
    pub desc: String,
    pub deps: BTreeMap<String, String>,
    pub modules: Vec<String>,
    pub native_widgets: Vec<String>,
    pub native_formats: Vec<String>,
}

/// Parse an `arb.toml` manifest via `toml::Value` (no derive).
pub fn parse_manifest(s: &str) -> Result<Manifest, String> {
    let v: toml::Value = s.parse().map_err(|e| format!("arb.toml: {e}"))?;
    let pkg = v.get("package").ok_or("arb.toml: missing [package]")?;
    let str_of =
        |t: &toml::Value, k: &str| t.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let list_of = |t: Option<&toml::Value>, k: &str| -> Vec<String> {
        t.and_then(|x| x.get(k))
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut deps = BTreeMap::new();
    if let Some(d) = v.get("deps").and_then(|x| x.as_table()) {
        for (k, val) in d {
            deps.insert(k.clone(), val.as_str().unwrap_or("").to_string());
        }
    }
    let exports = v.get("exports");
    let native = exports.and_then(|e| e.get("native"));
    Ok(Manifest {
        name: str_of(pkg, "name"),
        version: str_of(pkg, "version"),
        license: str_of(pkg, "license"),
        desc: str_of(pkg, "description"),
        deps,
        modules: list_of(exports, "modules"),
        native_widgets: list_of(native, "widgets"),
        native_formats: list_of(native, "formats"),
    })
}

/// Resolve an imported module name to a package's `.arb` source under `pkg_root`.
/// `NAME` -> `NAME/NAME.arb`, then `NAME/main.arb`, then the manifest's first
/// exported module; `NAME/sub` -> `NAME/sub.arb`. Explicit dir arg = unit-testable.
pub fn read_pkg_module(pkg_root: &Path, name: &str) -> Option<String> {
    if let Some((pkg, sub)) = name.split_once('/') {
        return std::fs::read_to_string(pkg_root.join(pkg).join(format!("{sub}.arb"))).ok();
    }
    let root = pkg_root.join(name);
    for candidate in [format!("{name}.arb"), "main.arb".to_string()] {
        if let Ok(s) = std::fs::read_to_string(root.join(&candidate)) {
            return Some(s);
        }
    }
    // Fall back to the manifest's first exported module.
    let manifest = std::fs::read_to_string(root.join("arb.toml")).ok()?;
    let m = parse_manifest(&manifest).ok()?;
    let first = m.modules.first()?;
    std::fs::read_to_string(root.join(format!("{first}.arb"))).ok()
}

/// Run `git` with `args` (optionally in `cwd`); non-zero exit -> Err(stderr).
fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("git: {e} (is git installed?)"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Validate an installed package dir: `arb.toml` parses and its entry module
/// builds. Returns the manifest.
fn validate_package(dir: &Path, name: &str) -> Result<Manifest, String> {
    let manifest_src = std::fs::read_to_string(dir.join("arb.toml"))
        .map_err(|_| format!("package `{name}`: missing arb.toml"))?;
    let manifest = parse_manifest(&manifest_src)?;
    // Native/cdylib packages ([exports.native]) aren't loadable yet — reject them
    // rather than install with a silently-inert native half.
    if !manifest.native_widgets.is_empty() || !manifest.native_formats.is_empty() {
        return Err(format!(
            "package `{name}` declares native exports ([exports.native]) — not yet supported (script packages only)"
        ));
    }
    let entry = read_pkg_module(dir.parent().unwrap_or(dir), name).ok_or_else(|| {
        format!("package `{name}`: no entry module (NAME.arb / main.arb / [exports])")
    })?;
    let cmds = crate::parser::parse(&entry)?;
    crate::spec::build(&cmds)?;
    Ok(manifest)
}

/// `git clone --depth 1 repo pkg_dir/name`, then validate; rollback on failure.
pub fn install_from(repo: &str, name: &str, pkg_dir: &Path) -> Result<PathBuf, String> {
    let dest = pkg_dir.join(name);
    if dest.exists() {
        return Err(format!(
            "`{name}` is already installed (uninstall it first)"
        ));
    }
    std::fs::create_dir_all(pkg_dir).map_err(|e| format!("pkg dir: {e}"))?;
    run_git(
        &["clone", "--depth", "1", repo, &dest.to_string_lossy()],
        None,
    )?;
    match validate_package(&dest, name) {
        Ok(_) => Ok(dest),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dest); // rollback a broken package
            Err(format!("`{name}` failed validation: {e}"))
        }
    }
}

/// Recursively install `name` + its transitive `[deps]` from `idx` into
/// `pkg_dir`. `visited` guards cycles/diamonds; `installed` records what THIS run
/// created so the caller can roll back on failure. Deps resolve to each name's
/// index-pinned ref (no semver — SPEC §18 caveat). Pre-order: a package's
/// manifest must be on disk before its deps are discoverable.
fn install_rec(
    name: &str,
    idx: &Index,
    pkg_dir: &Path,
    visited: &mut BTreeSet<String>,
    installed: &mut Vec<String>,
) -> Result<(), String> {
    if !visited.insert(name.to_string()) {
        return Ok(()); // cycle or diamond — already handled this run
    }
    if pkg_dir.join(name).exists() {
        return Ok(()); // skip an already-installed dep
    }
    let entry = idx
        .get(name)
        .ok_or_else(|| format!("package `{name}` not in the registry"))?;
    install_from(&entry.repo, name, pkg_dir)?; // clone + validate + rollback-if-broken
    installed.push(name.to_string());
    // Re-read the freshly-installed manifest to discover its deps.
    let src = std::fs::read_to_string(pkg_dir.join(name).join("arb.toml"))
        .map_err(|_| format!("`{name}`: missing arb.toml after install"))?;
    let manifest = parse_manifest(&src)?;
    for (dep, constraint) in &manifest.deps {
        check_constraint(name, dep, constraint, idx)?;
        install_rec(dep, idx, pkg_dir, visited, installed)?;
    }
    Ok(())
}

/// Verify the index version of `dep` satisfies `requirer`'s `[deps]` constraint.
/// An unknown dep is left to `install_rec` (which errors on the missing entry);
/// an unparseable index version we don't own is a warning, not a hard failure.
fn check_constraint(
    requirer: &str,
    dep: &str,
    constraint: &str,
    idx: &Index,
) -> Result<(), String> {
    let Some(entry) = idx.get(dep) else {
        return Ok(());
    };
    let req = semver::VersionReq::parse(constraint).map_err(|e| {
        format!("package `{requirer}`: bad version constraint `{constraint}` for `{dep}`: {e}")
    })?;
    let ver = match semver::Version::parse(&entry.version) {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "arb: registry version `{}` for `{dep}` is not semver — skipping check",
                entry.version
            );
            return Ok(());
        }
    };
    if !req.matches(&ver) {
        return Err(format!(
            "package `{requirer}` requires `{dep}` {constraint} but the registry has `{dep}` {ver}"
        ));
    }
    Ok(())
}

/// Install `name` + transitive deps using an explicit index and pkg dir
/// (env-free — the unit-test seam). On any failure the whole run is rolled back
/// (a failed dep never leaves a partial tree; `arb install` never half-succeeds).
pub fn install_with_index(name: &str, idx: &Index, pkg_dir: &Path) -> Result<PathBuf, String> {
    if pkg_dir.join(name).exists() {
        return Err(format!(
            "`{name}` is already installed (uninstall it first)"
        ));
    }
    let mut visited = BTreeSet::new();
    let mut installed: Vec<String> = Vec::new();
    match install_rec(name, idx, pkg_dir, &mut visited, &mut installed) {
        Ok(()) => Ok(pkg_dir.join(name)),
        Err(e) => {
            for n in &installed {
                let _ = std::fs::remove_dir_all(pkg_dir.join(n)); // undo only this run
            }
            Err(e)
        }
    }
}

/// `arb install NAME`: look up the local index and install the named package
/// plus its transitive `[deps]`.
pub fn install(name: &str) -> Result<PathBuf, String> {
    let reg = registry_dir().ok_or("no HOME for the registry")?;
    let index_src = std::fs::read_to_string(reg.join("index.json"))
        .map_err(|_| "no registry index — run `arb update` first".to_string())?;
    let idx = parse_index(&index_src)?;
    let dir = pkg_dir().ok_or("no HOME for packages")?;
    install_with_index(name, &idx, &dir)
}

/// `arb update`: clone or fast-forward the registry index repo.
pub fn update_registry() -> Result<(), String> {
    let reg = registry_dir().ok_or("no HOME for the registry")?;
    if reg.join(".git").is_dir() {
        run_git(&["pull", "--ff-only"], Some(&reg))?;
    } else {
        run_git(
            &[
                "clone",
                "--depth",
                "1",
                &registry_url(),
                &reg.to_string_lossy(),
            ],
            None,
        )?;
    }
    Ok(())
}

/// `arb uninstall NAME`: remove an installed package. Returns whether it existed.
pub fn uninstall(name: &str) -> Result<bool, String> {
    let dir = pkg_dir().ok_or("no HOME for packages")?.join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("uninstall: {e}"))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Installed packages as (name, description).
pub fn list_installed() -> Vec<(String, String)> {
    let Some(dir) = pkg_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        if !e.path().is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let desc = std::fs::read_to_string(e.path().join("arb.toml"))
            .ok()
            .and_then(|s| parse_manifest(&s).ok())
            .map(|m| m.desc)
            .unwrap_or_default();
        out.push((name, desc));
    }
    out.sort();
    out
}

/// Validate a package directory for publishing: `arb.toml` parses, has a name +
/// version, isn't a (still-unsupported) native package, and every exported module
/// (entry + listed modules) builds. Returns the manifest.
pub fn validate_publish(dir: &Path) -> Result<Manifest, String> {
    let manifest_src = std::fs::read_to_string(dir.join("arb.toml"))
        .map_err(|_| "publish: no arb.toml in the current directory".to_string())?;
    let m = parse_manifest(&manifest_src)?;
    if m.name.is_empty() || m.version.is_empty() {
        return Err("publish: arb.toml [package] needs name and version".into());
    }
    if !m.native_widgets.is_empty() || !m.native_formats.is_empty() {
        return Err(format!(
            "publish: `{}` declares native exports ([exports.native]) — not yet supported (script packages only)",
            m.name
        ));
    }
    let mut names: Vec<String> = m.modules.clone();
    if names.is_empty() {
        names.push(m.name.clone());
    }
    for module in &names {
        let path = dir.join(format!("{module}.arb"));
        let src = std::fs::read_to_string(&path)
            .map_err(|_| format!("publish: exported module `{module}.arb` not found"))?;
        let cmds = crate::parser::parse(&src)?;
        crate::spec::build(&cmds)?;
    }
    Ok(m)
}

/// Serialize an `Index` back to the `index.json` on-disk form (pretty, sorted by
/// name via the `BTreeMap`, trailing newline).
pub fn serialize_index(idx: &Index) -> String {
    let map: serde_json::Map<String, serde_json::Value> = idx
        .iter()
        .map(|(name, e)| {
            (
                name.clone(),
                serde_json::json!({ "repo": e.repo, "version": e.version, "desc": e.desc }),
            )
        })
        .collect();
    let mut s = serde_json::to_string_pretty(&serde_json::Value::Object(map)).unwrap_or_default();
    s.push('\n');
    s
}

/// The result of a publish: the entry was pushed to the index remote, or the
/// commit was made locally but the push was denied (no write access → PR flow).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Published {
    Pushed,
    CommittedLocally,
}

/// Env-free publish core (the unit-test seam): validate the package in `pkg_dir`,
/// upsert its `{repo, version, desc}` entry into the registry clone at `reg_dir`
/// (cloning from `reg_url` if absent, else fast-forward pulling first), commit,
/// and — when `push` — push to the index remote. Returns the outcome + manifest.
pub fn publish_with(
    pkg_dir: &Path,
    repo: &str,
    reg_dir: &Path,
    reg_url: &str,
    push: bool,
) -> Result<(Published, Manifest), String> {
    let m = validate_publish(pkg_dir)?;
    if repo.trim().is_empty() {
        return Err("publish: the package's git repo URL is empty".into());
    }
    // Ensure the registry index is present and current before editing it.
    if reg_dir.join(".git").is_dir() {
        run_git(&["pull", "--ff-only"], Some(reg_dir))?;
    } else {
        run_git(&["clone", reg_url, &reg_dir.to_string_lossy()], None)?;
    }
    // Upsert the entry (latest-version-per-name, like the crates.io index tip).
    let idx_path = reg_dir.join("index.json");
    let mut idx = match std::fs::read_to_string(&idx_path) {
        Ok(s) => parse_index(&s)?,
        Err(_) => Index::new(),
    };
    idx.insert(
        m.name.clone(),
        IndexEntry {
            repo: repo.to_string(),
            version: m.version.clone(),
            desc: m.desc.clone(),
        },
    );
    std::fs::write(&idx_path, serialize_index(&idx))
        .map_err(|e| format!("publish: write index: {e}"))?;
    run_git(&["add", "index.json"], Some(reg_dir))?;
    run_git(
        &[
            "commit",
            "-m",
            &format!("publish {} v{}", m.name, m.version),
        ],
        Some(reg_dir),
    )?;
    if push {
        match run_git(&["push"], Some(reg_dir)) {
            Ok(_) => Ok((Published::Pushed, m)),
            // No write access — the commit stays local for a fork+PR.
            Err(_) => Ok((Published::CommittedLocally, m)),
        }
    } else {
        Ok((Published::CommittedLocally, m))
    }
}

/// `arb publish [REPO_URL]`: validate the package in `dir`, then register it in
/// the community index by committing + pushing its entry (real git over the
/// GitHub-hosted index). REPO_URL defaults to the package's `origin` remote.
pub fn publish(dir: &Path, repo: Option<&str>) -> Result<(), String> {
    let repo = match repo {
        Some(r) => r.to_string(),
        None => run_git(&["-C", &dir.to_string_lossy(), "remote", "get-url", "origin"], None)
            .map_err(|_| {
                "publish: no `origin` git remote in the package dir — pass the repo URL: `arb publish <git-url>`"
                    .to_string()
            })?
            .trim()
            .to_string(),
    };
    let reg = registry_dir().ok_or("no HOME for the registry")?;
    let url = registry_url();
    let (outcome, m) = publish_with(dir, &repo, &reg, &url, true)?;
    match outcome {
        Published::Pushed => {
            eprintln!("arb: published `{}` v{} to {url}", m.name, m.version);
        }
        Published::CommittedLocally => {
            eprintln!(
                "arb: validated + committed `{}` v{} to the local index, but the push to {url} was denied.",
                m.name, m.version
            );
            eprintln!("arb: you likely lack write access — fork the index and open a PR:");
            eprintln!("arb:   gh repo fork {url} --clone && cd arb-registry \\");
            eprintln!("arb:     && git fetch {} && git cherry-pick FETCH_HEAD && git push && gh pr create", reg.display());
        }
    }
    Ok(())
}

/// argv shim for the registry verbs (SPEC §18), run before clap. Returns
/// `Some(exit_code)` when the first arg is a registry verb, else `None` to fall
/// through to the normal flag/spec CLI.
pub fn dispatch(args: &[String]) -> Option<i32> {
    let verb = args.first()?;
    if verb.starts_with('-') {
        return None;
    }
    match verb.as_str() {
        "install" | "add" => Some(match args.get(1) {
            Some(name) => match install(name) {
                Ok(dir) => {
                    eprintln!("arb: installed `{name}` -> {}", dir.display());
                    0
                }
                Err(e) => {
                    eprintln!("arb: {e}");
                    1
                }
            },
            None => {
                eprintln!("arb: usage: arb install NAME");
                2
            }
        }),
        "uninstall" => Some(match args.get(1) {
            Some(name) => match uninstall(name) {
                Ok(true) => {
                    eprintln!("arb: uninstalled `{name}`");
                    0
                }
                Ok(false) => {
                    eprintln!("arb: `{name}` is not installed");
                    1
                }
                Err(e) => {
                    eprintln!("arb: {e}");
                    1
                }
            },
            None => {
                eprintln!("arb: usage: arb uninstall NAME");
                2
            }
        }),
        "search" => Some(match args.get(1) {
            Some(q) => match registry_dir()
                .ok_or("no HOME".to_string())
                .and_then(|r| {
                    std::fs::read_to_string(r.join("index.json"))
                        .map_err(|_| "no registry index — run `arb update` first".to_string())
                })
                .and_then(|s| parse_index(&s))
            {
                Ok(idx) => {
                    for (name, e) in search_index(&idx, q) {
                        println!("{name:<20} {} — {}", e.version, e.desc);
                    }
                    0
                }
                Err(e) => {
                    eprintln!("arb: {e}");
                    1
                }
            },
            None => {
                eprintln!("arb: usage: arb search QUERY");
                2
            }
        }),
        "update" => Some(match update_registry() {
            Ok(()) => {
                eprintln!("arb: registry updated");
                0
            }
            Err(e) => {
                eprintln!("arb: update: {e}");
                1
            }
        }),
        "publish" => Some(
            match publish(Path::new("."), args.get(1).map(String::as_str)) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("arb: {e}");
                    1
                }
            },
        ),
        _ => None,
    }
}
