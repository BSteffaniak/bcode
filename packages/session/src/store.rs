//! Filesystem-rooted bounded session catalog and manifest store.

use crate::{
    CURRENT_SESSION_FORMAT_EPOCH, SessionFormatMarker, SessionManifest, SessionState,
    SessionTitleSource, canonical_session_id_from_dir, db,
    lease::{self, SessionLeaseOwnerContext},
    safe_catalog_namespace,
};
use bcode_metrics::MetricsRegistry;
use bcode_session_models::{SessionId, SessionSummary};
use std::{
    collections::BTreeMap,
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

    pub(crate) fn load_catalog(
        &self,
    ) -> Result<BTreeMap<SessionId, SessionState>, SessionStoreError> {
        let mut summaries = if self.catalog_db_path().exists() {
            match self.load_global_catalog_summaries() {
                Ok(summaries) => summaries,
                Err(error) => {
                    eprintln!("ignoring unreadable derived session catalog: {error}");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        summaries.extend(self.load_session_manifests()?);
        summaries.extend(self.discover_canonical_session_summaries()?);
        match self.load_legacy_catalog_summaries() {
            Ok(legacy) => summaries.extend(legacy),
            Err(error) => eprintln!("ignoring unreadable legacy session catalog: {error}"),
        }
        summaries.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| right.updated_at_ms.cmp(&left.updated_at_ms))
        });
        summaries.dedup_by_key(|summary| summary.id);

        let mut sessions = BTreeMap::new();
        for summary in summaries {
            let summary = match self.load_session_manifest(summary.id) {
                Ok(Some(manifest_summary)) => manifest_summary,
                Ok(None) => summary,
                Err(error) => {
                    eprintln!(
                        "using canonical fallback for session {} with unreadable manifest metadata: {error}",
                        summary.id
                    );
                    summary
                }
            };
            sessions.insert(summary.id, SessionState::from_catalog_summary(summary));
        }
        Ok(sessions)
    }

    pub(crate) fn backfill_catalog(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let mut summaries = self.load_session_manifests()?;
        summaries.extend(self.discover_canonical_session_summaries()?);
        match self.load_legacy_catalog_summaries() {
            Ok(legacy) => summaries.extend(legacy),
            Err(error) => eprintln!("ignoring unreadable legacy session catalog: {error}"),
        }
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

        self.write_global_catalog_summaries(&summaries)?;
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
                fork: None,
                execution: None,
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

    fn load_legacy_catalog_summaries(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        if self.catalog_namespace().is_none() || !db::global_catalog_db_path(&self.root).exists() {
            return Ok(Vec::new());
        }
        Self::load_catalog_summaries_at_path(db::global_catalog_db_path(&self.root))
    }

    fn load_catalog_summaries_at_path(
        path: PathBuf,
    ) -> Result<Vec<SessionSummary>, SessionStoreError> {
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
            runtime.block_on(async move {
                let catalog = db::GlobalSessionDb::open_turso_without_catalog_lock(&path)
                    .await
                    .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
                catalog
                    .list_sessions()
                    .await
                    .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))
            })
        })
        .join()
        .map_err(|_| SessionStoreError::CatalogLoad("catalog loader panicked".to_string()))?
    }

    fn write_global_catalog_summaries(
        &self,
        summaries: &[SessionSummary],
    ) -> Result<(), SessionStoreError> {
        let root = self.root.clone();
        let namespace = self.catalog_namespace();
        let summaries = summaries.to_vec();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
            runtime.block_on(async move {
                let catalog = match namespace.as_deref() {
                    Some(namespace) => {
                        db::GlobalSessionDb::open_turso_in_root_namespace(&root, namespace).await
                    }
                    None => db::GlobalSessionDb::open_turso_in_root(&root).await,
                }
                .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
                for summary in summaries {
                    catalog
                        .upsert_session(&summary, &db::session_db_path(&root, summary.id))
                        .await
                        .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
                }
                Ok(())
            })
        })
        .join()
        .map_err(|_| SessionStoreError::CatalogLoad("catalog writer panicked".to_string()))?
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

    fn catalog_namespace(&self) -> Option<String> {
        self.lease_owner
            .build_fingerprint
            .as_deref()
            .map(safe_catalog_namespace)
    }

    fn catalog_db_path(&self) -> PathBuf {
        self.catalog_namespace().map_or_else(
            || db::global_catalog_db_path(&self.root),
            |namespace| db::namespaced_catalog_db_path(&self.root, &namespace),
        )
    }

    fn load_global_catalog_summaries(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let root = self.root.clone();
        let namespace = self.catalog_namespace();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
            runtime.block_on(async move {
                let catalog = match namespace.as_deref() {
                    Some(namespace) => {
                        db::GlobalSessionDb::open_turso_in_root_namespace(&root, namespace).await
                    }
                    None => db::GlobalSessionDb::open_turso_in_root(&root).await,
                }
                .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))?;
                catalog
                    .list_sessions()
                    .await
                    .map_err(|error| SessionStoreError::CatalogLoad(error.to_string()))
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
