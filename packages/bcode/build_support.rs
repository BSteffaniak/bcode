//! Deterministic build-script behavior shared by the build entry point and tests.

use sha2::{Digest as _, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SOURCE_ROOTS: &[&str] = &[".cargo", "catalog", "packages", "plugins", "workers"];
pub const ROOT_FILES: &[&str] = &[
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
    "clippy.toml",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitMetadata {
    pub commit: String,
    pub dirty: bool,
}

pub fn distribution_mode(value: Option<&str>) -> Result<&'static str, String> {
    match value {
        None | Some("0") => Ok("developer"),
        Some("1") => Ok("distribution"),
        Some(value) => Err(format!(
            "invalid BCODE_DISTRIBUTION_BUILD {value:?}; expected `0` or `1`"
        )),
    }
}

pub fn git_metadata(workspace: &Path) -> GitMetadata {
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

pub fn git_output(workspace: &Path, args: &[&str]) -> Option<String> {
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

pub fn source_files(workspace: &Path) -> Vec<PathBuf> {
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

pub fn fallback_source_files(workspace: &Path) -> Vec<PathBuf> {
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
}

pub fn source_digest(workspace: &Path, files: &[PathBuf]) -> String {
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

pub fn relative_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
