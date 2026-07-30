use bcode_session_models::{
    SessionId, SessionMigrationProgress, SessionMigrationProgressUnit, SessionMigrationStage,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, ErrorKind, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

const BACKUP_BUFFER_BYTES: usize = 64 * 1024;
const MIGRATION_BACKUP_MANIFEST_FILE: &str = "migration-backup.json";
const MIGRATION_BACKUPS_DIRECTORY: &str = "session-migration-backups";

/// Progress callback for retained migration-backup planning, copying, and verification.
pub type BackupProgressCallback = Arc<dyn Fn(SessionMigrationProgress) + Send + Sync>;

/// Length and digest evidence for one physical database file retained in a migration backup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationBackupFileEvidence {
    /// Source file length in bytes.
    pub bytes: u64,
    /// Lowercase SHA-256 digest of the source bytes.
    pub sha256: String,
}

/// Canonical source-history evidence captured before migration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationBackupCanonicalEvidence {
    /// Decode/classification coverage captured before backup. This equals `event_count` for a
    /// migratable source; a smaller value records where damaged or unsupported history failed.
    pub classified_event_count: u64,
    /// Canonical source event count.
    pub event_count: u64,
    /// Canonical source tail, if the session contains events.
    pub event_tail: Option<u64>,
    /// Digest over ordered canonical source payloads.
    pub payload_digest_sha256: String,
}

/// Metadata written alongside a verified retained migration backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationBackupManifest {
    /// Session identifier whose physical store was retained.
    pub session_id: SessionId,
    /// Stable migration operation identity.
    #[serde(default)]
    pub operation_id: String,
    /// Writer epoch observed before migration.
    pub source_writer_epoch: u64,
    /// Writer epoch the migration intends to produce.
    pub target_writer_epoch: u64,
    /// Ordered monotonic migration steps selected for this source.
    #[serde(default)]
    pub migration_step_ids: Vec<String>,
    /// Canonical source-history evidence captured before backup.
    #[serde(default)]
    pub canonical_source: MigrationBackupCanonicalEvidence,
    /// Converted event counts keyed by `schema:kind`.
    #[serde(default)]
    pub converted_events: BTreeMap<String, u64>,
    /// Retired-known event counts keyed by `schema:kind`.
    #[serde(default)]
    pub retired_known_events: BTreeMap<String, u64>,
    /// Main database file evidence.
    #[serde(default)]
    pub database: MigrationBackupFileEvidence,
    /// WAL sidecar evidence when the source has a WAL.
    #[serde(default)]
    pub wal: Option<MigrationBackupFileEvidence>,
    /// shared-memory sidecar evidence when the source has one.
    #[serde(default)]
    pub shm: Option<MigrationBackupFileEvidence>,
    /// Backup creation time as Unix epoch milliseconds.
    pub created_at_ms: u64,
    /// Time at which all retained files passed length and digest verification.
    #[serde(default)]
    pub verified_at_ms: u64,
}

/// Inputs used to build a retained backup request from migration-owned writer policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationBackupRequestPlan {
    /// Canonical sessions storage root.
    pub sessions_root: PathBuf,
    /// Session identifier and canonical directory name.
    pub session_id: SessionId,
    /// Stable migration operation identity.
    pub operation_id: String,
    /// Writer epoch observed before migration.
    pub source_writer_epoch: u64,
    /// Canonical source-history evidence captured before backup.
    pub canonical_source: MigrationBackupCanonicalEvidence,
    /// Converted event counts keyed by `schema:kind`.
    pub converted_events: BTreeMap<String, u64>,
    /// Retired-known event counts keyed by `schema:kind`.
    pub retired_known_events: BTreeMap<String, u64>,
}

