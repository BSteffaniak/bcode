//! Bounded, read-only physical storage accounting. Never opens a session database.

use bcode_session_models::{SessionId, SessionStorageBytes, SessionStorageUsage};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Maximum directory entries inspected by a single explicit storage measurement.
pub const MAX_STORAGE_MEASUREMENT_ENTRIES: u32 = 100_000;
const MAX_DEPTH: usize = 32;

/// Minimum free-space cushion before starting a maintenance write, not a WAL-size guarantee.
pub const MIN_MAINTENANCE_HEADROOM_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy)]
enum Category {
    Session,
    Database,
    Artifact,
    RetainedBackup,
}

/// Measure one session and its artifacts without loading, repairing, or replaying history.
///
/// Symlinks and special files are skipped, never followed. Work and retained traversal state are
/// bounded by the entry budget and depth, respectively. The result is a best-effort observation,
/// not a consistent snapshot. Missing artifact directories are allowed; missing canonical storage
/// is not. This does not inspect search-provider state or distinguish database tables. External
/// recovery backups are excluded; use `measure_session_storage_with_backups` with a migration-owned
/// inventory to include them.
///
/// # Errors
///
/// Returns an error for a zero or excessive entry budget, an unavailable root or canonical session
/// database, or a symlink in a session-owned root. Nested unreadable entries are counted as skipped.
pub fn measure_session_storage(
    root: &Path,
    session_id: SessionId,
    entry_budget: u32,
) -> io::Result<SessionStorageUsage> {
    measure_session_storage_with_backups(root, session_id, entry_budget, &[])
}

/// Measure storage plus migration-owned, retained recovery directories with one shared budget.
///
/// The caller must supply an exhaustive, bounded inventory for this session from the migration
/// domain. This function never discovers, classifies, validates, or deletes recovery copies. An
/// empty list means no supplied backups, not proof that none exist. Discovery incompleteness must
/// be surfaced separately by the caller. Paths must be absolute immediate children of the sibling
/// `session-migration-backups` directory; each supplied root consumes one traversal entry. Backup
/// contents are opaque files, even if their format is unknown or corrupt.
///
/// # Errors
/// Returns ordinary measurement failures, or rejects duplicate, nested, linked, missing or foreign
/// backup roots. Entries beyond the shared budget are not inspected and set `budget_exhausted`.
pub fn measure_session_storage_with_backups(
    root: &Path,
    session_id: SessionId,
    entry_budget: u32,
    retained_backup_paths: &[PathBuf],
) -> io::Result<SessionStorageUsage> {
    if !(1..=MAX_STORAGE_MEASUREMENT_ENTRIES).contains(&entry_budget) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid storage measurement budget",
        ));
    }
    let root = root.canonicalize()?;
    let session = root.join(session_id.to_string());
    require_directory(&session)?;
    require_confined(&session, &root)?;
    crate::db_path::resolve_existing_session_db(&session.join("session.db"))?;
    let mut usage = SessionStorageUsage::default();
    visit(
        &session,
        &session,
        Category::Session,
        entry_budget,
        0,
        &mut usage,
    )?;
    let artifacts_root = root.join("session-artifacts");
    match require_directory(&artifacts_root) {
        Ok(()) => {
            require_confined(&artifacts_root, &root)?;
            let artifacts = artifacts_root.join(session_id.to_string());
            match require_directory(&artifacts) {
                Ok(()) => {
                    require_confined(&artifacts, &artifacts_root)?;
                    visit(
                        &artifacts,
                        &artifacts,
                        Category::Artifact,
                        entry_budget,
                        0,
                        &mut usage,
                    )?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if !retained_backup_paths.is_empty() {
        let backup_root = root
            .parent()
            .unwrap_or(&root)
            .join("session-migration-backups");
        let mut seen = std::collections::BTreeSet::new();
        for backup in retained_backup_paths {
            if usage.visited_entries == entry_budget {
                usage.budget_exhausted = true;
                break;
            }
            usage.visited_entries += 1;
            require_directory(&backup_root)?;
            require_confined(&backup_root, root.parent().unwrap_or(&root))?;
            if backup.parent() != Some(backup_root.as_path())
                || !seen.insert(backup.clone())
                || backup
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid retained backup root",
                ));
            }
            require_directory(backup)?;
            require_confined(backup, &backup_root)?;
            if backup.canonicalize()?.parent() != Some(backup_root.as_path()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "retained backup must be an immediate child",
                ));
            }
            visit(
                backup,
                &backup_root,
                Category::RetainedBackup,
                entry_budget,
                0,
                &mut usage,
            )?;
        }
    }
    Ok(usage)
}

