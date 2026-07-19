//! Package-registry tests — headless, CI-safe. Parsing/resolver tests are pure;
//! the install test uses a local `file://` git repo and is skipped where `git`
//! is absent.

use arb::pkg::{
    install_from, parse_index, parse_manifest, read_pkg_module, search_index, IndexEntry,
};
use std::path::PathBuf;
use std::process::Command;

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
