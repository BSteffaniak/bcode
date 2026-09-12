//! Physical artifact containers and atomic maintenance publication.
//!
//! A compressed artifact occupies the same logical path as the raw file, but is a directory with
//! one versioned payload. Old raw-file readers fail closed on the directory. The payload is
//! authoritative artifact content, not a disposable sidecar or a session-history replacement.

use crate::artifact_compression::ArtifactCompression;
use crate::artifact_reader::{ArtifactEncoding, ArtifactReader, prepare_artifact_transition};
use bcode_session_models::{MAX_SESSION_ARTIFACT_RANGE_BYTES, SessionId};
use std::fs::{self, File};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

const PAYLOAD: &str = "content.v1.zstd";

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "artifact storage requires maintenance",
    )
}

fn confined(path: &Path, root: &Path) -> io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(root) || fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(invalid());
    }
    Ok(canonical)
}

fn open_content(path: &Path, root: &Path) -> io::Result<(File, ArtifactEncoding)> {
    let path = confined(path, root)?;
    let metadata = fs::symlink_metadata(&path)?;
    let (file, encoding) = if metadata.is_file() {
        (File::open(&path)?, ArtifactEncoding::Raw)
    } else if metadata.is_dir() {
        // An unsupported container must not be treated as empty, raw, or an older version.
        let mut entries = fs::read_dir(&path)?;
        let entry = entries.next().ok_or_else(invalid)??;
        if entry.file_name() != PAYLOAD || entries.next().is_some() {
            return Err(invalid());
        }
        let payload = confined(&entry.path(), &path)?;
        if !fs::symlink_metadata(&payload)?.is_file() {
            return Err(invalid());
        }
        (File::open(payload)?, ArtifactEncoding::ChunkedZstd)
    } else {
        return Err(invalid());
    };
    if !file.metadata()?.is_file() {
        return Err(invalid());
    }
    Ok((file, encoding))
}

/// Read bounded logical bytes from raw or versioned container storage.
///
/// # Errors
///
/// Rejects unconfined paths, invalid ranges, unknown containers, corrupt compressed content or I/O.
/// Caller must retain session ownership through the actual blocking operation.
pub fn read_artifact_range(
    root: &Path,
    path: &Path,
    offset: u64,
    length: u32,
) -> io::Result<(u64, Vec<u8>)> {
    if length == 0 || length > MAX_SESSION_ARTIFACT_RANGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid artifact range length",
        ));
    }
    let root = root.canonicalize()?;
    let (file, encoding) = open_content(path, &root)?;
    let mut reader = ArtifactReader::new(file, encoding)?;
    let total = reader.logical_bytes();
    if offset > total {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "artifact offset exceeds logical length",
        ));
    }
    reader.seek(SeekFrom::Start(offset))?;
    let mut bytes =
        vec![0; usize::try_from((total - offset).min(u64::from(length))).map_err(|_| invalid())?];
    reader.read_exact(&mut bytes)?;
    Ok((total, bytes))
}

/// Result of explicit offline artifact maintenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactStorageOutcome {
    /// No replacement was performed because the candidate did not save enough file bytes.
    Unchanged,
    /// The candidate was published at the original path.
    Compressed {
        /// File-byte savings, excluding filesystem allocation rounding.
        saved_bytes: u64,
        /// Retained prior representation if post-publication cleanup failed. Never authoritative.
        retained_backup: Option<PathBuf>,
    },
}

