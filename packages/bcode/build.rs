mod build_support;

use build_support::{ROOT_FILES, SOURCE_ROOTS};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=PROFILE");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    println!("cargo:rerun-if-env-changed=RUSTFLAGS");
    println!("cargo:rerun-if-env-changed=BCODE_DISTRIBUTION_BUILD");
    println!("cargo:rerun-if-env-changed=BCODE_RELEASE_CHANNEL");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rerun-if-env-changed=RUSTC_WRAPPER");
    for (name, _) in env::vars().filter(|(name, _)| name.starts_with("CARGO_FEATURE_")) {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let workspace = workspace_root();
    let mode =
        build_support::distribution_mode(env::var("BCODE_DISTRIBUTION_BUILD").ok().as_deref())
            .unwrap_or_else(|error| panic!("{error}"));
    let git = build_support::git_metadata(&workspace);
    let source_files = build_support::source_files(&workspace);
    for path in &source_files {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    for root_file in ROOT_FILES {
        println!(
            "cargo:rerun-if-changed={}",
            workspace.join(root_file).display()
        );
    }
    for root in SOURCE_ROOTS {
        println!("cargo:rerun-if-changed={}", workspace.join(root).display());
    }
    emit_git_rerun_paths(&workspace);

    let target = env::var("TARGET").unwrap_or_default();
    let profile = env::var("PROFILE").unwrap_or_default();
    let features = enabled_features();
    let rustc = rustc_identity();
    let compiler_summary = rustc
        .lines()
        .next()
        .unwrap_or("rustc-unavailable")
        .to_owned();
    let source_digest = build_support::source_digest(&workspace, &source_files);
    let digest = bcode_build_info::BuildFacts {
        source_digest,
        target,
        profile,
        features,
        compiler: rustc,
    }
    .diagnostic_digest();

    println!("cargo:rustc-env=BCODE_BUILD_MODE={mode}");
    println!("cargo:rustc-env=BCODE_BUILD_GIT_COMMIT={}", git.commit);
    println!(
        "cargo:rustc-env=BCODE_BUILD_GIT_DIRTY={}",
        if git.dirty { "1" } else { "0" }
    );
    println!("cargo:rustc-env=BCODE_BUILD_DIGEST={digest}");
    println!(
        "cargo:rustc-env=BCODE_BUILD_TARGET={}",
        env::var("TARGET").unwrap_or_default()
    );
    println!(
        "cargo:rustc-env=BCODE_BUILD_PROFILE={}",
        env::var("PROFILE").unwrap_or_default()
    );
    println!(
        "cargo:rustc-env=BCODE_BUILD_FEATURES={}",
        enabled_features().join(",")
    );
    println!("cargo:rustc-env=BCODE_BUILD_COMPILER={compiler_summary}");
    println!(
        "cargo:rustc-env=BCODE_RELEASE_CHANNEL={}",
        env::var("BCODE_RELEASE_CHANNEL").unwrap_or_default()
    );
    let source_date_epoch = env::var("SOURCE_DATE_EPOCH").unwrap_or_default();
    if !source_date_epoch.is_empty()
        && source_date_epoch
            .parse::<u64>()
            .ok()
            .filter(|value| (1..=253_402_300_799).contains(value))
            .is_none()
    {
        panic!("SOURCE_DATE_EPOCH must be a supported positive Unix timestamp");
    }
    println!("cargo:rustc-env=BCODE_BUILD_TIMESTAMP={source_date_epoch}");
}

fn emit_git_rerun_paths(workspace: &Path) {
    for name in ["HEAD", "index"] {
        if let Some(path) = build_support::git_output(workspace, &["rev-parse", "--git-path", name])
        {
            println!(
                "cargo:rerun-if-changed={}",
                resolve_path(workspace, &path).display()
            );
        }
    }
    if let Some(head) = build_support::git_output(workspace, &["symbolic-ref", "-q", "HEAD"])
        && let Some(path) =
            build_support::git_output(workspace, &["rev-parse", "--git-path", &head])
    {
        println!(
            "cargo:rerun-if-changed={}",
            resolve_path(workspace, &path).display()
        );
    }
}

fn resolve_path(workspace: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    }
}

fn enabled_features() -> Vec<String> {
    let mut features = env::vars()
        .filter_map(|(name, value)| {
            (value == "1")
                .then(|| name.strip_prefix("CARGO_FEATURE_").map(ToOwned::to_owned))
                .flatten()
        })
        .collect::<Vec<_>>();
    features.sort();
    features
}

fn rustc_identity() -> String {
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    Command::new(rustc)
        .arg("-Vv")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or_else(
            || "rustc-unavailable".to_owned(),
            |output| String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
}
