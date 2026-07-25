use bcode_session_models::{
    SessionId, SessionMigrationProgress, SessionMigrationProgressUnit, SessionMigrationStage,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, ErrorKind, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

const BACKUP_BUFFER_BYTES: usize = 64 * 1024;

/// Progress callback for retained migration-backup planning, copying, and verification.
pub type BackupProgressCallback = Arc<dyn Fn(SessionMigrationProgress) + Send + Sync>;

/// Metadata written alongside a verified retained migration backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrationBackupManifest {
    /// Session identifier whose physical store was retained.
    pub session_id: SessionId,
    /// Writer epoch observed before migration.
    pub source_writer_epoch: u64,
    /// Writer epoch the migration intends to produce.
    pub target_writer_epoch: u64,
    /// Backup creation time as Unix epoch milliseconds.
    pub created_at_ms: u64,
}

/// Request to create a verified retained migration backup using the standard storage layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationBackupRequest {
    /// Canonical sessions storage root.
    pub sessions_root: PathBuf,
    /// Session identifier and canonical directory name.
    pub session_id: SessionId,
    /// Writer epoch observed before migration.
    pub source_writer_epoch: u64,
    /// Writer epoch the migration intends to produce.
    pub target_writer_epoch: u64,
}

/// Timing, size, and retained path for one verified migration backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedMigrationBackup {
    /// Retained backup directory.
    pub path: PathBuf,
    /// Number of copied source files.
    pub files: u64,
    /// Total copied source bytes.
    pub bytes: u64,
    /// Time spent enumerating source files.
    pub plan_duration: Duration,
    /// Time spent streaming and hashing source files.
    pub copy_duration: Duration,
    /// Time spent verifying destination lengths and hashes.
    pub verify_duration: Duration,
}

/// Timing and size measurements for one verified retained backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedMigrationBackup {
    /// Number of copied source files.
    pub files: u64,
    /// Total copied source bytes.
    pub bytes: u64,
    /// Time spent enumerating source files.
    pub plan_duration: Duration,
    /// Time spent streaming and hashing source files.
    pub copy_duration: Duration,
    /// Time spent verifying destination lengths and hashes.
    pub verify_duration: Duration,
}

/// Failure to create a verified retained migration backup.
#[derive(Debug, Error)]
pub enum MigrationBackupError {
    /// The system clock is earlier than the Unix epoch.
    #[error("system clock is earlier than the Unix epoch")]
    Clock,
    /// The backup manifest could not be serialized.
    #[error("backup manifest serialization failed: {0}")]
    Manifest(#[from] serde_json::Error),
    /// The blocking backup worker failed to join.
    #[error("backup worker failed: {0}")]
    Worker(#[from] tokio::task::JoinError),
    /// Filesystem backup work failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackupCopyFault {
    None,
    #[cfg(test)]
    PermissionDenied,
    #[cfg(test)]
    ShortWriteAfter(u64),
}

/// Create a retained physical backup using the canonical session-backup layout and manifest.
///
/// # Errors
///
/// Returns an error if the clock is invalid, manifest serialization fails, the source cannot be
/// copied, the destination conflicts, destination verification fails, or the worker cannot run.
pub async fn create_retained_migration_backup(
    request: MigrationBackupRequest,
    progress: Option<BackupProgressCallback>,
) -> Result<RetainedMigrationBackup, MigrationBackupError> {
    let created_at_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| MigrationBackupError::Clock)?
            .as_millis(),
    )
    .unwrap_or(u64::MAX);
    let source = request.sessions_root.join(request.session_id.to_string());
    let destination = request
        .sessions_root
        .parent()
        .unwrap_or(&request.sessions_root)
        .join("session-migration-backups")
        .join(format!(
            "{}-{}-epoch-{}",
            created_at_ms, request.session_id, request.source_writer_epoch
        ));
    let manifest = serde_json::to_vec_pretty(&MigrationBackupManifest {
        session_id: request.session_id,
        source_writer_epoch: request.source_writer_epoch,
        target_writer_epoch: request.target_writer_epoch,
        created_at_ms,
    })?;
    let result =
        create_verified_migration_backup(&source, &destination, &manifest, progress).await?;
    Ok(RetainedMigrationBackup {
        path: destination,
        files: result.files,
        bytes: result.bytes,
        plan_duration: result.plan_duration,
        copy_duration: result.copy_duration,
        verify_duration: result.verify_duration,
    })
}