/// Compress one finalized artifact during explicit offline maintenance.
///
/// Acquires the owning session's maintenance fence before opening any artifact, verifies the
/// candidate against original content, syncs it, and atomically exchanges the two representations.
/// The logical path remains authoritative before and after a crash. Staging paths are never read
/// as fallback. Cancellation before exchange leaves the original intact; after exchange publication
/// is committed and cleanup is best-effort. Readers must hold session ownership, and callers must
/// have established that this artifact is finalized and belongs to the current canonical session.
///
/// # Errors
///
/// Fails without publication for unsupported platforms, active/unverifiable ownership, missing
/// canonical storage, path escape, existing staging residue, nonregular input, insufficient
/// integrity, cancellation, or I/O. A post-exchange sync error reports failure with the original
/// path still authoritative and the prior representation retained for explicit maintenance.
pub fn compress_session_artifact(
    sessions_root: &Path,
    session_id: SessionId,
    relative_artifact_path: &Path,
    compression: ArtifactCompression,
    minimum_saved_bytes: u64,
    mut check_cancelled: impl FnMut() -> io::Result<()>,
) -> io::Result<ArtifactStorageOutcome> {
    check_cancelled()?;
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic artifact exchange is unavailable",
        ));
    }
    if relative_artifact_path
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
        || relative_artifact_path.as_os_str().is_empty()
    {
        return Err(invalid());
    }
    let sessions_root = sessions_root.canonicalize()?;
    let session = confined(&sessions_root.join(session_id.to_string()), &sessions_root)?;
    if !fs::symlink_metadata(session.join("session.db"))?.is_file() {
        return Err(invalid());
    }
    let _maintenance = crate::lease::acquire_session_maintenance_guard(&sessions_root, session_id)
        .map_err(io::Error::other)?;
    let root = confined(
        &sessions_root
            .join("session-artifacts")
            .join(session_id.to_string()),
        &sessions_root,
    )?;
    let path = confined(&root.join(relative_artifact_path), &root)?;
    let parent = path.parent().ok_or_else(invalid)?;
    let name = path
        .file_name()
        .ok_or_else(invalid)?
        .to_str()
        .ok_or_else(invalid)?;
    let staging = parent.join(format!(".{name}.compression-pending"));
    let (mut original, encoding) = open_content(&path, &root)?;
    fs::create_dir(&staging)?;
    let result = (|| {
        let mut candidate = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(staging.join(PAYLOAD))?;
        let outcome = prepare_artifact_transition(
            &mut original,
            encoding,
            &mut candidate,
            compression,
            minimum_saved_bytes,
            &mut check_cancelled,
        )?;
        let Some(saved_bytes) = outcome.saved_bytes else {
            return Ok(None);
        };
        candidate.sync_all()?;
        File::open(&staging)?.sync_all()?;
        check_cancelled()?;
        Ok(Some(saved_bytes))
    })();
    let saved_bytes = match result {
        Ok(Some(saved_bytes)) => saved_bytes,
        Ok(None) => {
            remove_container(&staging)?;
            return Ok(ArtifactStorageOutcome::Unchanged);
        }
        Err(error) => {
            let _ = remove_container(&staging);
            return Err(error);
        }
    };
    exchange(&path, &staging)?;
    File::open(parent)?.sync_all()?;
    drop(original);
    let retained_backup = if remove_container(&staging).is_ok() {
        File::open(parent)?.sync_all()?;
        None
    } else {
        Some(staging)
    };
    Ok(ArtifactStorageOutcome::Compressed {
        saved_bytes,
        retained_backup,
    })
}

