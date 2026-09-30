//! Bounded content observations made by the shell owner, never supplied outcomes.
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_FILES: usize = 64;
const MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Versioned observation of explicitly selected scopes, not a claim of complete input coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentObservation {
    pub version: u32,
    pub workspace: PathBuf,
    pub files: Vec<FileObservation>,
    /// Exhaustively enumerated directory scopes, including empty directories.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directories: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileObservation {
    pub path: PathBuf,
    pub sha256: String,
}

/// Observe bounded regular files and directory scopes confined to the command directory.
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
    let mut pending = paths.to_vec();
    let mut directories = Vec::new();
    while let Some(path) = pending.pop() {
        if path.as_os_str().is_empty()
            || path.as_os_str().len() > 4096
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
            || !unique.insert(path.clone())
        {
            return Err("observation requires unique normalized relative file paths".into());
        }
        if unique.len() > MAX_FILES {
            return Err("content observation entry limit exceeded".into());
        }
        let mut file = open_confined(&workspace, &path)?;
        let metadata = file
            .metadata()
            .map_err(|_| "observation metadata unavailable")?;
        if metadata.is_dir() {
            let children = directory_entries(file, MAX_FILES - unique.len())?;
            if pending.len() + children.len() + unique.len() > MAX_FILES {
                return Err("content observation entry limit exceeded".into());
            }
            pending.extend(children.into_iter().map(|name| path.join(name)));
            directories.push(path);
            continue;
        }
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
    if directories.is_empty() {
        files.reverse(); // Preserve the V1 caller-selected order.
    } else {
        files.sort_by(|left, right| left.path.cmp(&right.path));
        directories.sort();
    }
    Ok(ContentObservation {
        version: if directories.is_empty() { 1 } else { 2 },
        workspace,
        files,
        directories,
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn directory_entries(file: std::fs::File, limit: usize) -> Result<Vec<PathBuf>, String> {
    use std::os::fd::{FromRawFd, IntoRawFd};
    use std::os::unix::ffi::OsStrExt;

    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            // SAFETY: this guard exclusively owns the stream from fdopendir.
            unsafe { libc::closedir(self.0) };
        }
    }
    let descriptor = file.into_raw_fd();
    // SAFETY: descriptor is live and owned; ownership transfers only on success.
    let stream = unsafe { libc::fdopendir(descriptor) };
    if stream.is_null() {
        // SAFETY: failed fdopendir leaves descriptor ownership with the caller.
        drop(unsafe { std::fs::File::from_raw_fd(descriptor) });
        return Err("observation directory unavailable".into());
    }
    let directory = Directory(stream);
    let mut entries = Vec::new();
    loop {
        // SAFETY: errno is thread-local, and this live directory has no other users.
        let name = unsafe {
            #[cfg(target_os = "macos")]
            let errno = libc::__error();
            #[cfg(target_os = "linux")]
            let errno = libc::__errno_location();
            *errno = 0;
            let entry = libc::readdir(directory.0);
            if entry.is_null() {
                if *errno != 0 {
                    return Err("observation directory read failed".into());
                }
                break;
            }
            std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()).to_bytes()
        };
        if name == b"." || name == b".." {
            continue;
        }
        if entries.len() >= limit || std::str::from_utf8(name).is_err() {
            return Err("observation directory exceeds supported bounds".into());
        }
        entries.push(PathBuf::from(std::ffi::OsStr::from_bytes(name)));
    }
    Ok(entries)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn directory_entries(_file: std::fs::File, _limit: usize) -> Result<Vec<PathBuf>, String> {
    Err("directory observations are unsupported on this platform".into())
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
    fn directory_scopes_detect_membership_and_content_changes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/empty")).unwrap();
        std::fs::write(root.path().join("src/a"), "before").unwrap();
        let paths = [PathBuf::from("src")];
        let before = observe(root.path(), &paths).unwrap();
        assert_eq!(before.version, 2);
        assert_eq!(
            before.directories,
            [PathBuf::from("src"), PathBuf::from("src/empty")]
        );
        assert_eq!(before, observe(root.path(), &paths).unwrap());
        std::fs::write(root.path().join("src/b"), "added").unwrap();
        assert_ne!(before, observe(root.path(), &paths).unwrap());
        std::fs::remove_file(root.path().join("src/b")).unwrap();
        assert_eq!(before, observe(root.path(), &paths).unwrap());
        std::fs::remove_dir(root.path().join("src/empty")).unwrap();
        assert_ne!(before, observe(root.path(), &paths).unwrap());
        assert!(observe(root.path(), &[PathBuf::from("src"), PathBuf::from("src/a")]).is_err());
        for index in 0..64 {
            std::fs::write(root.path().join(format!("src/{index}")), "").unwrap();
        }
        assert!(observe(root.path(), &paths).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn directory_scope_rejects_symlink_members() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::os::unix::fs::symlink("/", root.path().join("src/link")).unwrap();
        assert!(observe(root.path(), &[PathBuf::from("src")]).is_err());
    }

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
