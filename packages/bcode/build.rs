use sha2::{Digest as _, Sha256};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SOURCE_ROOTS: &[&str] = &[".cargo", "catalog", "packages", "plugins", "workers"];
const ROOT_FILES: &[&str] = &[
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
    "clippy.toml",
];

fn main() {
    println!("cargo:rerun-if-env-changed=PROFILE");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    println!("cargo:rerun-if-env-changed=RUSTFLAGS");
    println!("cargo:rerun-if-env-changed=BCODE_DISTRIBUTION_BUILD");
    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rerun-if-env-changed=RUSTC_WRAPPER");
    let workspace = workspace_root();
    let mode = distribution_mode();
    let git = git_metadata(&workspace);
    let source_files = source_files(&workspace);
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
    let source_digest = source_digest(&workspace, &source_files);
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
}

fn distribution_mode() -> &'static str {
    match env::var("BCODE_DISTRIBUTION_BUILD") {
        Err(env::VarError::NotPresent) => "developer",
        Ok(value) if value == "1" => "distribution",
        Ok(value) if value == "0" => "developer",
        Ok(value) => panic!("invalid BCODE_DISTRIBUTION_BUILD {value:?}; expected `0` or `1`"),
        Err(env::VarError::NotUnicode(_)) => {
            panic!("BCODE_DISTRIBUTION_BUILD is not valid Unicode")
        }
    }
}

struct GitMetadata {
    commit: String,
    dirty: bool,
}

fn git_metadata(workspace: &Path) -> GitMetadata {
    let commit = git_output(workspace, &["rev-parse", "--short=8", "HEAD"])
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .unwrap_or_default();
    let dirty = git_output(
        workspace,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    )
    .is_some_and(|value| !value.is_empty());
    GitMetadata { commit, dirty }
}

fn git_output(workspace: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("--no-pager")
        .args(args)
        .current_dir(workspace)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn emit_git_rerun_paths(workspace: &Path) {
    for name in ["HEAD", "index"] {
        if let Some(path) = git_output(workspace, &["rev-parse", "--git-path", name]) {
            let path = PathBuf::from(path);
            let path = if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            };
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    if let Some(head) = git_output(workspace, &["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git_output(workspace, &["rev-parse", "--git-path", &head])
    {
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            workspace.join(path)
        };
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn source_files(workspace: &Path) -> Vec<PathBuf> {
    let Some(output) = git_output(
        workspace,
        &["ls-files", "--cached", "--others", "--exclude-standard"],
    ) else {
        return fallback_source_files(workspace);
    };
    let mut files = output
        .lines()
        .map(|relative| workspace.join(relative))
        .filter(|path| is_product_source(workspace, path) && path.is_file())
        .collect::<Vec<_>>();
    files.sort_by_key(|path| relative_path(workspace, path));
    files
}

fn fallback_source_files(workspace: &Path) -> Vec<PathBuf> {
    let mut files = ROOT_FILES
        .iter()
        .map(|path| workspace.join(path))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    for root in SOURCE_ROOTS {
        collect_files(&workspace.join(root), &mut files);
    }
    files.sort_by_key(|path| relative_path(workspace, path));
    files
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, files);
        } else if path.is_file() {
            files.push(path);
        }
    }
}

fn is_product_source(workspace: &Path, path: &Path) -> bool {
    let relative = relative_path(workspace, path);
    ROOT_FILES.contains(&relative.as_str())
        || SOURCE_ROOTS
            .iter()
            .any(|root| relative == *root || relative.starts_with(&format!("{root}/")))
        || relative.starts_with(".cargo/")
}

fn source_digest(workspace: &Path, files: &[PathBuf]) -> String {
    let mut digest = Sha256::new();
    for path in files {
        update(&mut digest, relative_path(workspace, path).as_bytes());
        if let Ok(contents) = fs::read(path) {
            update(&mut digest, &contents);
        }
    }
    format!("{:x}", digest.finalize())
}

fn update(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    digest.update(value);
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

fn relative_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
