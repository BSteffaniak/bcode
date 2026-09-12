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

#[cfg(unix)]
mod confined;

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

#[cfg(unix)]
fn open_content(path: &Path, root: &Path) -> io::Result<(File, ArtifactEncoding)> {
    let relative = path.strip_prefix(root).map_err(|_| invalid())?;
    let file = confined::open_relative(root, relative)?;
    let metadata = file.metadata()?;
    if metadata.is_file() {
        return Ok((file, ArtifactEncoding::Raw));
    }
    if !metadata.is_dir()
        || confined::container_names(&file)? != [std::ffi::OsString::from(PAYLOAD)]
    {
        return Err(invalid());
    }
    let payload = confined::open_child(&file, c"content.v1.zstd", false)?;
    if !payload.metadata()?.is_file() {
        return Err(invalid());
    }
    Ok((payload, ArtifactEncoding::ChunkedZstd))
}

#[cfg(not(unix))]
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
    let supplied_root = root;
    let root = root.canonicalize()?;
    let relative = path
        .strip_prefix(supplied_root)
        .or_else(|_| path.strip_prefix(&root))
        .map_err(|_| invalid())?;
    let path = root.join(relative);
    let (file, encoding) = open_content(&path, &root)?;
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

/// Return a bounded page of finalized artifact identities for offline maintenance.
///
/// # Errors
/// Fails for foreign ownership, unavailable storage, stale projections, or unsupported contracts.
pub async fn maintenance_candidates(
    root: &Path,
    session_id: SessionId,
    after: Option<(&str, &str)>,
) -> io::Result<Vec<(String, String)>> {
    let _maintenance = crate::lease::acquire_session_maintenance_guard(root, session_id)
        .map_err(io::Error::other)?;
    let db = crate::db::SessionDb::open_existing_turso_in_root(session_id, root)
        .await
        .map_err(io::Error::other)?;
    let result = db.artifact_maintenance_page(after).await;
    db.database().close().await.map_err(io::Error::other)?;
    result.map_err(io::Error::other)
}

/// Verify a finalized artifact reference and compress it under one maintenance fence.
///
/// Uses the current session database boundary, rejects stale projections and unsupported writer
/// contracts, and requires explicit completeness and a logical length matching the stored content.
/// Relative, invocation-capability, and historical local references use the same session-owned
/// resolver as application reads, followed by confinement beneath the owning artifact root.
///
/// # Errors
///
/// Returns an error for ownership, compatibility, projection, reference, length, codec or I/O
/// failures. No publication occurs when finalization cannot be verified.
pub async fn compress_finalized_artifact(
    sessions_root: &Path,
    session_id: SessionId,
    artifact_id: &str,
    reference_key: &str,
    compression: ArtifactCompression,
    minimum_saved_bytes: u64,
) -> io::Result<ArtifactStorageOutcome> {
    compress_finalized_artifact_with_age(
        sessions_root,
        session_id,
        artifact_id,
        reference_key,
        compression,
        minimum_saved_bytes,
        None,
    )
    .await
}

