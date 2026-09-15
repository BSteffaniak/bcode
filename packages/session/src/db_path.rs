//! Canonical global-catalog and per-session database paths.

use bcode_session_models::SessionId;
use std::path::{Path, PathBuf};

/// Return Bcode's canonical global catalog database path under `root`.
#[must_use]
pub fn global_catalog_db_path(root: &Path) -> PathBuf {
    root.join("catalog.db")
}

/// Return Bcode's canonical per-session directory under `root`.
#[must_use]
pub fn session_dir_path(root: &Path, session_id: SessionId) -> PathBuf {
    root.join(session_id.to_string())
}

/// Return Bcode's default per-session database path for `session_id`.
#[must_use]
pub fn session_db_path(root: &Path, session_id: SessionId) -> PathBuf {
    session_dir_path(root, session_id).join("session.db")
}

/// Resolve an existing current-format database directory without modifying storage.
///
/// Regular files remain directly addressable. Directory format 1 contains an exact `format`
/// marker (`BCODE_SESSION_DB 1\n`) and the database at `data.db`. Callers retain session ownership
/// throughout resolution and access; this function does not authorize migration or maintenance.
///
/// # Errors
/// Returns an error for missing paths, non-regular files, symlinks, unsupported markers, or a
/// missing inner database. Never substitutes a different representation or creates storage.
pub fn resolve_existing_session_db(path: &Path) -> std::io::Result<PathBuf> {
    use std::io::{Error, ErrorKind, Read};
    let invalid = || {
        Error::new(
            ErrorKind::InvalidData,
            "unsupported session database representation",
        )
    };
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_file() {
        return Ok(path.to_path_buf());
    }
    if !metadata.is_dir() {
        return Err(invalid());
    }
    let marker = path.join("format");
    if !std::fs::symlink_metadata(&marker)?.is_file() {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(marker)?
        .take(20)
        .read_to_end(&mut bytes)?;
    if bytes != b"BCODE_SESSION_DB 1\n" {
        return Err(invalid());
    }
    let database = path.join("data.db");
    if !std::fs::symlink_metadata(&database)?.is_file() {
        return Err(invalid());
    }
    Ok(database)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_resolution_is_bounded_and_non_mutating() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.db");
        std::fs::create_dir(&path).unwrap();
        assert!(resolve_existing_session_db(&path).is_err());
        std::fs::write(path.join("format"), b"BCODE_SESSION_DB 1\n").unwrap();
        assert!(resolve_existing_session_db(&path).is_err());
        assert!(!path.join("data.db").exists());
        std::fs::write(path.join("data.db"), b"unchanged").unwrap();
        assert_eq!(
            resolve_existing_session_db(&path).unwrap(),
            path.join("data.db")
        );
        for marker in [
            b"BCODE_SESSION_DB 2\n".as_slice(),
            b"BCODE_SESSION_DB 1\nextra",
            b"",
        ] {
            std::fs::write(path.join("format"), marker).unwrap();
            assert!(resolve_existing_session_db(&path).is_err());
            assert_eq!(std::fs::read(path.join("format")).unwrap(), marker);
            assert_eq!(std::fs::read(path.join("data.db")).unwrap(), b"unchanged");
        }
    }

    #[cfg(unix)]
    #[test]
    fn directory_resolution_rejects_symlinked_content() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.db");
        std::fs::create_dir(&path).unwrap();
        let outside = root.path().join("outside");
        std::fs::write(&outside, b"BCODE_SESSION_DB 1\n").unwrap();
        std::os::unix::fs::symlink(&outside, path.join("format")).unwrap();
        assert!(resolve_existing_session_db(&path).is_err());
        std::fs::remove_file(path.join("format")).unwrap();
        std::fs::write(path.join("format"), b"BCODE_SESSION_DB 1\n").unwrap();
        std::os::unix::fs::symlink(&outside, path.join("data.db")).unwrap();
        assert!(resolve_existing_session_db(&path).is_err());
    }

    #[test]
    fn canonical_paths_do_not_depend_on_writer_identity() {
        let root = Path::new("sessions");
        let session_id = SessionId::new();
        assert_eq!(
            session_db_path(root, session_id),
            root.join(session_id.to_string()).join("session.db")
        );
        assert_eq!(global_catalog_db_path(root), root.join("catalog.db"));
    }
}