/// Create and hash-verify a retained physical migration backup off the async runtime worker.
///
/// # Errors
///
/// Returns an error if the blocking worker cannot run, the destination already exists, a source
/// file cannot be copied, the destination does not match the source, or the manifest cannot be
/// written. Incomplete destinations are removed on failure.
pub async fn create_verified_migration_backup(
    source: &Path,
    destination: &Path,
    manifest: &[u8],
    progress: Option<BackupProgressCallback>,
) -> Result<VerifiedMigrationBackup, MigrationBackupError> {
    let source = source.to_path_buf();
    let destination = destination.to_path_buf();
    let manifest = manifest.to_vec();
    Ok(tokio::task::spawn_blocking(move || {
        create_verified_migration_backup_blocking(
            &source,
            &destination,
            &manifest,
            BackupCopyFault::None,
            progress.as_ref(),
        )
    })
    .await??)
}

fn publish_progress(
    progress: Option<&BackupProgressCallback>,
    stage: SessionMigrationStage,
    completed: u64,
    total: u64,
    unit: SessionMigrationProgressUnit,
    message: &str,
) {
    if let Some(progress) = progress {
        progress(SessionMigrationProgress {
            stage,
            completed_units: Some(completed),
            total_units: Some(total),
            unit: Some(unit),
            message: message.to_owned(),
        });
    }
}

fn create_verified_migration_backup_blocking(
    source: &Path,
    destination: &Path,
    manifest: &[u8],
    fault: BackupCopyFault,
    progress: Option<&BackupProgressCallback>,
) -> std::io::Result<VerifiedMigrationBackup> {
    if destination.exists() {
        return Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            format!(
                "backup destination already exists: {}",
                destination.display()
            ),
        ));
    }
    let result = (|| {
        let started = Instant::now();
        let files = migration_backup_files(source, source)?;
        let plan_duration = started.elapsed();
        let bytes = files
            .iter()
            .fold(0_u64, |total, file| total.saturating_add(file.bytes));
        let file_count = u64::try_from(files.len()).unwrap_or(u64::MAX);
        publish_progress(
            progress,
            SessionMigrationStage::PlanningBackup,
            file_count,
            file_count,
            SessionMigrationProgressUnit::Files,
            "Planned retained backup",
        );
        publish_progress(
            progress,
            SessionMigrationStage::CopyingBackup,
            0,
            bytes,
            SessionMigrationProgressUnit::Bytes,
            "Copying retained backup",
        );
        fs::create_dir_all(destination)?;
        let started = Instant::now();
        let source_hashes =
            copy_and_hash_backup_files(source, destination, &files, fault, progress, bytes)?;
        let copy_duration = started.elapsed();
        publish_progress(
            progress,
            SessionMigrationStage::VerifyingBackup,
            0,
            bytes,
            SessionMigrationProgressUnit::Bytes,
            "Verifying retained backup",
        );
        let started = Instant::now();
        verify_backup_files(destination, &files, &source_hashes, progress, bytes)?;
        let verify_duration = started.elapsed();
        fs::write(destination.join("migration-backup.json"), manifest)?;
        Ok(VerifiedMigrationBackup {
            files: file_count,
            bytes,
            plan_duration,
            copy_duration,
            verify_duration,
        })
    })();
    if result.is_err() && destination.exists() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

#[derive(Debug)]
struct BackupFile {
    relative_path: PathBuf,
    bytes: u64,
}