/// Check available destination space without counting bytes occupied by the source as free.
///
/// `additional_bytes` is caller-owned scratch-space policy, including any desired safety margin.
/// This is an observation, not a reservation: concurrent writers, filesystem metadata, quotas and
/// copy-on-write behavior can still make a later write fail. The directory descriptor pins the
/// destination filesystem, avoiding a path re-resolution between admission and the check.
///
/// # Errors
/// Rejects non-directory handles, unsupported platforms, failed filesystem queries, unrepresentable
/// measurements, or insufficient space available to the current user.
pub fn ensure_storage_headroom(destination: &fs::File, additional_bytes: u64) -> io::Result<()> {
    if !destination.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "storage destination is not a directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        let mut statistics = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: destination remains open and statistics points to writable, correctly sized memory.
        if unsafe { libc::fstatvfs(destination.as_raw_fd(), statistics.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful fstatvfs initialized the entire result.
        let statistics = unsafe { statistics.assume_init() };
        if statistics.f_frsize == 0 {
            return Err(io::Error::other("unknown filesystem allocation unit"));
        }
        let available = u128::from(statistics.f_bavail)
            .checked_mul(u128::from(statistics.f_frsize))
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or_else(|| io::Error::other("invalid available storage measurement"))?;
        check_available_storage(available, additional_bytes)
    }
    #[cfg(not(unix))]
    {
        let _ = additional_bytes;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "storage headroom query unavailable",
        ))
    }
}

#[cfg(any(unix, test))]
fn check_available_storage(available: u64, required: u64) -> io::Result<()> {
    if available < required {
        Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "insufficient maintenance scratch space",
        ))
    } else {
        Ok(())
    }
}

fn require_directory(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "storage root is not a directory",
        ))
    }
}

fn require_confined(path: &Path, root: &Path) -> io::Result<()> {
    if path.canonicalize()?.starts_with(root) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "storage path escapes its root",
        ))
    }
}

fn visit(
    path: &Path,
    root: &Path,
    category: Category,
    budget: u32,
    depth: usize,
    usage: &mut SessionStorageUsage,
) -> io::Result<()> {
    require_directory(path)?;
    require_confined(path, root)?;
    for entry in fs::read_dir(path)? {
        if usage.visited_entries == budget {
            usage.budget_exhausted = true;
            break;
        }
        usage.visited_entries += 1;
        let Ok(entry) = entry else {
            usage.skipped_entries += 1;
            continue;
        };
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            usage.skipped_entries += 1;
            continue;
        };
        if metadata.is_dir() {
            let child_category = if matches!(category, Category::Session)
                && depth == 0
                && entry.file_name() == "session.db"
            {
                Category::Database
            } else {
                category
            };
            if depth == MAX_DEPTH
                || visit(
                    &entry.path(),
                    root,
                    child_category,
                    budget,
                    depth + 1,
                    usage,
                )
                .is_err()
            {
                usage.skipped_entries += 1;
            }
            if usage.budget_exhausted {
                break;
            }
        } else if metadata.is_file() {
            let bucket = match category {
                Category::Artifact => &mut usage.artifacts,
                Category::RetainedBackup => &mut usage.retained_backups,
                Category::Database => &mut usage.database,
                Category::Session
                    if depth == 0
                        && matches!(
                            entry.file_name().to_str(),
                            Some(
                                "session.db"
                                    | "session.db-wal"
                                    | "session.db-shm"
                                    | "session.db-journal"
                            )
                        ) =>
                {
                    &mut usage.database
                }
                Category::Session => &mut usage.other,
            };
            add_file(bucket, &metadata);
        } else {
            usage.skipped_entries += 1;
        }
    }
    Ok(())
}

