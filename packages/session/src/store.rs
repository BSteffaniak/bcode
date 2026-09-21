//! Filesystem-rooted bounded session catalog and manifest store.

use crate::{
    CURRENT_SESSION_FORMAT_EPOCH, SessionFormatMarker, SessionManifest, SessionState,
    SessionTitleSource, canonical_session_id_from_dir, db,
    lease::{self, SessionLeaseOwnerContext},
};
use bcode_metrics::MetricsRegistry;
use bcode_session_models::{SessionId, SessionSummary};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
use thiserror::Error;

use crate::{SESSION_FORMAT_FAMILY, SESSION_MANIFEST_SCHEMA_VERSION};

/// Errors returned by the session store.
#[derive(Debug, Error)]
pub enum SessionStoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("blocking session store task failed: {0}")]
    BlockingTask(#[from] tokio::task::JoinError),
    #[error("session catalog load failed: {0}")]
    CatalogLoad(String),
    #[error(transparent)]
    Lease(#[from] lease::SessionLeaseError),
}

/// Filesystem-rooted session store for DB-backed session histories.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
    pub(crate) metrics: MetricsRegistry,
    lease_owner: SessionLeaseOwnerContext,
}

impl SessionStore {
    /// Create an event store rooted at the provided directory.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            metrics: MetricsRegistry::default(),
            lease_owner: SessionLeaseOwnerContext::default(),
        }
    }

    /// Create an event store rooted at the provided directory with metrics instrumentation.
    #[must_use]
    pub fn with_metrics(root: impl Into<PathBuf>, metrics: MetricsRegistry) -> Self {
        Self {
            root: root.into(),
            metrics,
            lease_owner: SessionLeaseOwnerContext::default(),
        }
    }

    /// Discover bounded session summaries for read-only aggregated catalog display.
    ///
    /// This is the discovery path for a state location this process does not own. It
    /// reads only derived per-session manifests plus canonical directory presence, so it
    /// opens no canonical database, acquires no lease or lock, and performs no maintenance
    /// or history replay. Aggregated discovery confers no authority: a session's
    /// mutations and ownership still belong to the location that owns its canonical
    /// storage.
    ///
    /// A missing root yields an empty list rather than an error, so an unmounted volume
    /// is reported as an empty or degraded source instead of failing the whole catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the root exists but cannot be enumerated.
    pub fn discover_readable_session_summaries(
        &self,
    ) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let mut summaries = self.load_session_manifests()?;
        let manifested = summaries
            .iter()
            .map(|summary| summary.id)
            .collect::<BTreeSet<_>>();
        for summary in self.discover_canonical_session_summaries()? {
            if !manifested.contains(&summary.id) {
                summaries.push(summary);
            }
        }
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.updated_at_ms));
        Ok(summaries)
    }

    /// Load one catalog-backed actor seed without enumerating unrelated sessions when
    /// its current manifest is available. Canonical existence remains required.
    pub(crate) fn load_catalog_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionState>, SessionStoreError> {
        if db::session_db_path(&self.root, session_id).is_file()
            && let Ok(Some(summary)) = self.load_session_manifest(session_id)
        {
            return Ok(Some(SessionState::from_catalog_summary(summary)));
        }
        // Preserve the existing degraded discovery behavior for absent/damaged metadata.
        self.load_catalog()
            .map(|mut catalog| catalog.remove(&session_id))
    }

    pub(crate) fn load_catalog(
        &self,
    ) -> Result<BTreeMap<SessionId, SessionState>, SessionStoreError> {
        let mut summaries = if self.catalog_db_path().exists() {
            match self.load_global_catalog_summaries() {
                Ok(summaries) => summaries
                    .into_iter()
                    .filter(|summary| db::session_dir_path(&self.root, summary.id).exists())
                    .collect(),
                Err(error) => {
                    eprintln!("ignoring unreadable derived session catalog: {error}");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let manifests = self
            .load_session_manifests()?
            .into_iter()
            .map(|summary| (summary.id, summary))
            .collect::<BTreeMap<_, _>>();
        summaries.extend(manifests.values().cloned());
        summaries.extend(self.discover_canonical_session_summaries()?);
        summaries.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| right.updated_at_ms.cmp(&left.updated_at_ms))
        });
        summaries.dedup_by_key(|summary| summary.id);

        let mut sessions = BTreeMap::new();
        for summary in summaries {
            // Manifests carry fields absent from catalog rows and remain preferred even
            // when their timestamp is older. Reuse the discovery snapshot rather than
            // reopening and decoding every manifest a second time.
            let summary = manifests.get(&summary.id).cloned().unwrap_or(summary);
            sessions.insert(summary.id, SessionState::from_catalog_summary(summary));
        }
        Ok(sessions)
    }

    pub(crate) fn backfill_catalog(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let mut summaries = self.load_session_manifests()?;
        summaries.extend(self.discover_canonical_session_summaries()?);
        summaries.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| right.updated_at_ms.cmp(&left.updated_at_ms))
        });
        summaries.dedup_by_key(|summary| summary.id);
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.updated_at_ms));
        if summaries.is_empty() {
            return Ok(summaries);
        }
        Ok(summaries)
    }

    fn discover_canonical_session_summaries(
        &self,
    ) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let mut summaries = Vec::new();
        if !self.root.exists() {
            return Ok(summaries);
        }
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            let Some(session_id) = canonical_session_id_from_dir(&path) else {
                continue;
            };
            if !db::session_db_path(&self.root, session_id).exists() {
                continue;
            }
            summaries.push(SessionSummary {
                id: session_id,
                name: None,
                explicit_name: None,
                derived_title: None,
                title_source: SessionTitleSource::EmptyDraft,
                client_count: 0,
                created_at_ms: 0,
                updated_at_ms: 0,
                working_directory: self.root.clone(),
                import: None,
                execution: None,
                location: None,
            });
        }
        Ok(summaries)
    }

    fn load_session_manifests(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let mut summaries = Vec::new();
        if !self.root.exists() {
            return Ok(summaries);
        }
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            let Some(session_id) = canonical_session_id_from_dir(&path) else {
                continue;
            };
            match self.load_session_manifest(session_id) {
                Ok(Some(summary)) => summaries.push(summary),
                Ok(None) => {}
                Err(error) => {
                    eprintln!("skipping unreadable session manifest {session_id}: {error}");
                }
            }
        }
        Ok(summaries)
    }

    pub(crate) fn load_session_manifest(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionSummary>, SessionStoreError> {
        let path = self.session_manifest_path(session_id);
        if !path.exists() {
            return Ok(None);
        }
        let contents = fs::read(&path)?;
        let value: serde_json::Value = serde_json::from_slice(&contents)
            .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
        let schema_version = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64);
        if schema_version == Some(1) {
            let summary: SessionSummary =
                serde_json::from_value(value.get("summary").cloned().ok_or_else(|| {
                    SessionStoreError::CatalogLoad(
                        "legacy session manifest is missing its summary".to_owned(),
                    )
                })?)
                .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
            if summary.id != session_id {
                return Err(SessionStoreError::CatalogLoad(format!(
                    "session manifest id mismatch: expected {session_id}, found {}",
                    summary.id
                )));
            }
            return Ok(Some(summary));
        }
        if schema_version != Some(u64::from(SESSION_MANIFEST_SCHEMA_VERSION)) {
            return Err(SessionStoreError::CatalogLoad(format!(
                "unsupported session manifest schema version {schema_version:?}"
            )));
        }
        let format_family = value
            .pointer("/session_format/family")
            .and_then(serde_json::Value::as_str);
        let format_epoch = value
            .pointer("/session_format/epoch")
            .and_then(serde_json::Value::as_u64);
        if format_family != Some(SESSION_FORMAT_FAMILY)
            || format_epoch != Some(u64::from(CURRENT_SESSION_FORMAT_EPOCH))
        {
            return Err(SessionStoreError::CatalogLoad(format!(
                "unsupported session format family={format_family:?} epoch={format_epoch:?}"
            )));
        }
        let manifest: SessionManifest = serde_json::from_value(value)
            .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
        if manifest.schema_version != SESSION_MANIFEST_SCHEMA_VERSION {
            return Err(SessionStoreError::CatalogLoad(format!(
                "unsupported session manifest schema version {}",
                manifest.schema_version
            )));
        }
        if manifest.session_format.family != SESSION_FORMAT_FAMILY
            || manifest.session_format.epoch != CURRENT_SESSION_FORMAT_EPOCH
        {
            return Err(SessionStoreError::CatalogLoad(format!(
                "unsupported session format family={} epoch={}",
                manifest.session_format.family, manifest.session_format.epoch
            )));
        }
        if manifest.summary.id != session_id {
            return Err(SessionStoreError::CatalogLoad(format!(
                "session manifest id mismatch: expected {session_id}, found {}",
                manifest.summary.id
            )));
        }
        Ok(Some(manifest.summary))
    }

    pub(crate) fn write_session_manifest(
        &self,
        summary: &SessionSummary,
    ) -> Result<(), SessionStoreError> {
        let path = self.session_manifest_path(summary.id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut summary = summary.clone();
        summary.client_count = 0;
        let manifest = SessionManifest {
            schema_version: SESSION_MANIFEST_SCHEMA_VERSION,
            session_format: SessionFormatMarker {
                family: SESSION_FORMAT_FAMILY.to_owned(),
                epoch: CURRENT_SESSION_FORMAT_EPOCH,
            },
            summary,
        };
        let temp_path = path.with_extension("json.tmp");
        fs::write(
            &temp_path,
            serde_json::to_vec_pretty(&manifest)
                .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?,
        )?;
        fs::rename(&temp_path, path)?;
        Ok(())
    }

    fn session_manifest_path(&self, session_id: SessionId) -> PathBuf {
        db::session_dir_path(&self.root, session_id).join("manifest.json")
    }

    fn catalog_db_path(&self) -> PathBuf {
        db::global_catalog_db_path(&self.root)
    }

    fn load_global_catalog_summaries(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let root = self.root.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
            runtime.block_on(async move {
                let catalog = db::GlobalSessionDb::open_existing_turso_in_root(&root)
                    .await
                    .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
                let summaries = catalog
                    .list_sessions()
                    .await
                    .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()));
                let close = catalog
                    .close()
                    .await
                    .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()));
                let summaries = summaries?;
                close?;
                Ok(summaries)
            })
        })
        .join()
        .map_err(|_| SessionStoreError::CatalogLoad("global catalog loader panicked".to_string()))?
    }

    pub(crate) fn root(&self) -> &Path {
        self.root.as_path()
    }

    pub(crate) fn with_lease_owner(mut self, lease_owner: SessionLeaseOwnerContext) -> Self {
        self.lease_owner = lease_owner;
        self
    }

    pub(crate) const fn lease_owner(&self) -> &SessionLeaseOwnerContext {
        &self.lease_owner
    }
}

