//! Canonical global-catalog and per-session database paths.

use bcode_session_models::SessionId;
use std::path::{Path, PathBuf};

/// Return Bcode's legacy global catalog database path under `root`.
#[must_use]
pub fn global_catalog_db_path(root: &Path) -> PathBuf {
    root.join("catalog.db")
}

/// Return a build/compatibility-scoped catalog database path under `root`.
#[must_use]
pub fn namespaced_catalog_db_path(root: &Path, namespace: &str) -> PathBuf {
    root.join("catalogs").join(namespace).join("catalog.db")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_paths_do_not_depend_on_writer_identity() {
        let root = Path::new("sessions");
        let session_id = SessionId::new();
        assert_eq!(
            session_db_path(root, session_id),
            root.join(session_id.to_string()).join("session.db")
        );
        assert_eq!(global_catalog_db_path(root), root.join("catalog.db"));
        assert_eq!(
            namespaced_catalog_db_path(root, "build"),
            root.join("catalogs").join("build").join("catalog.db")
        );
    }
}