fn add_file(bucket: &mut SessionStorageBytes, metadata: &fs::Metadata) {
    bucket.files += 1;
    bucket.file_bytes = bucket.file_bytes.saturating_add(metadata.len());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        bucket.allocated_bytes = Some(
            bucket
                .allocated_bytes
                .unwrap_or_default()
                .saturating_add(metadata.blocks().saturating_mul(512)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backup_fixture() -> (tempfile::TempDir, PathBuf, SessionId, PathBuf) {
        let state = tempfile::tempdir().expect("state");
        let root = state.path().join("sessions");
        let id = SessionId::new();
        fs::create_dir_all(root.join(id.to_string())).expect("session");
        fs::write(root.join(id.to_string()).join("session.db"), b"canonical").expect("database");
        let backup = state.path().join("session-migration-backups/recovery");
        fs::create_dir_all(&backup).expect("backup");
        fs::write(backup.join("session.db"), b"opaque historical backup").expect("backup bytes");
        (state, root, id, backup)
    }

    #[test]
    fn supplied_external_backups_are_separate_and_never_interpreted() {
        let (_state, root, id, backup) = backup_fixture();
        fs::create_dir(backup.join("nested")).expect("nested");
        fs::write(backup.join("nested/wal"), b"wal").expect("wal");
        let before = fs::read(backup.join("session.db")).expect("before");
        let usage =
            measure_session_storage_with_backups(&root, id, 100, std::slice::from_ref(&backup))
                .expect("measurement");
        assert_eq!(usage.database.file_bytes, 9);
        assert_eq!(usage.other.files, 0);
        assert_eq!(usage.retained_backups.files, 2);
        assert_eq!(usage.retained_backups.file_bytes, before.len() as u64 + 3);
        assert_eq!(usage.skipped_entries, 0);
        assert!(!usage.budget_exhausted);
        assert_eq!(fs::read(backup.join("session.db")).expect("after"), before);
        assert_eq!(
            measure_session_storage(&root, id, 100)
                .expect("base")
                .retained_backups
                .files,
            0
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let expected = fs::metadata(backup.join("session.db"))
                .expect("metadata")
                .blocks()
                + fs::metadata(backup.join("nested/wal"))
                    .expect("metadata")
                    .blocks();
            assert_eq!(usage.retained_backups.allocated_bytes, Some(expected * 512));
        }
    }

    #[test]
    fn backup_roots_and_contents_share_the_overall_budget() {
        let (_state, root, id, backup) = backup_fixture();
        let paths = [backup];
        let usage = measure_session_storage_with_backups(&root, id, 2, &paths).expect("bounded");
        assert_eq!(usage.visited_entries, 2);
        assert!(usage.budget_exhausted);
        assert_eq!(usage.database.files, 1);
        assert_eq!(usage.retained_backups.files, 0);
        let complete =
            measure_session_storage_with_backups(&root, id, 3, &paths).expect("complete");
        assert_eq!(complete.visited_entries, 3);
        assert!(!complete.budget_exhausted);
        assert_eq!(complete.retained_backups.files, 1);
    }

    #[test]
    fn backup_inventory_rejects_duplicates_foreign_and_ancestor_paths() {
        let (_state, root, id, backup) = backup_fixture();
        assert!(
            measure_session_storage_with_backups(&root, id, 100, &[backup.clone(), backup.clone()])
                .is_err()
        );
        assert!(
            measure_session_storage_with_backups(&root, id, 100, std::slice::from_ref(&root))
                .is_err()
        );
        let ancestor = backup.parent().expect("backup root").join(".");
        assert!(measure_session_storage_with_backups(&root, id, 100, &[ancestor]).is_err());
        let nested = backup.join("nested");
        fs::create_dir(&nested).expect("nested");
        assert!(measure_session_storage_with_backups(&root, id, 100, &[nested]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn linked_backup_roots_are_rejected_and_linked_contents_are_skipped() {
        use std::os::unix::fs::symlink;
        let (_state, root, id, backup) = backup_fixture();
        let linked = backup.with_file_name("linked");
        symlink(&backup, &linked).expect("linked root");
        assert!(measure_session_storage_with_backups(&root, id, 100, &[linked]).is_err());
        symlink(root.join(id.to_string()), backup.join("escape")).expect("linked content");
        let usage =
            measure_session_storage_with_backups(&root, id, 100, &[backup]).expect("measurement");
        assert_eq!(usage.skipped_entries, 1);
        assert_eq!(usage.retained_backups.files, 1);
    }

    #[test]
    fn scratch_admission_does_not_credit_payload_savings() {
        assert!(check_available_storage(4096, 4096).is_ok());
        assert!(check_available_storage(0, 0).is_ok());
        assert_eq!(
            check_available_storage(4095, 4096)
                .expect_err("space")
                .kind(),
            io::ErrorKind::StorageFull
        );
        assert_eq!(
            check_available_storage(0, u64::MAX)
                .expect_err("space")
                .kind(),
            io::ErrorKind::StorageFull
        );
    }

    #[cfg(unix)]
    #[test]
    fn headroom_query_uses_directory_handle_and_does_not_mutate_storage() {
        let directory = tempfile::tempdir().expect("directory");
        let handle = fs::File::open(directory.path()).expect("handle");
        ensure_storage_headroom(&handle, 0).expect("query");
        assert_eq!(fs::read_dir(directory.path()).expect("entries").count(), 0);
        let file = directory.path().join("file");
        fs::write(&file, b"unchanged").expect("file");
        assert_eq!(
            ensure_storage_headroom(&fs::File::open(&file).expect("file handle"), 0)
                .expect_err("not directory")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(fs::read(file).expect("content"), b"unchanged");
    }

    fn fixture() -> (tempfile::TempDir, SessionId) {
        let root = tempfile::tempdir().expect("root");
        let id = SessionId::new();
        let session = root.path().join(id.to_string());
        fs::create_dir(&session).expect("session");
        fs::write(session.join("session.db"), b"not a database").expect("database");
        (root, id)
    }

    #[tokio::test]
    async fn manager_measurement_does_not_load_a_session() {
        let (root, id) = fixture();
        let manager = crate::SessionManager::persistent_lazy(root.path());
        let usage = manager.storage_usage(id, 10).await.expect("measurement");
        assert_eq!(usage.database.file_bytes, 14);
        // An invalid database can still be measured without being interpreted or repaired.
        assert_eq!(
            fs::read(root.path().join(id.to_string()).join("session.db")).expect("database"),
            b"not a database"
        );
    }

    #[test]
    fn measures_directory_database_and_sidecars_with_shared_budget() {
        let root = tempfile::tempdir().expect("root");
        let id = SessionId::new();
        let session = root.path().join(id.to_string());
        let database = session.join("session.db");
        fs::create_dir_all(&database).expect("directory");
        let marker = b"BCODE_SESSION_DB 1\n";
        fs::write(database.join("format"), marker).expect("marker");
        fs::write(database.join("data.db"), b"uninterpreted database").expect("database");
        fs::write(database.join("data.db-wal"), b"wal").expect("wal");
        fs::write(session.join("note"), b"note").expect("other");
        let usage = measure_session_storage(root.path(), id, 100).expect("measure");
        assert_eq!(usage.database.files, 3);
        assert_eq!(usage.database.file_bytes, 44);
        assert_eq!(usage.other.file_bytes, 4);
        assert_eq!(usage.skipped_entries, 0);
        assert_eq!(fs::read(database.join("format")).unwrap(), marker);
        assert_eq!(
            fs::read(database.join("data.db")).unwrap(),
            b"uninterpreted database"
        );
        let bounded = measure_session_storage(root.path(), id, 1).expect("bounded");
        assert_eq!(bounded.visited_entries, 1);
        assert!(bounded.budget_exhausted);
    }

    #[test]
    fn depth_limit_reports_incomplete_measurement() {
        let (root, id) = fixture();
        let mut directory = root.path().join(id.to_string());
        for _ in 0..MAX_DEPTH + 2 {
            directory = directory.join("nested");
            fs::create_dir(&directory).expect("nested directory");
        }
        fs::write(directory.join("unvisited"), b"bytes").expect("deep file");
        let usage = measure_session_storage(root.path(), id, 1000).expect("measure");
        assert_eq!(usage.skipped_entries, 1);
        assert_eq!(usage.other.files, 0);
        assert!(!usage.budget_exhausted);
    }

    #[test]
    fn accounts_files_without_opening_database_or_mutating_contents() {
        let (root, id) = fixture();
        let session = root.path().join(id.to_string());
        fs::write(session.join("session.db-wal"), b"wal").expect("wal");
        fs::create_dir(session.join("backup")).expect("backup");
        fs::write(session.join("backup/session.db"), b"backup").expect("backup db");
        let artifact = root.path().join("session-artifacts").join(id.to_string());
        fs::create_dir_all(&artifact).expect("artifact dir");
        fs::write(artifact.join("terminal.bin"), b"terminal").expect("terminal");
        let usage = measure_session_storage(root.path(), id, 100).expect("measure");
        assert_eq!(usage.database.files, 2);
        assert_eq!(usage.database.file_bytes, 17);
        assert_eq!(usage.other.file_bytes, 6);
        assert_eq!(usage.artifacts.file_bytes, 8);
        assert_eq!(usage.skipped_entries, 0);
        assert!(!usage.budget_exhausted);
        assert_eq!(
            fs::read(session.join("session.db")).expect("unchanged"),
            b"not a database"
        );
        assert_eq!(
            measure_session_storage(root.path(), id, 100).expect("repeat"),
            usage
        );
    }

    #[test]
    fn enforces_budget_and_missing_canonical_authority() {
        let (root, id) = fixture();
        let session = root.path().join(id.to_string());
        fs::write(session.join("extra"), b"extra").expect("extra");
        let usage = measure_session_storage(root.path(), id, 1).expect("bounded");
        assert_eq!(usage.visited_entries, 1);
        assert!(usage.budget_exhausted);
        for budget in [0, MAX_STORAGE_MEASUREMENT_ENTRIES + 1] {
            assert_eq!(
                measure_session_storage(root.path(), id, budget)
                    .expect_err("invalid")
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert_eq!(
            measure_session_storage(root.path(), SessionId::new(), 10)
                .expect_err("missing")
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[cfg(unix)]
    #[test]
    fn excludes_symbolic_links_and_rejects_linked_roots() {
        use std::os::unix::fs::symlink;
        let (root, id) = fixture();
        let outside = tempfile::tempdir().expect("outside");
        fs::write(outside.path().join("secret"), b"private").expect("secret");
        let session = root.path().join(id.to_string());
        symlink(outside.path(), session.join("escape")).expect("link");
        let usage = measure_session_storage(root.path(), id, 100).expect("measure");
        assert_eq!(usage.skipped_entries, 1);
        assert_eq!(usage.other.files, 0);
        symlink(outside.path(), root.path().join("session-artifacts")).expect("artifact root link");
        assert_eq!(
            measure_session_storage(root.path(), id, 100)
                .expect_err("unsafe root")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
