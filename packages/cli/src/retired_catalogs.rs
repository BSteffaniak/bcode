//! Explicit retired build-scoped catalog inventory and cleanup.

use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetiredCatalogAction {
    WouldRemove,
    Removed,
    RefusedLiveOrAmbiguousDaemon,
    SkippedNotCatalog,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogNamespaceClassification {
    Retired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogDaemonEvidence {
    Clear,
    LiveOrAmbiguous,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetiredCatalogReport {
    pub namespace: String,
    pub classification: CatalogNamespaceClassification,
    pub daemon_evidence: CatalogDaemonEvidence,
    pub path: PathBuf,
    pub database_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
    pub draft_rows: usize,
    pub migrated_drafts: usize,
    pub skipped_draft_conflicts: usize,
    pub removed_bytes: u64,
    pub action: RetiredCatalogAction,
    pub error: Option<String>,
}

fn file_bytes(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}-{suffix}", db_path.display()))
}

#[allow(clippy::too_many_lines)]
async fn inventory_one(
    state_dir: &Path,
    session_root: &Path,
    namespace_dir: PathBuf,
    apply: bool,
) -> RetiredCatalogReport {
    let namespace = namespace_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();
    let db_path = namespace_dir.join("catalog.db");
    let mut report = RetiredCatalogReport {
        namespace: namespace.clone(),
        classification: CatalogNamespaceClassification::Retired,
        daemon_evidence: CatalogDaemonEvidence::Clear,
        path: namespace_dir.clone(),
        database_bytes: file_bytes(&db_path),
        wal_bytes: file_bytes(&sidecar_path(&db_path, "wal")),
        shm_bytes: file_bytes(&sidecar_path(&db_path, "shm")),
        draft_rows: 0,
        migrated_drafts: 0,
        skipped_draft_conflicts: 0,
        removed_bytes: 0,
        action: RetiredCatalogAction::WouldRemove,
        error: None,
    };
    if !db_path.exists() {
        report.action = RetiredCatalogAction::SkippedNotCatalog;
        return report;
    }
    if bcode_daemon_lifecycle::namespace_has_live_or_ambiguous_evidence(state_dir, &namespace)
        .await
        .unwrap_or(true)
    {
        report.daemon_evidence = CatalogDaemonEvidence::LiveOrAmbiguous;
        report.action = RetiredCatalogAction::RefusedLiveOrAmbiguousDaemon;
        return report;
    }
    let retired =
        match bcode_session::db::GlobalSessionDb::open_existing_turso_without_catalog_lock(&db_path)
            .await
        {
            Ok(catalog) => catalog,
            Err(error) => {
                report.action = RetiredCatalogAction::Failed;
                report.error = Some(error.to_string());
                return report;
            }
        };
    let drafts = match retired.list_draft_session_composer_drafts().await {
        Ok(drafts) => drafts,
        Err(error) => {
            let _ = retired.close().await;
            report.action = RetiredCatalogAction::Failed;
            report.error = Some(error.to_string());
            return report;
        }
    };
    report.draft_rows = drafts.len();
    if !apply {
        if let Err(error) = retired.close().await {
            report.action = RetiredCatalogAction::Failed;
            report.error = Some(error.to_string());
        }
        return report;
    }

    let catalog_lock = match bcode_session::lease::acquire_catalog_lock(session_root) {
        Ok(lock) => lock,
        Err(error) => {
            let _ = retired.close().await;
            report.action = RetiredCatalogAction::Failed;
            report.error = Some(error.to_string());
            return report;
        }
    };
    let retired_catalog_lock = match bcode_session::lease::acquire_catalog_lock(&namespace_dir) {
        Ok(lock) => lock,
        Err(error) => {
            drop(catalog_lock);
            let _ = retired.close().await;
            report.action = RetiredCatalogAction::Failed;
            report.error = Some(error.to_string());
            return report;
        }
    };
    if bcode_daemon_lifecycle::namespace_has_live_or_ambiguous_evidence(state_dir, &namespace)
        .await
        .unwrap_or(true)
    {
        drop(retired_catalog_lock);
        drop(catalog_lock);
        let _ = retired.close().await;
        report.action = RetiredCatalogAction::RefusedLiveOrAmbiguousDaemon;
        return report;
    }
    let active_path = bcode_session::db::global_catalog_db_path(session_root);
    let active =
        match bcode_session::db::GlobalSessionDb::open_turso_without_catalog_lock(&active_path)
            .await
        {
            Ok(catalog) => catalog,
            Err(error) => {
                drop(retired_catalog_lock);
                drop(catalog_lock);
                let _ = retired.close().await;
                report.action = RetiredCatalogAction::Failed;
                report.error = Some(error.to_string());
                return report;
            }
        };
    for draft in drafts {
        let existing = active
            .draft_session_composer_draft_record(&draft.launch_working_directory)
            .await
            .ok()
            .flatten();
        let skipped = existing
            .as_ref()
            .is_some_and(|active_draft| active_draft.updated_at_ms >= draft.updated_at_ms);
        if active
            .set_draft_session_composer_draft(
                &draft.launch_working_directory,
                &draft.text,
                draft.updated_at_ms,
            )
            .await
            .is_err()
        {
            report.action = RetiredCatalogAction::Failed;
            report.error = Some("failed to migrate retired catalog draft".to_owned());
            let _ = active.close().await;
            let _ = retired.close().await;
            drop(retired_catalog_lock);
            drop(catalog_lock);
            return report;
        }
        if skipped {
            report.skipped_draft_conflicts += 1;
        } else {
            report.migrated_drafts += 1;
        }
    }
    if let Err(error) = active.close().await.and(retired.close().await) {
        drop(retired_catalog_lock);
        drop(catalog_lock);
        report.action = RetiredCatalogAction::Failed;
        report.error = Some(error.to_string());
        return report;
    }
    let removed_bytes = report
        .database_bytes
        .saturating_add(report.wal_bytes)
        .saturating_add(report.shm_bytes);
    match std::fs::remove_dir_all(&namespace_dir) {
        Ok(()) => {
            report.action = RetiredCatalogAction::Removed;
            report.removed_bytes = removed_bytes;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            report.action = RetiredCatalogAction::Removed;
        }
        Err(error) => {
            report.action = RetiredCatalogAction::Failed;
            report.error = Some(error.to_string());
        }
    }
    drop(retired_catalog_lock);
    drop(catalog_lock);
    report
}

/// Inventory or explicitly clean every retired build-scoped catalog namespace.
///
/// # Errors
///
/// Returns an error when the namespace directory cannot be read.
pub async fn retired_catalog_reports(
    state_dir: &Path,
    session_root: &Path,
    apply: bool,
) -> std::io::Result<Vec<RetiredCatalogReport>> {
    let catalogs_root = session_root.join("catalogs");
    let entries = match std::fs::read_dir(&catalogs_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    paths.sort();
    let mut reports = Vec::with_capacity(paths.len());
    for path in paths {
        reports.push(inventory_one(state_dir, session_root, path, apply).await);
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dry_run_inventory_is_non_mutating_and_reports_fixture_sizes() {
        let state = tempfile::tempdir().expect("state");
        let session_root = state.path().join("sessions");
        let namespace = "a427b7980996a913";
        let namespace_dir = session_root.join("catalogs").join(namespace);
        std::fs::create_dir_all(&namespace_dir).expect("namespace");
        let catalog = bcode_session::db::GlobalSessionDb::open_turso_without_catalog_lock(
            &namespace_dir.join("catalog.db"),
        )
        .await
        .expect("catalog");
        catalog
            .set_draft_session_composer_draft(Path::new("/workspace"), "draft", 20)
            .await
            .expect("draft");
        catalog.close().await.expect("close");
        std::fs::write(namespace_dir.join("catalog.db-wal"), vec![0_u8; 4096])
            .expect("oversized fixture sidecar");

        let reports = retired_catalog_reports(state.path(), &session_root, false)
            .await
            .expect("inventory");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].namespace, namespace);
        assert_eq!(reports[0].wal_bytes, 4096);
        assert_eq!(reports[0].draft_rows, 0);
        assert_eq!(reports[0].action, RetiredCatalogAction::Failed);
        assert!(reports[0].error.is_some());
        assert!(namespace_dir.exists());
    }

    #[tokio::test]
    async fn cleanup_preserves_newer_active_draft_as_conflict() {
        let state = tempfile::tempdir().expect("state");
        let session_root = state.path().join("sessions");
        let namespace_dir = session_root.join("catalogs").join("retired-build");
        std::fs::create_dir_all(&namespace_dir).expect("namespace");
        let active = bcode_session::db::GlobalSessionDb::initialize_turso_in_root(&session_root)
            .await
            .expect("active");
        active
            .set_draft_session_composer_draft(Path::new("/workspace"), "newer active", 30)
            .await
            .expect("active draft");
        active.close().await.expect("active closes");
        let retired = bcode_session::db::GlobalSessionDb::open_turso_without_catalog_lock(
            &namespace_dir.join("catalog.db"),
        )
        .await
        .expect("retired");
        retired
            .set_draft_session_composer_draft(Path::new("/workspace"), "older retired", 20)
            .await
            .expect("retired draft");
        retired.close().await.expect("retired closes");

        let reports = retired_catalog_reports(state.path(), &session_root, true)
            .await
            .expect("cleanup");
        assert_eq!(reports[0].action, RetiredCatalogAction::Removed);
        assert_eq!(reports[0].migrated_drafts, 0);
        assert_eq!(reports[0].skipped_draft_conflicts, 1);
        let active = bcode_session::db::GlobalSessionDb::open_existing_turso_in_root(&session_root)
            .await
            .expect("active reopens");
        assert_eq!(
            active
                .draft_session_composer_draft(Path::new("/workspace"))
                .await
                .expect("draft reads")
                .as_deref(),
            Some("newer active")
        );
        active.close().await.expect("active closes");
    }

    #[tokio::test]
    async fn malformed_daemon_registry_evidence_refuses_cleanup() {
        let state = tempfile::tempdir().expect("state");
        let session_root = state.path().join("sessions");
        let namespace_dir = session_root.join("catalogs").join("retired-build");
        std::fs::create_dir_all(&namespace_dir).expect("namespace");
        let retired = bcode_session::db::GlobalSessionDb::open_turso_without_catalog_lock(
            &namespace_dir.join("catalog.db"),
        )
        .await
        .expect("retired");
        retired.close().await.expect("retired closes");
        let registry = bcode_daemon_lifecycle::registry_dir(state.path());
        std::fs::create_dir_all(&registry).expect("registry");
        std::fs::write(registry.join("ambiguous.json"), b"not-json").expect("record");

        let reports = retired_catalog_reports(state.path(), &session_root, true)
            .await
            .expect("cleanup report");
        assert_eq!(
            reports[0].action,
            RetiredCatalogAction::RefusedLiveOrAmbiguousDaemon
        );
        assert_eq!(
            reports[0].daemon_evidence,
            CatalogDaemonEvidence::LiveOrAmbiguous
        );
        assert!(namespace_dir.exists());
    }

    #[tokio::test]
    async fn cleanup_migrates_newer_draft_and_is_idempotent() {
        let state = tempfile::tempdir().expect("state");
        let session_root = state.path().join("sessions");
        let namespace_dir = session_root.join("catalogs").join("retired-build");
        std::fs::create_dir_all(&namespace_dir).expect("namespace");
        let active = bcode_session::db::GlobalSessionDb::initialize_turso_in_root(&session_root)
            .await
            .expect("active");
        active
            .set_draft_session_composer_draft(Path::new("/workspace"), "older", 10)
            .await
            .expect("active draft");
        active.close().await.expect("active closes");
        let retired = bcode_session::db::GlobalSessionDb::open_turso_without_catalog_lock(
            &namespace_dir.join("catalog.db"),
        )
        .await
        .expect("retired");
        retired
            .set_draft_session_composer_draft(Path::new("/workspace"), "newer", 20)
            .await
            .expect("retired draft");
        retired.close().await.expect("retired closes");

        let reports = retired_catalog_reports(state.path(), &session_root, true)
            .await
            .expect("cleanup");
        assert_eq!(reports[0].action, RetiredCatalogAction::Removed);
        assert_eq!(reports[0].migrated_drafts, 1);
        assert!(!namespace_dir.exists());
        assert!(
            retired_catalog_reports(state.path(), &session_root, true)
                .await
                .expect("second cleanup")
                .is_empty()
        );
        let active = bcode_session::db::GlobalSessionDb::open_existing_turso_in_root(&session_root)
            .await
            .expect("active reopens");
        assert_eq!(
            active
                .draft_session_composer_draft(Path::new("/workspace"))
                .await
                .expect("draft reads")
                .as_deref(),
            Some("newer")
        );
        active.close().await.expect("active closes");
    }
}