fn remove_container(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_file() {
        return fs::remove_file(path);
    }
    fs::remove_file(path.join(PAYLOAD))?;
    fs::remove_dir(path)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn exchange(left: &Path, right: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let left = std::ffi::CString::new(left.as_os_str().as_bytes()).map_err(|_| invalid())?;
    let right = std::ffi::CString::new(right.as_os_str().as_bytes()).map_err(|_| invalid())?;
    // SAFETY: both strings are valid NUL-terminated paths, and the atomic exchange operates only
    // on the already-confined same-parent paths while exclusive session maintenance is held.
    #[cfg(target_os = "macos")]
    let result = unsafe { libc::renamex_np(left.as_ptr(), right.as_ptr(), libc::RENAME_SWAP) };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn exchange(_left: &Path, _right: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic artifact exchange is unavailable",
    ))
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;

    #[test]
    fn publishes_at_same_path_and_reads_original_bytes() {
        let root = tempfile::tempdir().expect("root");
        let id = SessionId::new();
        let session = root.path().join(id.to_string());
        fs::create_dir(&session).expect("session");
        fs::write(session.join("session.db"), b"fixture").expect("database");
        let artifacts = root.path().join("session-artifacts").join(id.to_string());
        fs::create_dir_all(&artifacts).expect("artifacts");
        let bytes = "terminal 世界\n".repeat(100_000).into_bytes();
        let path = artifacts.join("recording.bin");
        fs::write(&path, &bytes).expect("original");
        let outcome = compress_session_artifact(
            root.path(),
            id,
            Path::new("recording.bin"),
            ArtifactCompression::Light,
            4096,
            || Ok(()),
        )
        .expect("compress");
        assert!(matches!(
            outcome,
            ArtifactStorageOutcome::Compressed {
                retained_backup: None,
                ..
            }
        ));
        assert!(path.is_dir());
        assert!(fs::read(&path).is_err());
        for offset in [0, 262_140, bytes.len() as u64 - 7] {
            let (total, actual) =
                read_artifact_range(&artifacts, &path, offset, 32).expect("range");
            assert_eq!(total, bytes.len() as u64);
            let start = usize::try_from(offset).expect("offset");
            assert_eq!(actual, bytes[start..bytes.len().min(start + 32)]);
        }
        let _ = compress_session_artifact(
            root.path(),
            id,
            Path::new("recording.bin"),
            ArtifactCompression::Deep,
            1,
            || Ok(()),
        )
        .expect("deep");
        assert_eq!(
            read_artifact_range(&artifacts, &path, 0, 32)
                .expect("deep read")
                .1,
            bytes[..32]
        );
        assert_eq!(
            fs::read(session.join("session.db")).expect("canonical"),
            b"fixture"
        );
    }

    #[test]
    fn cancellation_and_staging_residue_preserve_raw_authority() {
        let root = tempfile::tempdir().expect("root");
        let id = SessionId::new();
        let session = root.path().join(id.to_string());
        fs::create_dir(&session).expect("session");
        fs::write(session.join("session.db"), b"fixture").expect("database");
        let artifacts = root.path().join("session-artifacts").join(id.to_string());
        fs::create_dir_all(&artifacts).expect("artifacts");
        let path = artifacts.join("recording");
        let bytes = vec![42; 1_000_000];
        fs::write(&path, &bytes).expect("raw");
        let mut calls = 0;
        assert!(
            compress_session_artifact(
                root.path(),
                id,
                Path::new("recording"),
                ArtifactCompression::Light,
                1,
                || {
                    calls += 1;
                    if calls == 4 {
                        Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
                    } else {
                        Ok(())
                    }
                }
            )
            .is_err()
        );
        assert_eq!(fs::read(&path).expect("original"), bytes);
        let pending = artifacts.join(".recording.compression-pending");
        assert!(!pending.exists());
        fs::create_dir(&pending).expect("residue");
        fs::write(pending.join("unknown"), b"preserve").expect("unknown");
        assert!(
            compress_session_artifact(
                root.path(),
                id,
                Path::new("recording"),
                ArtifactCompression::Light,
                1,
                || Ok(())
            )
            .is_err()
        );
        assert_eq!(fs::read(&path).expect("original"), bytes);
        assert_eq!(
            fs::read(pending.join("unknown")).expect("preserved"),
            b"preserve"
        );
    }

    #[test]
    fn refuses_live_owner_and_unknown_container() {
        let root = tempfile::tempdir().expect("root");
        let id = SessionId::new();
        let session = root.path().join(id.to_string());
        fs::create_dir(&session).expect("session");
        fs::write(session.join("session.db"), b"fixture").expect("database");
        let _owner = crate::lease::acquire_session_lease(
            root.path(),
            id,
            &crate::lease::SessionLeaseOwnerContext::default(),
        )
        .expect("owner");
        assert!(
            compress_session_artifact(
                root.path(),
                id,
                Path::new("recording"),
                ArtifactCompression::Light,
                1,
                || Ok(())
            )
            .is_err()
        );
        let unknown = root.path().join("unknown");
        fs::create_dir(&unknown).expect("unknown");
        fs::write(unknown.join("content.v99.zstd"), b"future").expect("future");
        assert!(read_artifact_range(root.path(), &unknown, 0, 1).is_err());
    }
}