fn migration_backup_files(root: &Path, directory: &Path) -> std::io::Result<Vec<BackupFile>> {
    let mut files = Vec::new();
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            files.extend(migration_backup_files(root, &path)?);
        } else if metadata.is_file() {
            let relative_path = path
                .strip_prefix(root)
                .map_err(|error| std::io::Error::other(error.to_string()))?
                .to_path_buf();
            if relative_path == Path::new("migration-backup.json") {
                return Err(std::io::Error::new(
                    ErrorKind::AlreadyExists,
                    "source contains reserved migration-backup.json path",
                ));
            }
            files.push(BackupFile {
                relative_path,
                bytes: metadata.len(),
            });
        }
    }
    Ok(files)
}

fn copy_and_hash_backup_files(
    source: &Path,
    destination: &Path,
    files: &[BackupFile],
    fault: BackupCopyFault,
    progress: Option<&BackupProgressCallback>,
    total_bytes: u64,
) -> std::io::Result<BTreeMap<PathBuf, [u8; 32]>> {
    #[cfg(not(test))]
    let _ = fault;
    #[cfg(test)]
    if fault == BackupCopyFault::PermissionDenied {
        return Err(std::io::Error::new(
            ErrorKind::PermissionDenied,
            "injected backup permission failure",
        ));
    }
    let mut total_written = 0_u64;
    let mut source_digests = BTreeMap::new();
    for file in files {
        let source_path = source.join(&file.relative_path);
        let destination_path = destination.join(&file.relative_path);
        fs::create_dir_all(
            destination_path
                .parent()
                .ok_or_else(|| std::io::Error::other("backup destination file has no parent"))?,
        )?;
        let mut reader = BufReader::with_capacity(BACKUP_BUFFER_BYTES, File::open(&source_path)?);
        let destination_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination_path)?;
        let mut writer = BufWriter::with_capacity(BACKUP_BUFFER_BYTES, destination_file);
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; BACKUP_BUFFER_BYTES];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            #[cfg(test)]
            if let BackupCopyFault::ShortWriteAfter(limit) = fault
                && total_written.saturating_add(u64::try_from(read).unwrap_or(u64::MAX)) > limit
            {
                let allowed = usize::try_from(limit.saturating_sub(total_written))
                    .unwrap_or(usize::MAX)
                    .min(read);
                if allowed > 0 {
                    writer.write_all(&buffer[..allowed])?;
                }
                writer.flush()?;
                return Err(std::io::Error::new(
                    ErrorKind::WriteZero,
                    "injected short backup write",
                ));
            }
            writer.write_all(&buffer[..read])?;
            total_written = total_written.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            publish_progress(
                progress,
                SessionMigrationStage::CopyingBackup,
                total_written,
                total_bytes,
                SessionMigrationProgressUnit::Bytes,
                "Copying retained backup",
            );
            hasher.update(&buffer[..read]);
        }
        writer.flush()?;
        fs::set_permissions(&destination_path, fs::metadata(&source_path)?.permissions())?;
        source_digests.insert(file.relative_path.clone(), hasher.finalize().into());
    }
    Ok(source_digests)
}

fn verify_backup_files(
    destination: &Path,
    files: &[BackupFile],
    source_hashes: &BTreeMap<PathBuf, [u8; 32]>,
    progress: Option<&BackupProgressCallback>,
    total_bytes: u64,
) -> std::io::Result<()> {
    let mut verified_bytes = 0_u64;
    for file in files {
        let destination_path = destination.join(&file.relative_path);
        if fs::metadata(&destination_path)?.len() != file.bytes {
            return Err(std::io::Error::other(format!(
                "backup length verification failed for {}",
                file.relative_path.display()
            )));
        }
        let actual = hash_file(&destination_path)?;
        if source_hashes.get(&file.relative_path) != Some(&actual) {
            return Err(std::io::Error::other(format!(
                "backup hash verification failed for {}",
                file.relative_path.display()
            )));
        }
        verified_bytes = verified_bytes.saturating_add(file.bytes);
        publish_progress(
            progress,
            SessionMigrationStage::VerifyingBackup,
            verified_bytes,
            total_bytes,
            SessionMigrationProgressUnit::Bytes,
            "Verifying retained backup",
        );
    }
    Ok(())
}

fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut reader = BufReader::with_capacity(BACKUP_BUFFER_BYTES, File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; BACKUP_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retained_backup_owns_layout_and_manifest_policy() {
        let temp = tempfile::tempdir().expect("temp dir");
        let sessions_root = temp.path().join("sessions");
        let session_id = SessionId::new();
        let source = sessions_root.join(session_id.to_string());
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("session.db"), b"canonical").expect("database");

        let result = create_retained_migration_backup(
            MigrationBackupRequest {
                sessions_root,
                session_id,
                source_writer_epoch: 2,
                target_writer_epoch: 5,
            },
            None,
        )
        .await
        .expect("retained backup");

        assert_eq!(
            fs::read(result.path.join("session.db")).expect("database"),
            b"canonical"
        );
        assert_eq!(
            result.path.parent(),
            Some(temp.path().join("session-migration-backups").as_path())
        );
        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(result.path.join("migration-backup.json")).expect("manifest"),
        )
        .expect("manifest json");
        assert_eq!(manifest["session_id"], session_id.to_string());
        assert_eq!(manifest["source_writer_epoch"], 2);
        assert_eq!(manifest["target_writer_epoch"], 5);
        assert!(manifest["created_at_ms"].as_u64().is_some());
    }

    #[test]
    fn streaming_backup_handles_nested_empty_and_large_files() {
        let temp = tempfile::tempdir().expect("temp dir");
        let source = temp.path().join("source");
        let destination = temp.path().join("backup");
        fs::create_dir_all(source.join("nested")).expect("nested source");
        fs::write(source.join("empty"), []).expect("empty file");
        fs::write(source.join("nested/small"), b"small").expect("small file");
        let large = vec![0x5a; BACKUP_BUFFER_BYTES * 3 + 17];
        fs::write(source.join("large"), &large).expect("large file");
        let result = create_verified_migration_backup_blocking(
            &source,
            &destination,
            br#"{"manifest":true}"#,
            BackupCopyFault::None,
            None,
        )
        .expect("backup");
        assert_eq!(result.files, 3);
        assert_eq!(result.bytes, u64::try_from(large.len() + 5).expect("bytes"));
        assert_eq!(fs::read(destination.join("large")).expect("large"), large);
        assert!(
            fs::read(destination.join("empty"))
                .expect("empty")
                .is_empty()
        );
    }

    #[test]
    fn backup_refuses_conflicts_corruption_and_reserved_manifest() {
        let temp = tempfile::tempdir().expect("temp dir");
        let source = temp.path().join("source");
        let destination = temp.path().join("backup");
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("file"), b"content").expect("file");
        fs::create_dir_all(&destination).expect("conflict");
        assert_eq!(
            create_verified_migration_backup_blocking(
                &source,
                &destination,
                b"manifest",
                BackupCopyFault::None,
                None,
            )
            .expect_err("conflict")
            .kind(),
            ErrorKind::AlreadyExists
        );
        fs::remove_dir_all(&destination).expect("remove conflict");
        fs::create_dir(source.join("migration-backup.json")).expect("reserved directory");
        fs::write(source.join("migration-backup.json/child"), b"child").expect("child");
        assert!(
            create_verified_migration_backup_blocking(
                &source,
                &destination,
                b"manifest",
                BackupCopyFault::None,
                None,
            )
            .is_err()
        );
        assert!(!destination.exists());
    }

    #[test]
    fn backup_faults_are_deterministic_and_cleanup_partial_output() {
        let temp = tempfile::tempdir().expect("temp dir");
        let source = temp.path().join("source");
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("large"), vec![0x7f; BACKUP_BUFFER_BYTES * 2]).expect("file");
        for (index, fault, expected) in [
            (
                0,
                BackupCopyFault::PermissionDenied,
                ErrorKind::PermissionDenied,
            ),
            (
                1,
                BackupCopyFault::ShortWriteAfter(17),
                ErrorKind::WriteZero,
            ),
        ] {
            let destination = temp.path().join(format!("backup-{index}"));
            let error = create_verified_migration_backup_blocking(
                &source,
                &destination,
                b"manifest",
                fault,
                None,
            )
            .expect_err("fault");
            assert_eq!(error.kind(), expected);
            assert!(!destination.exists());
        }
    }
}