/// Compress a finalized reference only if its durable access age still meets the supplied policy.
///
/// # Errors
/// Returns an error for unknown/stale access evidence or any finalized maintenance failure.
#[allow(clippy::too_many_arguments)]
pub async fn compress_finalized_artifact_with_age(
    sessions_root: &Path,
    session_id: SessionId,
    artifact_id: &str,
    reference_key: &str,
    compression: ArtifactCompression,
    minimum_saved_bytes: u64,
    age: Option<(u64, u64)>,
) -> io::Result<ArtifactStorageOutcome> {
    let root = sessions_root.canonicalize()?;
    let session = confined(&root.join(session_id.to_string()), &root)?;
    if !fs::symlink_metadata(session.join("session.db"))?.is_file() {
        return Err(invalid());
    }
    let maintenance = crate::lease::acquire_session_maintenance_guard(&root, session_id)
        .map_err(io::Error::other)?;
    if let Some((now_ms, minimum_age_ms)) = age {
        let mut access = File::open(session.join("storage-access.bin"))?;
        let crate::storage_access::StorageAccessObservation::Recorded(record) =
            crate::storage_access::observe_access(&mut access)?
        else {
            return Err(invalid());
        };
        if now_ms
            .checked_sub(record.observed_at_ms)
            .is_none_or(|elapsed| elapsed < minimum_age_ms)
        {
            return Ok(ArtifactStorageOutcome::Unchanged);
        }
    }
    let db = crate::db::SessionDb::open_existing_turso_in_root(session_id, &root)
        .await
        .map_err(io::Error::other)?;
    let reference_result = db
        .finalized_artifact_reference(artifact_id, reference_key)
        .await;
    let finalized_age = if age.is_some() {
        match &reference_result {
            Ok(Some(reference)) => Some(
                db.artifact_finalized_at_ms(reference.finalized_event_seq)
                    .await,
            ),
            _ => None,
        }
    } else {
        None
    };
    let close_result = db.database().close().await;
    close_result.map_err(io::Error::other)?;
    if let (Some((now_ms, minimum_age_ms)), Some(timestamp)) = (age, finalized_age) {
        let timestamp = timestamp.map_err(io::Error::other)?;
        if now_ms
            .checked_sub(timestamp)
            .is_none_or(|elapsed| elapsed < minimum_age_ms)
        {
            return Ok(ArtifactStorageOutcome::Unchanged);
        }
    }
    let reference = reference_result
        .map_err(io::Error::other)?
        .ok_or_else(invalid)?;
    if reference.complete != Some(true) || reference.availability.as_deref() != Some("complete") {
        return Err(invalid());
    }
    let artifacts = confined(
        &root.join("session-artifacts").join(session_id.to_string()),
        &root,
    )?;
    let resolved = crate::artifact_reference::resolve_artifact_reference(
        &reference.storage_uri.ok_or_else(invalid)?,
        &artifacts,
    )
    .map_err(|_| invalid())?;
    let resolved = confined(&resolved, &artifacts)?;
    let relative = resolved
        .strip_prefix(&artifacts)
        .map_err(|_| invalid())?
        .to_path_buf();
    let expected_bytes = reference.byte_len.ok_or_else(invalid)?;
    tokio::task::spawn_blocking(move || {
        let artifacts = confined(
            &root.join("session-artifacts").join(session_id.to_string()),
            &root,
        )?;
        let (file, encoding) = open_content(&artifacts.join(&relative), &artifacts)?;
        let reader = ArtifactReader::new(file, encoding)?;
        if reader.logical_bytes() != expected_bytes {
            return Err(invalid());
        }
        drop(reader);
        compress_artifact_with_maintenance(
            &root,
            session_id,
            &relative,
            compression,
            minimum_saved_bytes,
            || Ok(()),
            maintenance,
        )
    })
    .await
    .map_err(|_| io::Error::other("artifact maintenance task failed"))?
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
    let maintenance = crate::lease::acquire_session_maintenance_guard(&sessions_root, session_id)
        .map_err(io::Error::other)?;
    compress_artifact_with_maintenance(
        &sessions_root,
        session_id,
        relative_artifact_path,
        compression,
        minimum_saved_bytes,
        check_cancelled,
        maintenance,
    )
}