#[cfg(test)]
mod tests {
    use super::SessionStore;
    use bcode_session_models::SessionId;

    #[test]
    fn catalog_discovery_preserves_manifest_metadata_and_damaged_neighbors() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path());
        let ids = [SessionId::new(), SessionId::new(), SessionId::new()];
        for id in ids {
            let directory = temp.path().join(id.to_string());
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("session.db"), b"never opened").unwrap();
        }
        let mut summary = store
            .discover_canonical_session_summaries()
            .unwrap()
            .into_iter()
            .find(|summary| summary.id == ids[0])
            .unwrap();
        summary.name = Some("manifest title".to_owned());
        summary.updated_at_ms = 1;
        summary.working_directory = temp.path().join("project");
        store.write_session_manifest(&summary).unwrap();
        let damaged = temp.path().join(ids[1].to_string()).join("manifest.json");
        std::fs::write(&damaged, b"broken").unwrap();

        let catalog = store.load_catalog().unwrap();
        assert_eq!(catalog.len(), 3);
        assert_eq!(catalog[&ids[0]].summary, summary);
        assert_eq!(std::fs::read(damaged).unwrap(), b"broken");
        for id in ids {
            assert_eq!(
                std::fs::read(temp.path().join(id.to_string()).join("session.db")).unwrap(),
                b"never opened"
            );
        }
    }

    #[test]
    fn targeted_manifest_lookup_preserves_summary_without_opening_database() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path());
        let id = SessionId::new();
        let directory = temp.path().join(id.to_string());
        std::fs::create_dir_all(&directory).unwrap();
        let database = directory.join("session.db");
        std::fs::write(&database, b"not opened by metadata lookup").unwrap();
        let mut summary = store
            .discover_canonical_session_summaries()
            .unwrap()
            .remove(0);
        summary.name = Some("target".to_owned());
        summary.updated_at_ms = 123;
        store.write_session_manifest(&summary).unwrap();
        let before = std::fs::read(&database).unwrap();
        let targeted = store.load_catalog_session(id).unwrap().unwrap();
        let catalog = store.load_catalog().unwrap().remove(&id).unwrap();
        assert_eq!(targeted.summary, catalog.summary);
        assert_eq!(targeted.summary, summary);
        assert_eq!(std::fs::read(&database).unwrap(), before);
        std::fs::write(directory.join("manifest.json"), b"broken").unwrap();
        assert!(store.load_catalog_session(id).unwrap().is_some());
        assert_eq!(
            std::fs::read(directory.join("manifest.json")).unwrap(),
            b"broken"
        );
        assert!(
            store
                .load_catalog_session(SessionId::new())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn readable_discovery_of_a_missing_root_is_empty_and_non_fatal() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let absent = temp.path().join("unmounted-volume").join("sessions");
        let store = SessionStore::new(&absent);

        let summaries = store
            .discover_readable_session_summaries()
            .expect("a missing root is reported as empty, not as an error");

        assert!(summaries.is_empty());
        assert!(
            !absent.exists(),
            "read-only discovery must not create the root it inspects"
        );
    }

    #[test]
    fn readable_discovery_finds_canonical_sessions_without_opening_or_mutating_them() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let root = temp.path().join("sessions");
        let session_id = SessionId::new();
        let session_dir = root.join(session_id.to_string());
        std::fs::create_dir_all(&session_dir).expect("session directory");
        let database = session_dir.join("session.db");
        std::fs::write(&database, b"canonical-bytes").expect("session database");
        let before = std::fs::read(&database).expect("read database");

        let store = SessionStore::new(&root);
        let summaries = store
            .discover_readable_session_summaries()
            .expect("discovery succeeds");

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, session_id);
        assert_eq!(
            std::fs::read(&database).expect("read database after discovery"),
            before,
            "discovery must not rewrite or otherwise mutate canonical storage"
        );
        assert!(
            !root.join("catalog.db").exists(),
            "discovery must not create a derived catalog in a location it does not own"
        );
        assert!(
            !root.join("leases").exists() && !root.join("locks").exists(),
            "discovery must not take leases or locks in a foreign location"
        );
    }

    #[test]
    fn discovery_retains_directory_formats_without_interpreting_or_repairing_them() {
        let temp = tempfile::tempdir().expect("root");
        let root = temp.path();
        let store = SessionStore::new(root);
        let mut expected = std::collections::BTreeSet::new();
        for marker in [
            b"BCODE_SESSION_DB 1\n".as_slice(),
            b"BCODE_SESSION_DB 2\n",
            b"broken",
        ] {
            let id = SessionId::new();
            expected.insert(id);
            let directory = root.join(id.to_string()).join("session.db");
            std::fs::create_dir_all(&directory).expect("directory");
            std::fs::write(directory.join("format"), marker).expect("marker");
            // Missing inner databases must remain discoverable, not disappear as empty sessions.
        }
        let summaries = store
            .discover_readable_session_summaries()
            .expect("discover");
        assert_eq!(
            summaries
                .into_iter()
                .map(|summary| summary.id)
                .collect::<std::collections::BTreeSet<_>>(),
            expected
        );
        for id in expected {
            let directory = root.join(id.to_string()).join("session.db");
            assert!(!directory.join("data.db").exists());
            assert_eq!(std::fs::read_dir(directory).unwrap().count(), 1);
        }
        assert!(!root.join("catalog.db").exists());
        assert!(!root.join("leases").exists());
        assert!(!root.join("locks").exists());
    }

    #[test]
    fn readable_discovery_ignores_directories_that_are_not_session_ids() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let root = temp.path().join("sessions");
        let session_id = SessionId::new();
        std::fs::create_dir_all(root.join(session_id.to_string())).expect("session directory");
        std::fs::write(root.join(session_id.to_string()).join("session.db"), b"db")
            .expect("session database");
        // Sibling directories such as session artifacts must not be mistaken for sessions.
        std::fs::create_dir_all(root.join("session-artifacts").join(session_id.to_string()))
            .expect("artifact directory");
        std::fs::create_dir_all(root.join("leases")).expect("leases");

        let store = SessionStore::new(&root);
        let summaries = store
            .discover_readable_session_summaries()
            .expect("discovery succeeds");

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, session_id);
    }

    #[test]
    fn readable_discovery_skips_session_directories_without_canonical_storage() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let root = temp.path().join("sessions");
        let session_id = SessionId::new();
        std::fs::create_dir_all(root.join(session_id.to_string())).expect("session directory");

        let store = SessionStore::new(&root);
        let summaries = store
            .discover_readable_session_summaries()
            .expect("discovery succeeds");

        assert!(
            summaries.is_empty(),
            "a directory without session.db is not a discoverable session"
        );
    }
}
