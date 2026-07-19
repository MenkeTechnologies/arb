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
//! `publish` is **client-only**: the hosted index repo and its merge flow are not
//! built, so it validates the package and prints the manual PR steps — it never
//! claims a package was registered.

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
    let obj = v.as_object().ok_or("registry index: expected a JSON object")?;
    let mut idx = Index::new();
    for (name, entry) in obj {
        let get = |k: &str| entry.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        idx.insert(
            name.clone(),
            IndexEntry { repo: get("repo"), version: get("version"), desc: get("desc") },
        );
    }
    Ok(idx)
}

/// Case-insensitive substring search over package name + description.
pub fn search_index<'a>(idx: &'a Index, q: &str) -> Vec<(&'a String, &'a IndexEntry)> {
    let q = q.to_lowercase();
    idx.iter()
        .filter(|(name, e)| {
            name.to_lowercase().contains(&q) || e.desc.to_lowercase().contains(&q)
        })
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
    let str_of = |t: &toml::Value, k: &str| t.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let list_of = |t: Option<&toml::Value>, k: &str| -> Vec<String> {
        t.and_then(|x| x.get(k))
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
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
    let out = cmd.output().map_err(|e| format!("git: {e} (is git installed?)"))?;
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
    let entry = read_pkg_module(dir.parent().unwrap_or(dir), name)
        .ok_or_else(|| format!("package `{name}`: no entry module (NAME.arb / main.arb / [exports])"))?;
    let cmds = crate::parser::parse(&entry)?;
    crate::spec::build(&cmds)?;
    Ok(manifest)
}

/// `git clone --depth 1 repo pkg_dir/name`, then validate; rollback on failure.
pub fn install_from(repo: &str, name: &str, pkg_dir: &Path) -> Result<PathBuf, String> {
    let dest = pkg_dir.join(name);
    if dest.exists() {
        return Err(format!("`{name}` is already installed (uninstall it first)"));
    }
    std::fs::create_dir_all(pkg_dir).map_err(|e| format!("pkg dir: {e}"))?;
    run_git(&["clone", "--depth", "1", repo, &dest.to_string_lossy()], None)?;
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
    let entry = idx.get(name).ok_or_else(|| format!("package `{name}` not in the registry"))?;
    install_from(&entry.repo, name, pkg_dir)?; // clone + validate + rollback-if-broken
    installed.push(name.to_string());
    // Re-read the freshly-installed manifest to discover its deps.
    let src = std::fs::read_to_string(pkg_dir.join(name).join("arb.toml"))
        .map_err(|_| format!("`{name}`: missing arb.toml after install"))?;
    let manifest = parse_manifest(&src)?;
    for dep in manifest.deps.keys() {
        install_rec(dep, idx, pkg_dir, visited, installed)?;
    }
    Ok(())
}

/// Install `name` + transitive deps using an explicit index and pkg dir
/// (env-free — the unit-test seam). On any failure the whole run is rolled back
/// (a failed dep never leaves a partial tree; `arb install` never half-succeeds).
pub fn install_with_index(name: &str, idx: &Index, pkg_dir: &Path) -> Result<PathBuf, String> {
    if pkg_dir.join(name).exists() {
        return Err(format!("`{name}` is already installed (uninstall it first)"));
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
        run_git(&["clone", "--depth", "1", &registry_url(), &reg.to_string_lossy()], None)?;
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
    let Some(dir) = pkg_dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
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

/// `arb publish`: CLIENT-ONLY. Validate the package here, then print the manual
/// registration steps. The hosted index repo + merge flow do not exist yet, so
/// this never claims a package was published — it tells the user what to do.
pub fn publish(dir: &Path) -> Result<(), String> {
    let manifest_src = std::fs::read_to_string(dir.join("arb.toml"))
        .map_err(|_| "publish: no arb.toml in the current directory".to_string())?;
    let m = parse_manifest(&manifest_src)?;
    if m.name.is_empty() || m.version.is_empty() {
        return Err("publish: arb.toml [package] needs name and version".into());
    }
    // Validate every exported module builds (entry + any listed modules).
    let mut checked = 0;
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
        checked += 1;
    }
    eprintln!("arb: validated `{}` v{} ({checked} module(s) build)", m.name, m.version);
    eprintln!("arb: publishing is not automated — the registry index is community-hosted.");
    eprintln!("arb: to register, open a PR adding this entry to {}/index.json:", registry_url());
    eprintln!(
        "arb:   \"{}\": {{ \"repo\": \"<your git url>\", \"version\": \"{}\", \"desc\": \"{}\" }}",
        m.name, m.version, m.desc
    );
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
        "publish" => Some(match publish(Path::new(".")) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("arb: {e}");
                1
            }
        }),
        _ => None,
    }
}
