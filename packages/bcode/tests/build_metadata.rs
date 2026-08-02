#[path = "../build_support.rs"]
mod build_support;

use std::fs;
use std::process::Command;

#[test]
fn source_digest_tracks_modified_and_untracked_product_files() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path();
    fs::create_dir_all(root.join("packages/example/src")).expect("source dir");
    fs::write(root.join("Cargo.toml"), "[workspace]\n").expect("manifest");
    fs::write(
        root.join("packages/example/src/lib.rs"),
        "pub const VALUE: u8 = 1;\n",
    )
    .expect("source");
    run_git(root, &["init", "-q"]);
    run_git(root, &["config", "user.email", "fixture@example.invalid"]);
    run_git(root, &["config", "user.name", "Fixture"]);
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-qm", "fixture"]);

    let clean = build_support::git_metadata(root);
    assert!(!clean.commit.is_empty());
    assert!(!clean.dirty);
    let first_files = build_support::source_files(root);
    let first = build_support::source_digest(root, &first_files);

    fs::write(
        root.join("packages/example/src/lib.rs"),
        "pub const VALUE: u8 = 2;\n",
    )
    .expect("modified source");
    let dirty = build_support::git_metadata(root);
    assert_eq!(clean.commit, dirty.commit);
    assert!(dirty.dirty);
    let second = build_support::source_digest(root, &build_support::source_files(root));
    assert_ne!(first, second);

    fs::write(
        root.join("packages/example/src/new.rs"),
        "pub const NEW: bool = true;\n",
    )
    .expect("untracked source");
    let third = build_support::source_digest(root, &build_support::source_files(root));
    assert_ne!(second, third);
}

#[test]
fn git_unavailable_fallback_is_deterministic_and_normalized() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path();
    fs::create_dir_all(root.join("packages/example/src")).expect("source dir");
    fs::write(root.join("Cargo.toml"), "[workspace]\n").expect("manifest");
    fs::write(
        root.join("packages/example/src/lib.rs"),
        "pub struct Example;\n",
    )
    .expect("source");

    let git = build_support::git_metadata(root);
    assert_eq!(git.commit, "");
    assert!(!git.dirty);
    let files = build_support::fallback_source_files(root);
    assert_eq!(files, build_support::fallback_source_files(root));
    assert_eq!(
        build_support::source_digest(root, &files),
        build_support::source_digest(root, &files)
    );
    assert_eq!(
        build_support::relative_path(root, &root.join("packages/example/src/lib.rs")),
        "packages/example/src/lib.rs"
    );
}

#[test]
fn distribution_override_accepts_only_explicit_boolean_values() {
    assert_eq!(build_support::distribution_mode(None), Ok("developer"));
    assert_eq!(build_support::distribution_mode(Some("0")), Ok("developer"));
    assert_eq!(
        build_support::distribution_mode(Some("1")),
        Ok("distribution")
    );
    assert!(build_support::distribution_mode(Some("true")).is_err());
}

fn run_git(root: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("--no-pager")
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .expect("git command");
    assert!(status.success(), "git {args:?}");
}
