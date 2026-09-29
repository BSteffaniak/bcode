//! Bounded content observations made by the shell owner, never supplied outcomes.
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_FILES: usize = 64;
const MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Versioned observation of explicitly selected files, not a claim of complete coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentObservation {
    pub version: u32,
    pub workspace: PathBuf,
    pub files: Vec<FileObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileObservation {
    pub path: PathBuf,
    pub sha256: String,
}

/// Observe only bounded regular files confined to the actual command directory.
/// Symlinks and special files fail closed, including symlinked path components.
pub fn observe(workspace: &Path, paths: &[PathBuf]) -> Result<ContentObservation, String> {
    if paths.is_empty() || paths.len() > MAX_FILES {
        return Err("content observation requires 1..=64 files".into());
    }
    let workspace = workspace
        .canonicalize()
        .map_err(|_| "observation workspace unavailable")?;
    let mut remaining = MAX_BYTES;
    let mut files = Vec::with_capacity(paths.len());
    let mut unique = std::collections::BTreeSet::new();
    for path in paths {
        if path.as_os_str().is_empty()
            || path.as_os_str().len() > 4096
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
            || !unique.insert(path.clone())
        {
            return Err("observation requires unique normalized relative file paths".into());
        }
        let mut file = open_confined(&workspace, path)?;
        let metadata = file
            .metadata()
            .map_err(|_| "observation metadata unavailable")?;
        if !metadata.is_file() || metadata.len() > remaining {
            return Err("observation requires bounded regular files".into());
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(remaining + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "observation read failed")?;
        let length = u64::try_from(bytes.len()).map_err(|_| "observation size overflow")?;
        remaining = remaining
            .checked_sub(length)
            .ok_or("observation byte limit exceeded")?;
        files.push(FileObservation {
            path: path.clone(),
            sha256: hex::encode(Sha256::digest(&bytes)),
        });
    }
    Ok(ContentObservation {
        version: 1,
        workspace,
        files,
    })
}

// Walk from an anchored root descriptor. Never check a pathname and subsequently
// reopen it: each component is opened without following links relative to the
// already opened parent. NONBLOCK also prevents a replaced FIFO from hanging us.
#[cfg(unix)]
fn open_confined(workspace: &Path, path: &Path) -> Result<std::fs::File, String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let mut directory = std::fs::File::open("/").map_err(|_| "observation root unavailable")?;
    let components: Vec<_> = workspace
        .components()
        .chain(path.components())
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    for (index, name) in components.iter().enumerate() {
        let name =
            std::ffi::CString::new(name.as_bytes()).map_err(|_| "invalid observation component")?;
        let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        if index + 1 < components.len() {
            flags |= libc::O_DIRECTORY;
        }
        // SAFETY: directory is live and name is a valid NUL-terminated string.
        let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if descriptor < 0 {
            return Err("observation component unavailable or unsafe".into());
        }
        // SAFETY: openat returned a new owned descriptor, transferred exactly once.
        directory = unsafe { std::fs::File::from_raw_fd(descriptor) };
    }
    Ok(directory)
}

#[cfg(not(unix))]
fn open_confined(_workspace: &Path, _path: &Path) -> Result<std::fs::File, String> {
    Err("confined content observations are unsupported on this platform".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observations_bind_content_and_actual_workspace() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::write(first.path().join("file"), "before").unwrap();
        std::fs::write(second.path().join("file"), "before").unwrap();
        let paths = vec![PathBuf::from("file")];
        let before = observe(first.path(), &paths).unwrap();
        assert_ne!(before, observe(second.path(), &paths).unwrap());
        std::fs::write(first.path().join("file"), "after").unwrap();
        assert_ne!(before, observe(first.path(), &paths).unwrap());
        assert!(observe(first.path(), &[PathBuf::from("missing")]).is_err());
        assert!(observe(first.path(), &[PathBuf::from("../file")]).is_err());
        assert!(observe(first.path(), &[]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn replaced_parent_symlink_cannot_escape_workspace() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "outside").unwrap();
        std::fs::create_dir(root.path().join("parent")).unwrap();
        // Simulate replacement after caller validation, before descriptor opening.
        let workspace = root.path().canonicalize().unwrap();
        std::fs::rename(root.path().join("parent"), root.path().join("old")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("parent")).unwrap();
        assert!(open_confined(&workspace, Path::new("parent/secret")).is_err());
        assert!(observe(&workspace, &[PathBuf::from("parent/secret")]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_targets_are_not_observed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), "content").unwrap();
        std::os::unix::fs::symlink("file", root.path().join("link")).unwrap();
        assert!(observe(root.path(), &[PathBuf::from("link")]).is_err());
    }
}
