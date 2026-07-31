#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Confined, deterministic discovery of repository project instructions.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Project-instruction discovery contract version.
pub const PROJECT_INSTRUCTION_SET_VERSION: u32 = 1;
/// Default maximum bytes accepted from one instruction file.
pub const DEFAULT_MAX_INSTRUCTION_BYTES: u64 = 256 * 1024;

/// One applicable instruction file in root-to-target precedence order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectInstructionFile {
    /// Normalized repository-relative path.
    pub path: String,
    /// SHA-256 of the complete UTF-8 content.
    pub sha256: String,
    /// Complete bounded UTF-8 content.
    pub content: String,
}

/// Applicable project instructions and their canonical aggregate fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectInstructionSet {
    /// Contract version.
    pub version: u32,
    /// Files in root-to-target precedence order.
    pub files: Vec<ProjectInstructionFile>,
    /// SHA-256 over ordered normalized paths and file digests.
    pub fingerprint_sha256: String,
}

impl ProjectInstructionSet {
    /// Render instruction content in precedence order for agent context.
    #[must_use]
    pub fn content(&self) -> String {
        self.files
            .iter()
            .map(|file| file.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Project-instruction discovery failure.
#[derive(Debug, thiserror::Error)]
pub enum ProjectInstructionError {
    #[error("project instruction path is outside the canonical repository root")]
    OutsideRepository,
    #[error("project instruction path is ambiguous or contains traversal")]
    AmbiguousPath,
    #[error("project instruction file escapes through a symlink: {0}")]
    SymlinkEscape(String),
    #[error("project instruction file exceeds {limit} bytes: {path}")]
    Oversized { path: String, limit: u64 },
    #[error("project instruction file is not valid UTF-8: {0}")]
    InvalidEncoding(String),
    #[error("project instruction I/O failed for {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Discover applicable `AGENTS.md` files for one or more repository-relative target paths.
///
/// Files are returned in root-to-leaf precedence order. Duplicate files applicable to multiple
/// targets are emitted once. Every read is bounded and confined beneath the canonical root.
///
/// # Errors
///
/// Returns an error for a non-canonical root, target traversal or escape, symlink escape, oversized
/// content, malformed UTF-8, or I/O failure.
pub fn discover_project_instructions(
    repository_root: &Path,
    target_paths: &[PathBuf],
    max_file_bytes: u64,
) -> Result<ProjectInstructionSet, ProjectInstructionError> {
    let root = repository_root
        .canonicalize()
        .map_err(|source| ProjectInstructionError::Io {
            path: repository_root.display().to_string(),
            source,
        })?;
    if !root.is_dir() {
        return Err(ProjectInstructionError::OutsideRepository);
    }
    let mut relative_candidates = std::collections::BTreeSet::new();
    for target in target_paths {
        let relative = normalize_target(&root, target)?;
        let mut directory = PathBuf::new();
        relative_candidates.insert(PathBuf::from("AGENTS.md"));
        let components = relative.components().collect::<Vec<_>>();
        let directory_count = if root.join(&relative).is_dir() {
            components.len()
        } else {
            components.len().saturating_sub(1)
        };
        for component in components.into_iter().take(directory_count) {
            directory.push(component.as_os_str());
            relative_candidates.insert(directory.join("AGENTS.md"));
        }
    }
    let mut candidates = relative_candidates.into_iter().collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    let mut files = Vec::new();
    for relative in candidates {
        let candidate = root.join(&relative);
        let metadata = match fs::symlink_metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(ProjectInstructionError::Io {
                    path: relative.display().to_string(),
                    source,
                });
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(ProjectInstructionError::SymlinkEscape(
                relative.display().to_string(),
            ));
        }
        if !metadata.is_file() {
            return Err(ProjectInstructionError::AmbiguousPath);
        }
        if metadata.len() > max_file_bytes {
            return Err(ProjectInstructionError::Oversized {
                path: relative.display().to_string(),
                limit: max_file_bytes,
            });
        }
        let bytes = fs::read(&candidate).map_err(|source| ProjectInstructionError::Io {
            path: relative.display().to_string(),
            source,
        })?;
        let content = String::from_utf8(bytes.clone()).map_err(|_| {
            ProjectInstructionError::InvalidEncoding(relative.display().to_string())
        })?;
        files.push(ProjectInstructionFile {
            path: normalize_relative_display(&relative),
            sha256: sha256_hex(&bytes),
            content,
        });
    }
    let mut fingerprint = Sha256::new();
    for file in &files {
        fingerprint.update(file.path.as_bytes());
        fingerprint.update([0]);
        fingerprint.update(file.sha256.as_bytes());
        fingerprint.update([0]);
    }
    Ok(ProjectInstructionSet {
        version: PROJECT_INSTRUCTION_SET_VERSION,
        files,
        fingerprint_sha256: digest_hex(fingerprint.finalize()),
    })
}

fn normalize_target(root: &Path, target: &Path) -> Result<PathBuf, ProjectInstructionError> {
    let relative = if target.is_absolute() {
        target
            .strip_prefix(root)
            .map_err(|_| ProjectInstructionError::OutsideRepository)?
            .to_path_buf()
    } else {
        target.to_path_buf()
    };
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(ProjectInstructionError::AmbiguousPath);
    }
    Ok(relative)
}

fn normalize_relative_display(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn sha256_hex(bytes: &[u8]) -> String {
    digest_hex(Sha256::digest(bytes))
}

fn digest_hex(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
            output
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_root_to_target_union_with_stable_fingerprint() {
        let temp = tempfile::tempdir().expect("temp");
        fs::create_dir_all(temp.path().join("src/nested")).expect("directories");
        fs::write(temp.path().join("AGENTS.md"), "root").expect("root");
        fs::write(temp.path().join("src/AGENTS.md"), "source").expect("source");
        let set = discover_project_instructions(
            temp.path(),
            &[PathBuf::from("src/nested/file.rs")],
            DEFAULT_MAX_INSTRUCTION_BYTES,
        )
        .expect("instructions");
        assert_eq!(
            set.files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["AGENTS.md", "src/AGENTS.md"]
        );
        assert_eq!(set.content(), "root\n\nsource");
        assert_eq!(set.fingerprint_sha256.len(), 64);
        assert_eq!(
            set,
            discover_project_instructions(
                temp.path(),
                &[PathBuf::from("src/nested/file.rs")],
                DEFAULT_MAX_INSTRUCTION_BYTES,
            )
            .expect("stable")
        );
    }

    #[test]
    fn fails_closed_for_traversal_oversize_and_malformed_encoding() {
        let temp = tempfile::tempdir().expect("temp");
        assert!(matches!(
            discover_project_instructions(temp.path(), &[PathBuf::from("../escape")], 10),
            Err(ProjectInstructionError::AmbiguousPath)
        ));
        fs::write(temp.path().join("AGENTS.md"), "too long").expect("oversized");
        assert!(matches!(
            discover_project_instructions(temp.path(), &[PathBuf::from("file")], 2),
            Err(ProjectInstructionError::Oversized { .. })
        ));
        fs::write(temp.path().join("AGENTS.md"), [0xff]).expect("invalid");
        assert!(matches!(
            discover_project_instructions(temp.path(), &[PathBuf::from("file")], 2),
            Err(ProjectInstructionError::InvalidEncoding(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_instruction_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("temp");
        let outside = tempfile::NamedTempFile::new().expect("outside");
        symlink(outside.path(), temp.path().join("AGENTS.md")).expect("symlink");
        assert!(matches!(
            discover_project_instructions(temp.path(), &[PathBuf::from("file")], 10),
            Err(ProjectInstructionError::SymlinkEscape(_))
        ));
    }
}