fn compress_artifact_with_maintenance(
    sessions_root: &Path,
    session_id: SessionId,
    relative_artifact_path: &Path,
    compression: ArtifactCompression,
    minimum_saved_bytes: u64,
    mut check_cancelled: impl FnMut() -> io::Result<()>,
    _maintenance: crate::lease::SessionMaintenanceGuard,
) -> io::Result<ArtifactStorageOutcome> {
    let root = confined(
        &sessions_root
            .join("session-artifacts")
            .join(session_id.to_string()),
        sessions_root,
    )?;
    let path = confined(&root.join(relative_artifact_path), &root)?;
    let parent = path.parent().ok_or_else(invalid)?;
    let parent_handle = File::open(parent)?;
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
    #[cfg(test)]
    crash_boundary("prepared");
    exchange(&parent_handle, parent, &path, &staging)?;
    #[cfg(test)]
    crash_boundary("exchanged");
    parent_handle.sync_all()?;
    #[cfg(test)]
    crash_boundary("committed");
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

#[cfg(test)]
fn crash_boundary(phase: &str) {
    if std::env::var("BCODE_ARTIFACT_CRASH_PHASE").as_deref() == Ok(phase) {
        // Exit without unwinding: neither local cleanup nor lease destructors may run.
        std::process::exit(91);
    }
}

fn remove_container(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_file() {
        return fs::remove_file(path);
    }
    fs::remove_file(path.join(PAYLOAD))?;
    fs::remove_dir(path)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn exchange(parent: &File, parent_path: &Path, left: &Path, right: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;
    let pinned = parent.metadata()?;
    let current = fs::symlink_metadata(parent_path)?;
    if !current.is_dir()
        || pinned.dev() != current.dev()
        || pinned.ino() != current.ino()
        || left.parent() != Some(parent_path)
        || right.parent() != Some(parent_path)
    {
        return Err(invalid());
    }
    let left = std::ffi::CString::new(left.file_name().ok_or_else(invalid)?.as_bytes())
        .map_err(|_| invalid())?;
    let right = std::ffi::CString::new(right.file_name().ok_or_else(invalid)?.as_bytes())
        .map_err(|_| invalid())?;
    // SAFETY: names are single NUL-terminated components and parent is a pinned directory handle.
    // Neither syscall traverses a tool-controlled parent path during atomic exchange.
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            left.as_ptr(),
            parent.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            parent.as_raw_fd(),
            left.as_ptr(),
            parent.as_raw_fd(),
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
fn exchange(_parent: &File, _parent_path: &Path, _left: &Path, _right: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic artifact exchange is unavailable",
    ))
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;

    #[test]
    fn publication_rejects_replaced_parent_without_touching_replacement() {
        let root = tempfile::tempdir().expect("root");
        let parent = root.path().join("parent");
        fs::create_dir(&parent).expect("parent");
        fs::write(parent.join("raw"), b"original").expect("raw");
        fs::create_dir(parent.join("candidate")).expect("candidate");
        let pinned = File::open(&parent).expect("pin");
        fs::rename(&parent, root.path().join("moved")).expect("move parent");
        fs::create_dir(&parent).expect("replacement");
        fs::write(parent.join("raw"), b"replacement").expect("replacement raw");
        fs::create_dir(parent.join("candidate")).expect("replacement candidate");
        assert!(
            exchange(
                &pinned,
                &parent,
                &parent.join("raw"),
                &parent.join("candidate")
            )
            .is_err()
        );
        assert_eq!(
            fs::read(parent.join("raw")).expect("untouched replacement"),
            b"replacement"
        );
        assert_eq!(
            fs::read(root.path().join("moved/raw")).expect("untouched original"),
            b"original"
        );
    }

    #[tokio::test]
    async fn verified_maintenance_rejects_corrupt_canonical_storage_without_publication() {
        let root = tempfile::tempdir().expect("root");
        let id = SessionId::new();
        let session = root.path().join(id.to_string());
        fs::create_dir(&session).expect("session");
        fs::write(session.join("session.db"), b"not a database").expect("database");
        let artifacts = root.path().join("session-artifacts").join(id.to_string());
        fs::create_dir_all(&artifacts).expect("artifacts");
        let bytes = vec![42; 100_000];
        fs::write(artifacts.join("recording"), &bytes).expect("raw");
        assert!(
            compress_finalized_artifact(
                root.path(),
                id,
                "artifact",
                "recording",
                ArtifactCompression::Light,
                1
            )
            .await
            .is_err()
        );
        assert_eq!(
            fs::read(artifacts.join("recording")).expect("unchanged"),
            bytes
        );
        assert!(!artifacts.join(".recording.compression-pending").exists());
    }

    #[test]
    fn crash_child() {
        let Ok(root) = std::env::var("BCODE_ARTIFACT_CRASH_ROOT") else {
            return;
        };
        let id: SessionId = std::env::var("BCODE_ARTIFACT_CRASH_SESSION")
            .expect("session")
            .parse()
            .expect("id");
        compress_session_artifact(
            Path::new(&root),
            id,
            Path::new("recording"),
            ArtifactCompression::Light,
            1,
            || Ok(()),
        )
        .expect("child compression");
        panic!("crash boundary was not reached");
    }

    #[test]
    fn process_crashes_keep_exactly_one_logical_authority() {
        for phase in ["prepared", "exchanged", "committed"] {
            let root = tempfile::tempdir().expect("root");
            let id = SessionId::new();
            let session = root.path().join(id.to_string());
            fs::create_dir(&session).expect("session");
            fs::write(session.join("session.db"), b"fixture").expect("database");
            let artifacts = root.path().join("session-artifacts").join(id.to_string());
            fs::create_dir_all(&artifacts).expect("artifacts");
            let bytes = "terminal crash fixture 世界\n".repeat(40_000).into_bytes();
            let path = artifacts.join("recording");
            fs::write(&path, &bytes).expect("original");
            let status =
                std::process::Command::new(std::env::current_exe().expect("test executable"))
                    .args([
                        "--exact",
                        "artifact_storage::tests::crash_child",
                        "--nocapture",
                    ])
                    .env("BCODE_ARTIFACT_CRASH_ROOT", root.path())
                    .env("BCODE_ARTIFACT_CRASH_SESSION", id.to_string())
                    .env("BCODE_ARTIFACT_CRASH_PHASE", phase)
                    .status()
                    .expect("child");
            assert_eq!(status.code(), Some(91), "{phase}");
            assert_eq!(path.is_file(), phase == "prepared");
            let pending = artifacts.join(".recording.compression-pending");
            assert!(pending.exists());
            let mut actual = Vec::new();
            let mut offset = 0;
            while offset < bytes.len() as u64 {
                let (_, block) = read_artifact_range(&artifacts, &path, offset, 65536)
                    .expect("authoritative read");
                offset += block.len() as u64;
                actual.extend(block);
            }
            assert_eq!(actual, bytes, "{phase}");
            // Process death released maintenance ownership, but residue blocks a new conversion.
            assert!(crate::lease::acquire_session_maintenance_guard(root.path(), id).is_ok());
            assert!(
                compress_session_artifact(
                    root.path(),
                    id,
                    Path::new("recording"),
                    ArtifactCompression::Deep,
                    1,
                    || Ok(())
                )
                .is_err()
            );
            assert!(pending.exists());
            assert_eq!(
                fs::read(session.join("session.db")).expect("database unchanged"),
                b"fixture"
            );
        }
    }

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
