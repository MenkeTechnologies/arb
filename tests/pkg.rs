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
        assert!(Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success());
    }
    format!("file://{}", dir.display())
}

fn entry(repo: &str) -> IndexEntry {
    IndexEntry {
        repo: repo.into(),
        version: "0.1.0".into(),
        desc: String::new(),
    }
}

fn tmp(label: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arb-pkg-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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
    assert_eq!(
        read_pkg_module(&root, "gamma/sub").as_deref(),
        Some("list .g")
    );
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
    std::fs::write(
        src_repo.join("arb.toml"),
        "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(src_repo.join("foo.arb"), "gauge .g -max 100").unwrap();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(&src_repo)
            .output()
            .unwrap()
            .status
            .success());
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
    assert_eq!(
        read_pkg_module(&pkgdir, "foo").as_deref(),
        Some("gauge .g -max 100")
    );

    // A package whose entry spec fails to build is rolled back (dir removed).
    let bad_repo = tmp("badrepo");
    std::fs::write(
        bad_repo.join("arb.toml"),
        "[package]\nname = \"bad\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(bad_repo.join("bad.arb"), "gauge foo").unwrap(); // invalid: path must start with '.'
    let bgit = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(&bad_repo)
            .output()
            .unwrap()
            .status
            .success());
    };
    bgit(&["init", "-q"]);
    bgit(&["config", "user.email", "t@e"]);
    bgit(&["config", "user.name", "t"]);
    bgit(&["add", "-A"]);
    bgit(&["commit", "-qm", "init"]);
    let bad_url = format!("file://{}", bad_repo.display());
    assert!(install_from(&bad_url, "bad", &pkgdir).is_err());
    assert!(
        !pkgdir.join("bad").exists(),
        "broken package must be rolled back"
    );

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
    let url_b = make_pkg_repo(
        &base.join("b"),
        "arb-b",
        "[package]\nname = \"arb-b\"\nversion = \"0.1.0\"\n",
        "tail .b",
    );
    let mut idx = Index::new();
    idx.insert("arb-a".into(), entry(&url_a));
    idx.insert("arb-b".into(), entry(&url_b));
    let pkgdir = base.join("pkgs");
    install_with_index("arb-a", &idx, &pkgdir).expect("install ok");
    // Both the package and its dep land, and the dep resolves as a module tier.
    assert!(pkgdir.join("arb-a/arb-a.arb").exists());
    assert!(pkgdir.join("arb-b/arb-b.arb").exists());
    assert_eq!(
        read_pkg_module(&pkgdir, "arb-b").as_deref(),
        Some("tail .b")
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn install_with_index_rejects_incompatible_dep_version() {
    if !have_git() {
        return;
    }
    let base = tmp("deps-semver");
    // A requires arb-b ^2, but the index only has arb-b 0.1.0 -> reject + rollback.
    let url_a = make_pkg_repo(
        &base.join("a"),
        "arb-a",
        "[package]\nname = \"arb-a\"\nversion = \"0.1.0\"\n\n[deps]\narb-b = \"2\"\n",
        "gauge .g",
    );
    let url_b = make_pkg_repo(
        &base.join("b"),
        "arb-b",
        "[package]\nname = \"arb-b\"\nversion = \"0.1.0\"\n",
        "tail .b",
    );
    let mut idx = Index::new();
    idx.insert("arb-a".into(), entry(&url_a));
    idx.insert("arb-b".into(), entry(&url_b)); // version 0.1.0
    let pkgdir = base.join("pkgs");
    let e = install_with_index("arb-a", &idx, &pkgdir).unwrap_err();
    assert!(
        e.contains("requires") && e.contains("arb-b") && e.contains("0.1.0"),
        "err: {e}"
    );
    assert!(
        !pkgdir.join("arb-a").exists(),
        "rejected install must roll back"
    );
    // A compatible constraint (^0.1) installs fine.
    let url_a2 = make_pkg_repo(
        &base.join("a2"),
        "arb-a",
        "[package]\nname = \"arb-a\"\nversion = \"0.1.0\"\n\n[deps]\narb-b = \"0.1\"\n",
        "gauge .g",
    );
    idx.insert("arb-a".into(), entry(&url_a2));
    assert!(install_with_index("arb-a", &idx, &pkgdir).is_ok());
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
    assert!(
        e.contains("ghost") && e.contains("not in the registry"),
        "err: {e}"
    );
    // Failed-dep run is rolled back — no partial tree.
    assert!(!pkgdir.join("arb-c").exists());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn install_rejects_native_exports() {
    if !have_git() {
        return;
    }
    let base = tmp("native");
    let url = make_pkg_repo(
        &base.join("n"),
        "arb-native",
        "[package]\nname = \"arb-native\"\nversion = \"0.1.0\"\n\n[exports.native]\nwidgets = [\"flamegraph\"]\n",
        "gauge .g",
    );
    let mut idx = Index::new();
    idx.insert("arb-native".into(), entry(&url));
    let pkgdir = base.join("pkgs");
    let e = install_with_index("arb-native", &idx, &pkgdir).unwrap_err();
    assert!(
        e.contains("native exports") && e.contains("not yet supported"),
        "err: {e}"
    );
    assert!(!pkgdir.join("arb-native").exists()); // rolled back
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

/// Run `git args` in `cwd`, asserting success (test helper).
fn git_ok(cwd: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap()
            .status
            .success(),
        "git {args:?} failed in {}",
        cwd.display()
    );
}

/// Create a bare git repo seeded with `index.json` on `main`, returning its
/// `file://` URL (stands in for the GitHub-hosted registry index).
fn make_bare_registry(root: &Path, seed_index: &str) -> String {
    let bare = root.join("registry.git");
    std::fs::create_dir_all(&bare).unwrap();
    git_ok(
        &bare,
        &["-c", "init.defaultBranch=main", "init", "-q", "--bare"],
    );
    // Seed via a throwaway clone.
    let seed = root.join("seed");
    git_ok(
        root,
        &[
            "clone",
            "-q",
            &bare.to_string_lossy(),
            &seed.to_string_lossy(),
        ],
    );
    std::fs::write(seed.join("index.json"), seed_index).unwrap();
    for a in [
        &["config", "user.email", "t@e"][..],
        &["config", "user.name", "t"],
        &["add", "-A"],
        &["commit", "-qm", "seed"],
        &["push", "-q", "origin", "main"],
    ] {
        git_ok(&seed, a);
    }
    format!("file://{}", bare.display())
}

#[test]
fn publish_upserts_entry_and_pushes_to_the_index() {
    use arb::pkg::{publish_with, Published};
    if !have_git() {
        return;
    }
    let root = tmp("publish");
    let reg_url = make_bare_registry(&root, "{}\n");
    // A valid package repo (arb.toml + entry module that builds).
    let pkg = root.join("mypkg");
    make_pkg_repo(
        &pkg,
        "mypkg",
        "[package]\nname = \"mypkg\"\nversion = \"0.2.0\"\ndescription = \"a demo\"\n",
        "tail .t\nsource .t { in; count }\n",
    );
    // Pre-clone the registry so the commit uses a known git identity (CI has none
    // globally); publish_with then fast-forward-pulls this clone.
    let reg = root.join("regclone");
    git_ok(&root, &["clone", "-q", &reg_url, &reg.to_string_lossy()]);
    git_ok(&reg, &["config", "user.email", "pub@e"]);
    git_ok(&reg, &["config", "user.name", "pub"]);

    let (outcome, m) =
        publish_with(&pkg, "https://example.com/mypkg.git", &reg, &reg_url, true).unwrap();
    assert_eq!(outcome, Published::Pushed);
    assert_eq!(m.name, "mypkg");

    // The entry is on the REMOTE now: a fresh clone sees it.
    let verify = root.join("verify");
    git_ok(&root, &["clone", "-q", &reg_url, &verify.to_string_lossy()]);
    let idx = parse_index(&std::fs::read_to_string(verify.join("index.json")).unwrap()).unwrap();
    let e = idx.get("mypkg").expect("mypkg registered");
    assert_eq!(e.version, "0.2.0");
    assert_eq!(e.repo, "https://example.com/mypkg.git");
    assert_eq!(e.desc, "a demo");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn publish_rejects_invalid_package() {
    use arb::pkg::publish_with;
    if !have_git() {
        return;
    }
    let root = tmp("publish-bad");
    let reg_url = make_bare_registry(&root, "{}\n");
    let reg = root.join("regclone");
    // A package whose entry module does not build.
    let pkg = root.join("badpkg");
    make_pkg_repo(
        &pkg,
        "badpkg",
        "[package]\nname = \"badpkg\"\nversion = \"0.1.0\"\n",
        "gauge .g {\n", // unterminated block -> build error
    );
    assert!(publish_with(&pkg, "https://example.com/badpkg.git", &reg, &reg_url, true).is_err());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn install_checks_installed_version_not_the_index_lie() {
    if !have_git() {
        return;
    }
    let base = tmp("deps-indexlie");
    // arb-a requires arb-b ^5. The index CLAIMS arb-b 5.0.0, but the repo's
    // arb.toml actually builds 0.1.0 — the install must validate the artifact and
    // reject (the old code trusted the index and installed a violating dep).
    let url_a = make_pkg_repo(
        &base.join("a"),
        "arb-a",
        "[package]\nname = \"arb-a\"\nversion = \"0.1.0\"\n\n[deps]\narb-b = \"5\"\n",
        "gauge .g",
    );
    let url_b = make_pkg_repo(
        &base.join("b"),
        "arb-b",
        "[package]\nname = \"arb-b\"\nversion = \"0.1.0\"\n", // real version is 0.1.0
        "tail .b",
    );
    let mut idx = Index::new();
    idx.insert("arb-a".into(), entry(&url_a));
    // The index entry LIES: version 5.0.0 while the repo is 0.1.0.
    idx.insert(
        "arb-b".into(),
        IndexEntry {
            repo: url_b,
            version: "5.0.0".into(),
            desc: String::new(),
        },
    );
    let pkgdir = base.join("pkgs");
    let e = install_with_index("arb-a", &idx, &pkgdir).unwrap_err();
    assert!(e.contains("installed") && e.contains("0.1.0"), "err: {e}");
    assert!(!pkgdir.join("arb-a").exists(), "must roll back");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn install_walks_deps_of_a_preexisting_mid_tree_package() {
    if !have_git() {
        return;
    }
    let base = tmp("deps-preexist");
    let url_c = make_pkg_repo(
        &base.join("c"),
        "arb-c",
        "[package]\nname = \"arb-c\"\nversion = \"0.1.0\"\n",
        "tail .c",
    );
    let url_b = make_pkg_repo(
        &base.join("b"),
        "arb-b",
        "[package]\nname = \"arb-b\"\nversion = \"0.1.0\"\n\n[deps]\narb-c = \"0.1\"\n",
        "tail .b",
    );
    let url_a = make_pkg_repo(
        &base.join("a"),
        "arb-a",
        "[package]\nname = \"arb-a\"\nversion = \"0.1.0\"\n\n[deps]\narb-b = \"0.1\"\n",
        "gauge .g",
    );
    let mut idx = Index::new();
    for (n, u) in [("arb-a", url_a), ("arb-b", url_b), ("arb-c", url_c)] {
        idx.insert(n.into(), entry(&u));
    }
    let pkgdir = base.join("pkgs");
    // Install arb-b (pulls arb-c), then remove arb-c so it's a gap.
    install_with_index("arb-b", &idx, &pkgdir).expect("b ok");
    std::fs::remove_dir_all(pkgdir.join("arb-c")).unwrap();
    // Installing arb-a must re-fill arb-c through the pre-existing arb-b.
    install_with_index("arb-a", &idx, &pkgdir).expect("a ok");
    assert!(
        pkgdir.join("arb-c").exists(),
        "transitive dep must be installed"
    );
    let _ = std::fs::remove_dir_all(&base);
}
