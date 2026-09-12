//! Bounded, read-only physical storage accounting. Never opens a session database.

use bcode_session_models::{SessionId, SessionStorageBytes, SessionStorageUsage};
use std::fs;
use std::io;
use std::path::Path;

/// Maximum directory entries inspected by a single explicit storage measurement.
pub const MAX_STORAGE_MEASUREMENT_ENTRIES: u32 = 100_000;
const MAX_DEPTH: usize = 32;

#[derive(Clone, Copy)]
enum Category {
    Session,
    Artifact,
}

/// Measure one session and its artifacts without loading, repairing, or replaying history.
///
/// Symlinks and special files are skipped, never followed. Work and retained traversal state are
/// bounded by the entry budget and depth, respectively. The result is a best-effort observation,
/// not a consistent snapshot. Missing artifact directories are allowed; missing canonical storage
/// is not. This does not inspect search-provider state or distinguish database tables.
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
    let database = fs::symlink_metadata(session.join("session.db"))?;
    if !database.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "canonical database is not a regular file",
        ));
    }
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
    Ok(usage)
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
            if depth == MAX_DEPTH
                || visit(&entry.path(), root, category, budget, depth + 1, usage).is_err()
            {
                usage.skipped_entries += 1;
            }
            if usage.budget_exhausted {
                break;
            }
        } else if metadata.is_file() {
            let bucket = match category {
                Category::Artifact => &mut usage.artifacts,
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
