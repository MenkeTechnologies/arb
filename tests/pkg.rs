//! Package-registry tests — headless, CI-safe. Parsing/resolver tests are pure;
//! the install test uses a local `file://` git repo and is skipped where `git`
//! is absent.

use arb::pkg::{
    install_from, install_with_index, parse_index, parse_manifest, read_pkg_module, search_index,
    Index, IndexEntry,
};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Create a local git repo that is a valid arb package (arb.toml + NAME.arb),
/// commit it, and return its `file://` URL.
fn make_pkg_repo(dir: &Path, name: &str, arb_toml: &str, entry_arb: &str) -> String {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("arb.toml"), arb_toml).unwrap();
    std::fs::write(dir.join(format!("{name}.arb")), entry_arb).unwrap();
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "t@e"],
        &["config", "user.name", "t"],
        &["add", "-A"],
        &["commit", "-qm", "init"],
    ] {
        assert!(Command::new("git").args(args).current_dir(dir).output().unwrap().status.success());
    }
    format!("file://{}", dir.display())
}

fn entry(repo: &str) -> IndexEntry {
    IndexEntry { repo: repo.into(), version: "0.1.0".into(), desc: String::new() }
}

fn tmp(label: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arb-pkg-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn have_git() -> bool {
    Command::new("git").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

#[test]
fn parse_index_roundtrip_and_malformed() {
    let src = r#"{ "arb-http": { "repo": "https://x/y", "version": "0.2.0", "desc": "HTTP dashboard" } }"#;
    let idx = parse_index(src).unwrap();
    assert_eq!(
        idx.get("arb-http"),
        Some(&IndexEntry {
            repo: "https://x/y".into(),
            version: "0.2.0".into(),
            desc: "HTTP dashboard".into()
        })
    );
    assert!(parse_index("not json").is_err());
    assert!(parse_index("[1,2,3]").is_err()); // not an object
}

#[test]
fn search_index_is_case_insensitive_over_name_and_desc() {
    let idx = parse_index(
        r#"{ "arb-k8s": {"repo":"r","version":"1","desc":"kubernetes pods"},
             "arb-nginx": {"repo":"r","version":"1","desc":"web server logs"} }"#,
    )
    .unwrap();
    // by name
    assert_eq!(search_index(&idx, "K8S").len(), 1);
    // by description
    let web = search_index(&idx, "server");
    assert_eq!(web.len(), 1);
    assert_eq!(web[0].0, "arb-nginx");
    // no match
    assert!(search_index(&idx, "zzz").is_empty());
}

#[test]
fn parse_manifest_reads_package_deps_exports() {
    let src = r#"
[package]
name = "arb-k8s"
version = "0.1.0"
license = "MIT"
description = "kubectl dashboards"

[deps]
arb-http = "0.2"

[exports]
modules = ["pods", "nodes"]

[exports.native]
widgets = ["flamegraph"]
"#;
    let m = parse_manifest(src).unwrap();
    assert_eq!(m.name, "arb-k8s");
    assert_eq!(m.version, "0.1.0");
    assert_eq!(m.desc, "kubectl dashboards");
    assert_eq!(m.deps.get("arb-http").map(String::as_str), Some("0.2"));
    assert_eq!(m.modules, vec!["pods", "nodes"]);
    assert_eq!(m.native_widgets, vec!["flamegraph"]);
    // Missing [package] is an error.
    assert!(parse_manifest("[deps]\nx = \"1\"").is_err());
}

#[test]
fn read_pkg_module_resolves_entry_forms() {
    let root = tmp("resolve");
    // NAME/NAME.arb
    std::fs::create_dir_all(root.join("alpha")).unwrap();
    std::fs::write(root.join("alpha/alpha.arb"), "gauge .a").unwrap();
    assert_eq!(read_pkg_module(&root, "alpha").as_deref(), Some("gauge .a"));
    // NAME/main.arb
    std::fs::create_dir_all(root.join("beta")).unwrap();
    std::fs::write(root.join("beta/main.arb"), "tail .b").unwrap();
    assert_eq!(read_pkg_module(&root, "beta").as_deref(), Some("tail .b"));
    // NAME/sub.arb
    std::fs::create_dir_all(root.join("gamma")).unwrap();
    std::fs::write(root.join("gamma/sub.arb"), "list .g").unwrap();
    assert_eq!(read_pkg_module(&root, "gamma/sub").as_deref(), Some("list .g"));
    // unknown
    assert_eq!(read_pkg_module(&root, "nope"), None);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn install_from_clones_validates_and_rolls_back() {
    if !have_git() {
        eprintln!("skipping: git not available");
        return;
    }
    // Build a local git repo that is a valid arb package.
    let src_repo = tmp("srcrepo");
    std::fs::write(src_repo.join("arb.toml"), "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n").unwrap();
    std::fs::write(src_repo.join("foo.arb"), "gauge .g -max 100").unwrap();
    let git = |args: &[&str]| {
        assert!(Command::new("git").args(args).current_dir(&src_repo).output().unwrap().status.success());
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@e"]);
    git(&["config", "user.name", "t"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
    let repo_url = format!("file://{}", src_repo.display());

    // Install it into a fresh pkg dir.
    let pkgdir = tmp("pkgs");
    let dest = install_from(&repo_url, "foo", &pkgdir).expect("install ok");
    assert!(dest.join("foo.arb").exists());
    assert_eq!(read_pkg_module(&pkgdir, "foo").as_deref(), Some("gauge .g -max 100"));

    // A package whose entry spec fails to build is rolled back (dir removed).
    let bad_repo = tmp("badrepo");
    std::fs::write(bad_repo.join("arb.toml"), "[package]\nname = \"bad\"\nversion = \"0.1.0\"\n").unwrap();
    std::fs::write(bad_repo.join("bad.arb"), "gauge foo").unwrap(); // invalid: path must start with '.'
    let bgit = |args: &[&str]| {
        assert!(Command::new("git").args(args).current_dir(&bad_repo).output().unwrap().status.success());
    };
    bgit(&["init", "-q"]);
    bgit(&["config", "user.email", "t@e"]);
    bgit(&["config", "user.name", "t"]);
    bgit(&["add", "-A"]);
    bgit(&["commit", "-qm", "init"]);
    let bad_url = format!("file://{}", bad_repo.display());
    assert!(install_from(&bad_url, "bad", &pkgdir).is_err());
    assert!(!pkgdir.join("bad").exists(), "broken package must be rolled back");

    for d in [&src_repo, &pkgdir, &bad_repo] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn install_with_index_resolves_transitive_deps() {
    if !have_git() {
        eprintln!("skipping: git not available");
        return;
    }
    let base = tmp("deps");
    let url_a = make_pkg_repo(
        &base.join("a"),
        "arb-a",
        "[package]\nname = \"arb-a\"\nversion = \"0.1.0\"\n\n[deps]\narb-b = \"0.1\"\n",
        "gauge .g -max 100",
    );
    let url_b = make_pkg_repo(&base.join("b"), "arb-b", "[package]\nname = \"arb-b\"\nversion = \"0.1.0\"\n", "tail .b");
    let mut idx = Index::new();
    idx.insert("arb-a".into(), entry(&url_a));
    idx.insert("arb-b".into(), entry(&url_b));
    let pkgdir = base.join("pkgs");
    install_with_index("arb-a", &idx, &pkgdir).expect("install ok");
    // Both the package and its dep land, and the dep resolves as a module tier.
    assert!(pkgdir.join("arb-a/arb-a.arb").exists());
    assert!(pkgdir.join("arb-b/arb-b.arb").exists());
    assert_eq!(read_pkg_module(&pkgdir, "arb-b").as_deref(), Some("tail .b"));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn install_with_index_missing_dep_errors_and_rolls_back() {
    if !have_git() {
        return;
    }
    let base = tmp("deps-missing");
    let url_c = make_pkg_repo(
        &base.join("c"),
        "arb-c",
        "[package]\nname = \"arb-c\"\nversion = \"0.1.0\"\n\n[deps]\nghost = \"0.1\"\n",
        "list .c",
    );
    let mut idx = Index::new();
    idx.insert("arb-c".into(), entry(&url_c)); // "ghost" deliberately absent
    let pkgdir = base.join("pkgs");
    let e = install_with_index("arb-c", &idx, &pkgdir).unwrap_err();
    assert!(e.contains("ghost") && e.contains("not in the registry"), "err: {e}");
    // Failed-dep run is rolled back — no partial tree.
    assert!(!pkgdir.join("arb-c").exists());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn install_with_index_cycle_terminates() {
    if !have_git() {
        return;
    }
    let base = tmp("deps-cycle");
    let url_a = make_pkg_repo(
        &base.join("a"),
        "arb-a2",
        "[package]\nname = \"arb-a2\"\nversion = \"0.1.0\"\n\n[deps]\narb-b2 = \"0.1\"\n",
        "gauge .g",
    );
    let url_b = make_pkg_repo(
        &base.join("b"),
        "arb-b2",
        "[package]\nname = \"arb-b2\"\nversion = \"0.1.0\"\n\n[deps]\narb-a2 = \"0.1\"\n",
        "tail .b",
    );
    let mut idx = Index::new();
    idx.insert("arb-a2".into(), entry(&url_a));
    idx.insert("arb-b2".into(), entry(&url_b));
    let pkgdir = base.join("pkgs");
    // The visited-set must break the back-edge (a hang/overflow is the failure).
    install_with_index("arb-a2", &idx, &pkgdir).expect("cycle must terminate");
    assert!(pkgdir.join("arb-a2").exists() && pkgdir.join("arb-b2").exists());
    let _ = std::fs::remove_dir_all(&base);
}