/// Failure to build a retained backup request from the observed source writer.
#[derive(Debug, Error)]
pub enum MigrationBackupRequestError {
    /// The persisted writer epoch cannot be represented by the migration inventory.
    #[error("source writer epoch {0} cannot be represented")]
    WriterEpochOutOfRange(u64),
    /// No safe monotonic migration plan reaches the current writer.
    #[error(transparent)]
    Plan(#[from] crate::MigrationPlanError),
}

/// Build a retained backup request with migration-owned target epoch and ordered step identities.
///
/// # Errors
///
/// Returns an error when the source epoch cannot be represented or has no migration path.
pub fn build_migration_backup_request(
    input: MigrationBackupRequestPlan,
) -> Result<MigrationBackupRequest, MigrationBackupRequestError> {
    let source_writer_epoch = u32::try_from(input.source_writer_epoch).map_err(|_| {
        MigrationBackupRequestError::WriterEpochOutOfRange(input.source_writer_epoch)
    })?;
    let plan = crate::plan_writer_epoch_migration(source_writer_epoch)?;
    Ok(MigrationBackupRequest {
        sessions_root: input.sessions_root,
        session_id: input.session_id,
        operation_id: input.operation_id,
        source_writer_epoch: input.source_writer_epoch,
        target_writer_epoch: u64::from(crate::CURRENT_WRITER_EPOCH),
        migration_step_ids: plan.steps.iter().map(|step| step.id.to_owned()).collect(),
        canonical_source: input.canonical_source,
        converted_events: input.converted_events,
        retired_known_events: input.retired_known_events,
    })
}

/// Canonical source-history evidence and classification outcome collected by the current store.
pub struct MigrationSourceEvidence<E> {
    /// Canonical source facts used to verify a retained backup.
    pub canonical: MigrationBackupCanonicalEvidence,
    /// Converted event counts keyed by `schema:kind`.
    pub converted_events: BTreeMap<String, u64>,
    /// Retired-known event counts keyed by `schema:kind`.
    pub retired_known_events: BTreeMap<String, u64>,
    /// First classification failure, when the source requires repair.
    pub classification_error: Option<E>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationBackupRequest {
    /// Canonical sessions storage root.
    pub sessions_root: PathBuf,
    /// Session identifier and canonical directory name.
    pub session_id: SessionId,
    /// Stable migration operation identity.
    pub operation_id: String,
    /// Writer epoch observed before migration.
    pub source_writer_epoch: u64,
    /// Writer epoch the migration intends to produce.
    pub target_writer_epoch: u64,
    /// Ordered monotonic migration steps selected for this source.
    pub migration_step_ids: Vec<String>,
    /// Canonical source-history evidence captured before backup.
    pub canonical_source: MigrationBackupCanonicalEvidence,
    /// Converted event counts keyed by `schema:kind`.
    pub converted_events: BTreeMap<String, u64>,
    /// Retired-known event counts keyed by `schema:kind`.
    pub retired_known_events: BTreeMap<String, u64>,
}

/// A retained migration backup discovered for diagnosis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetainedMigrationBackupDiagnosis {
    /// Retained backup directory.
    pub path: PathBuf,
    /// Parsed retained-backup manifest.
    pub manifest: MigrationBackupManifest,
}

/// Discover the newest retained migration backup for one session without mutation.
///
/// Malformed manifests and directories for other sessions are ignored. Discovery reads only
/// bounded manifest sidecars and never opens or hashes retained databases.
///
/// # Errors
///
/// Returns an error when the backup root or a candidate manifest cannot be read for reasons other
/// than absence or malformed JSON.
pub fn latest_retained_migration_backup(
    sessions_root: &Path,
    session_id: SessionId,
) -> Result<Option<RetainedMigrationBackupDiagnosis>, MigrationBackupError> {
    let root = sessions_root
        .parent()
        .unwrap_or(sessions_root)
        .join(MIGRATION_BACKUPS_DIRECTORY);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut newest = None;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        let manifest = match fs::read(path.join(MIGRATION_BACKUP_MANIFEST_FILE)) {
            Ok(manifest) => manifest,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let Ok(manifest) = serde_json::from_slice::<MigrationBackupManifest>(&manifest) else {
            continue;
        };
        if manifest.session_id != session_id {
            continue;
        }
        let candidate = RetainedMigrationBackupDiagnosis { path, manifest };
        if newest
            .as_ref()
            .is_none_or(|current: &RetainedMigrationBackupDiagnosis| {
                candidate.manifest.created_at_ms > current.manifest.created_at_ms
            })
        {
            newest = Some(candidate);
        }
    }
    Ok(newest)
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

fn abort_at_backup_crash_boundary(boundary: &str) {
    if std::env::var("BCODE_MIGRATION_BACKUP_CRASH_PHASE").as_deref() == Ok(boundary) {
        std::process::abort();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackupCopyFault {
    None,
    #[cfg(test)]
    PermissionDenied,
    #[cfg(test)]
    ShortWriteAfter(u64),
}

#[derive(Debug)]
struct VerifiedBackupEvidence {
    result: VerifiedMigrationBackup,
    files: BTreeMap<PathBuf, MigrationBackupFileEvidence>,
    verified_at_ms: u64,
}

async fn write_backup_manifest(
    destination: &Path,
    manifest: Vec<u8>,
) -> Result<(), MigrationBackupError> {
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let result = fs::write(destination.join(MIGRATION_BACKUP_MANIFEST_FILE), manifest);
        if result.is_err() && destination.exists() {
            let _ = fs::remove_dir_all(&destination);
        }
        result
    })
    .await??;
    Ok(())
}

fn current_unix_timestamp_ms() -> Result<u64, MigrationBackupError> {
    Ok(u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| MigrationBackupError::Clock)?
            .as_millis(),
    )
    .unwrap_or(u64::MAX))
}

fn required_file_evidence(
    files: &BTreeMap<PathBuf, MigrationBackupFileEvidence>,
    relative_path: &str,
) -> Result<MigrationBackupFileEvidence, MigrationBackupError> {
    files.get(Path::new(relative_path)).cloned().ok_or_else(|| {
        MigrationBackupError::Io(std::io::Error::new(
            ErrorKind::NotFound,
            format!("migration source is missing required {relative_path}"),
        ))
    })
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
    let created_at_ms = current_unix_timestamp_ms()?;
    let source = request.sessions_root.join(request.session_id.to_string());
    let destination = request
        .sessions_root
        .parent()
        .unwrap_or(&request.sessions_root)
        .join(MIGRATION_BACKUPS_DIRECTORY)
        .join(format!(
            "{}-{}-epoch-{}",
            created_at_ms, request.session_id, request.source_writer_epoch
        ));
    let evidence =
        create_verified_migration_backup_evidence(&source, &destination, progress).await?;
    let manifest = MigrationBackupManifest {
        session_id: request.session_id,
        operation_id: request.operation_id,
        source_writer_epoch: request.source_writer_epoch,
        target_writer_epoch: request.target_writer_epoch,
        migration_step_ids: request.migration_step_ids,
        canonical_source: request.canonical_source,
        converted_events: request.converted_events,
        retired_known_events: request.retired_known_events,
        database: required_file_evidence(&evidence.files, "session.db")?,
        wal: evidence.files.get(Path::new("session.db-wal")).cloned(),
        shm: evidence.files.get(Path::new("session.db-shm")).cloned(),
        created_at_ms,
        verified_at_ms: evidence.verified_at_ms,
    };
    write_backup_manifest(&destination, serde_json::to_vec_pretty(&manifest)?).await?;
    Ok(RetainedMigrationBackup {
        path: destination,
        files: evidence.result.files,
        bytes: evidence.result.bytes,
        plan_duration: evidence.result.plan_duration,
        copy_duration: evidence.result.copy_duration,
        verify_duration: evidence.result.verify_duration,
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
    let evidence = create_verified_migration_backup_evidence(source, destination, progress).await?;
    write_backup_manifest(destination, manifest.to_vec()).await?;
    Ok(evidence.result)
}

async fn create_verified_migration_backup_evidence(
    source: &Path,
    destination: &Path,
    progress: Option<BackupProgressCallback>,
) -> Result<VerifiedBackupEvidence, MigrationBackupError> {
    let source = source.to_path_buf();
    let destination = destination.to_path_buf();
    Ok(tokio::task::spawn_blocking(move || {
        create_verified_migration_backup_blocking(
            &source,
            &destination,
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
    fault: BackupCopyFault,
    progress: Option<&BackupProgressCallback>,
) -> std::io::Result<VerifiedBackupEvidence> {
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
        abort_at_backup_crash_boundary("before_copy");
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
        let verified_at_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| std::io::Error::other("system clock is earlier than Unix epoch"))?
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        let files = files
            .into_iter()
            .map(|file| {
                let sha256 = source_hashes
                    .get(&file.relative_path)
                    .copied()
                    .ok_or_else(|| std::io::Error::other("missing source backup digest"))?;
                Ok((
                    file.relative_path,
                    MigrationBackupFileEvidence {
                        bytes: file.bytes,
                        sha256: hex_digest(sha256),
                    },
                ))
            })
            .collect::<std::io::Result<BTreeMap<_, _>>>()?;
        Ok(VerifiedBackupEvidence {
            result: VerifiedMigrationBackup {
                files: file_count,
                bytes,
                plan_duration,
                copy_duration,
                verify_duration,
            },
            files,
            verified_at_ms,
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
            if relative_path.starts_with(Path::new(MIGRATION_BACKUP_MANIFEST_FILE)) {
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
            abort_at_backup_crash_boundary("during_copy");
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
    abort_at_backup_crash_boundary("before_verification");
    for file in files {
        let destination_path = destination.join(&file.relative_path);
        if fs::metadata(&destination_path)?.len() != file.bytes {
            return Err(std::io::Error::other(format!(
                "backup length verification failed for {}",
                file.relative_path.display()
            )));
        }
        let actual = hash_file(&destination_path)?;
        abort_at_backup_crash_boundary("during_verification");
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

fn hex_digest(digest: [u8; 32]) -> String {
    use std::fmt::Write as _;

    digest
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        })
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

    #[test]
    #[ignore = "subprocess helper for backup_process_crash_boundaries_preserve_source"]
    fn backup_crash_helper() {
        let source =
            PathBuf::from(std::env::var_os("BCODE_MIGRATION_BACKUP_CRASH_SOURCE").expect("source"));
        let destination = PathBuf::from(
            std::env::var_os("BCODE_MIGRATION_BACKUP_CRASH_DESTINATION").expect("destination"),
        );
        create_verified_migration_backup_blocking(
            &source,
            &destination,
            BackupCopyFault::None,
            None,
        )
        .expect("backup crash phase must abort first");
    }

    fn run_backup_crash_child(source: &Path, destination: &Path, phase: &str) {
        let status =
            std::process::Command::new(std::env::current_exe().expect("current test binary"))
                .args([
                    "--exact",
                    "backup::tests::backup_crash_helper",
                    "--ignored",
                    "--nocapture",
                ])
                .env("BCODE_MIGRATION_BACKUP_CRASH_SOURCE", source)
                .env("BCODE_MIGRATION_BACKUP_CRASH_DESTINATION", destination)
                .env("BCODE_MIGRATION_BACKUP_CRASH_PHASE", phase)
                .status()
                .expect("run backup crash child");
        assert!(!status.success(), "backup helper must terminate abnormally");
    }

    #[test]
    fn backup_process_crash_boundaries_preserve_source() {
        for phase in [
            "before_copy",
            "during_copy",
            "before_verification",
            "during_verification",
        ] {
            let temp = tempfile::tempdir().expect("temp dir");
            let source = temp.path().join("source");
            let destination = temp.path().join("destination");
            fs::create_dir_all(&source).expect("source");
            let bytes = vec![0x5a; BACKUP_BUFFER_BYTES * 2 + 7];
            fs::write(source.join("session.db"), &bytes).expect("source DB");
            run_backup_crash_child(&source, &destination, phase);
            assert_eq!(fs::read(source.join("session.db")).expect("source"), bytes);
            match phase {
                "before_copy" | "during_copy" => {
                    assert!(
                        !destination.exists()
                            || !destination.join(MIGRATION_BACKUP_MANIFEST_FILE).exists(),
                        "copy crash must not expose a verified backup"
                    );
                }
                "before_verification" | "during_verification" => {
                    assert!(destination.exists());
                    assert!(
                        !destination.join(MIGRATION_BACKUP_MANIFEST_FILE).exists(),
                        "verification crash must not expose a verified manifest"
                    );
                }
                _ => unreachable!("fixed crash phase"),
            }
            let retry_destination = temp.path().join("retry-destination");
            let retried = create_verified_migration_backup_blocking(
                &source,
                &retry_destination,
                BackupCopyFault::None,
                None,
            )
            .expect("retry backup with a fresh destination");
            assert_eq!(retried.result.files, 1);
            assert_eq!(
                fs::read(retry_destination.join("session.db")).expect("retried backup"),
                bytes
            );
        }
    }

    #[test]
    fn backup_request_builder_owns_target_epoch_and_plan() {
        let session_id = SessionId::new();
        let request = build_migration_backup_request(MigrationBackupRequestPlan {
            sessions_root: PathBuf::from("sessions"),
            session_id,
            operation_id: "operation".to_owned(),
            source_writer_epoch: 2,
            canonical_source: MigrationBackupCanonicalEvidence::default(),
            converted_events: BTreeMap::new(),
            retired_known_events: BTreeMap::new(),
        })
        .expect("request");
        assert_eq!(
            request.target_writer_epoch,
            u64::from(crate::CURRENT_WRITER_EPOCH)
        );
        assert_eq!(
            request.migration_step_ids,
            [
                "session-writer-epoch-2-to-3",
                "session-writer-epoch-3-to-4",
                "session-writer-epoch-4-to-5",
                "session-writer-epoch-5-to-6",
            ]
        );
        assert_eq!(request.session_id, session_id);
    }

    #[tokio::test]
    async fn retained_backup_owns_layout_and_manifest_policy() {
        let temp = tempfile::tempdir().expect("temp dir");
        let sessions_root = temp.path().join("sessions");
        let session_id = SessionId::new();
        let source = sessions_root.join(session_id.to_string());
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("session.db"), b"canonical").expect("database");
        fs::write(source.join("session.db-wal"), b"wal").expect("WAL");
        fs::write(source.join("session.db-shm"), b"shm").expect("SHM");

        let result = create_retained_migration_backup(
            MigrationBackupRequest {
                sessions_root: sessions_root.clone(),
                session_id,
                operation_id: "operation-1".to_owned(),
                source_writer_epoch: 2,
                target_writer_epoch: 5,
                migration_step_ids: vec!["session-writer-epoch-2-to-3".to_owned()],
                canonical_source: MigrationBackupCanonicalEvidence {
                    classified_event_count: 1,
                    event_count: 1,
                    event_tail: Some(0),
                    payload_digest_sha256: "canonical-digest".to_owned(),
                },
                converted_events: BTreeMap::from([("28:tool_call_finished".to_owned(), 1)]),
                retired_known_events: BTreeMap::new(),
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
            Some(temp.path().join(MIGRATION_BACKUPS_DIRECTORY).as_path())
        );
        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(result.path.join("migration-backup.json")).expect("manifest"),
        )
        .expect("manifest json");
        assert_eq!(manifest["session_id"], session_id.to_string());
        assert_eq!(manifest["operation_id"], "operation-1");
        assert_eq!(manifest["source_writer_epoch"], 2);
        assert_eq!(manifest["target_writer_epoch"], 5);
        assert_eq!(manifest["canonical_source"]["classified_event_count"], 1);
        assert_eq!(manifest["canonical_source"]["event_count"], 1);
        assert_eq!(manifest["canonical_source"]["event_tail"], 0);
        assert_eq!(
            manifest["canonical_source"]["payload_digest_sha256"],
            "canonical-digest"
        );
        assert_eq!(manifest["database"]["bytes"], 9);
        assert_eq!(
            manifest["database"]["sha256"].as_str().map(str::len),
            Some(64)
        );
        assert_eq!(manifest["wal"]["bytes"], 3);
        assert_eq!(manifest["wal"]["sha256"].as_str().map(str::len), Some(64));
        assert_eq!(manifest["shm"]["bytes"], 3);
        assert_eq!(manifest["shm"]["sha256"].as_str().map(str::len), Some(64));
        assert!(manifest["created_at_ms"].as_u64().is_some());
        assert!(manifest["verified_at_ms"].as_u64().is_some());

        let diagnosis = latest_retained_migration_backup(&sessions_root, session_id)
            .expect("backup diagnosis")
            .expect("retained backup");
        assert_eq!(diagnosis.path, result.path);
        assert_eq!(diagnosis.manifest.operation_id, "operation-1");
    }

    #[test]
    fn retained_backup_diagnosis_accepts_legacy_manifest_for_incident_visibility() {
        let temp = tempfile::tempdir().expect("temp dir");
        let sessions_root = temp.path().join("sessions");
        let backups = temp.path().join(MIGRATION_BACKUPS_DIRECTORY);
        let session_id = SessionId::new();
        let backup = backups.join("legacy");
        fs::create_dir_all(&backup).expect("backup");
        fs::write(
            backup.join(MIGRATION_BACKUP_MANIFEST_FILE),
            serde_json::to_vec(&serde_json::json!({
                "session_id": session_id,
                "source_writer_epoch": 2,
                "target_writer_epoch": 4,
                "created_at_ms": 7
            }))
            .expect("manifest"),
        )
        .expect("manifest");

        let diagnosis = latest_retained_migration_backup(&sessions_root, session_id)
            .expect("diagnosis")
            .expect("legacy backup");
        assert_eq!(diagnosis.path, backup);
        assert_eq!(diagnosis.manifest.source_writer_epoch, 2);
        assert_eq!(diagnosis.manifest.target_writer_epoch, 4);
        assert_eq!(diagnosis.manifest.created_at_ms, 7);
        assert!(diagnosis.manifest.operation_id.is_empty());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn retained_backup_diagnosis_selects_newest_matching_valid_manifest_without_mutation() {
        let temp = tempfile::tempdir().expect("temp dir");
        let sessions_root = temp.path().join("sessions");
        let backups = temp.path().join(MIGRATION_BACKUPS_DIRECTORY);
        let session_id = SessionId::new();
        let other_session_id = SessionId::new();
        fs::create_dir_all(&sessions_root).expect("sessions");
        for (directory, manifest) in [
            (
                "older",
                MigrationBackupManifest {
                    session_id,
                    operation_id: "older".to_owned(),
                    source_writer_epoch: 2,
                    target_writer_epoch: 5,
                    migration_step_ids: Vec::new(),
                    canonical_source: MigrationBackupCanonicalEvidence {
                        classified_event_count: 0,
                        event_count: 0,
                        event_tail: None,
                        payload_digest_sha256: String::new(),
                    },
                    converted_events: BTreeMap::new(),
                    retired_known_events: BTreeMap::new(),
                    database: MigrationBackupFileEvidence {
                        bytes: 0,
                        sha256: String::new(),
                    },
                    wal: None,
                    shm: None,
                    created_at_ms: 1,
                    verified_at_ms: 2,
                },
            ),
            (
                "newer",
                MigrationBackupManifest {
                    session_id,
                    operation_id: "newer".to_owned(),
                    source_writer_epoch: 4,
                    target_writer_epoch: 5,
                    migration_step_ids: Vec::new(),
                    canonical_source: MigrationBackupCanonicalEvidence {
                        classified_event_count: 0,
                        event_count: 0,
                        event_tail: None,
                        payload_digest_sha256: String::new(),
                    },
                    converted_events: BTreeMap::new(),
                    retired_known_events: BTreeMap::new(),
                    database: MigrationBackupFileEvidence {
                        bytes: 0,
                        sha256: String::new(),
                    },
                    wal: None,
                    shm: None,
                    created_at_ms: 3,
                    verified_at_ms: 4,
                },
            ),
        ] {
            let path = backups.join(directory);
            fs::create_dir_all(&path).expect("backup");
            fs::write(
                path.join(MIGRATION_BACKUP_MANIFEST_FILE),
                serde_json::to_vec(&manifest).expect("manifest"),
            )
            .expect("manifest");
        }
        let other = backups.join("other");
        fs::create_dir_all(&other).expect("other backup");
        let mut other_manifest = serde_json::to_value(MigrationBackupManifest {
            session_id: other_session_id,
            operation_id: "other".to_owned(),
            source_writer_epoch: 4,
            target_writer_epoch: 5,
            migration_step_ids: Vec::new(),
            canonical_source: MigrationBackupCanonicalEvidence {
                classified_event_count: 0,
                event_count: 0,
                event_tail: None,
                payload_digest_sha256: String::new(),
            },
            converted_events: BTreeMap::new(),
            retired_known_events: BTreeMap::new(),
            database: MigrationBackupFileEvidence {
                bytes: 0,
                sha256: String::new(),
            },
            wal: None,
            shm: None,
            created_at_ms: 99,
            verified_at_ms: 99,
        })
        .expect("other manifest");
        other_manifest["ignored"] = serde_json::Value::Bool(true);
        fs::write(
            other.join(MIGRATION_BACKUP_MANIFEST_FILE),
            serde_json::to_vec(&other_manifest).expect("other manifest"),
        )
        .expect("other manifest");
        let damaged = backups.join("damaged");
        fs::create_dir_all(&damaged).expect("damaged backup");
        fs::write(damaged.join(MIGRATION_BACKUP_MANIFEST_FILE), b"not json")
            .expect("damaged manifest");

        let before = fs::read_dir(&backups).expect("before").count();
        let diagnosis = latest_retained_migration_backup(&sessions_root, session_id)
            .expect("diagnosis")
            .expect("matching backup");
        assert_eq!(diagnosis.path, backups.join("newer"));
        assert_eq!(diagnosis.manifest.operation_id, "newer");
        assert_eq!(fs::read_dir(&backups).expect("after").count(), before);
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
            BackupCopyFault::None,
            None,
        )
        .expect("backup");
        assert_eq!(result.result.files, 3);
        assert_eq!(
            result.result.bytes,
            u64::try_from(large.len() + 5).expect("bytes")
        );
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
            let error =
                create_verified_migration_backup_blocking(&source, &destination, fault, None)
                    .expect_err("fault");
            assert_eq!(error.kind(), expected);
            assert!(!destination.exists());
        }
    }
}
