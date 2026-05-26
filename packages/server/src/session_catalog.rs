//! Unified non-blocking session catalog snapshots.

use crate::{ServerState, catalog_status_to_ipc};
use bcode_ipc::{SessionCatalogSourceStatus, SessionCatalogStatus};
use bcode_session_import::ImportableSessionStatus;
use bcode_session_models::{SessionImportSummary, SessionSummary};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Notify, watch};

const NATIVE_SOURCE_ID: &str = "native";

/// Server-owned session catalog cache.
#[derive(Debug)]
pub struct SessionCatalog {
    inner: Mutex<SessionCatalogInner>,
    revision_tx: watch::Sender<u64>,
    revision_rx: watch::Receiver<u64>,
    notify: Notify,
}

#[derive(Debug, Default)]
struct SessionCatalogInner {
    revision: u64,
    native: SourceState,
    imports: BTreeMap<ImportScopeKey, SourceState>,
}

#[derive(Debug, Default, Clone)]
struct SourceState {
    status: SessionCatalogStatus,
    sessions: Vec<SessionSummary>,
    requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ImportScopeKey {
    source_id: String,
    working_directory: PathBuf,
}

/// Point-in-time catalog response for one working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCatalogSnapshot {
    pub sessions: Vec<SessionSummary>,
    pub status: SessionCatalogStatus,
    pub sources: Vec<SessionCatalogSourceStatus>,
    pub revision: u64,
}

impl Default for SessionCatalog {
    fn default() -> Self {
        let (revision_tx, revision_rx) = watch::channel(0);
        Self {
            inner: Mutex::default(),
            revision_tx,
            revision_rx,
            notify: Notify::new(),
        }
    }
}

impl SessionCatalog {
    /// Subscribe to catalog revision changes.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision_rx.clone()
    }

    /// Return the current coherent catalog snapshot for a working directory.
    pub async fn snapshot(
        &self,
        state: &Arc<ServerState>,
        working_directory: &Path,
    ) -> SessionCatalogSnapshot {
        self.ensure_native_refresh(state).await;
        self.ensure_import_refresh(state, working_directory).await;
        let mut inner = self.inner.lock().await;
        sync_native_status_locked(&mut inner, state);
        snapshot_locked(&inner, working_directory)
    }

    /// Mark the catalog dirty after a native session mutation.
    pub async fn refresh_native_now(&self, state: &Arc<ServerState>) {
        let sessions = state.sessions.all_session_summaries().await;
        let status = catalog_status_to_ipc(state.sessions.catalog_status());
        self.apply_native(sessions, status).await;
    }

    /// Force a catalog refresh for the selected sources.
    pub async fn refresh(
        &self,
        state: &Arc<ServerState>,
        working_directory: &Path,
        sources: Option<&[String]>,
    ) -> SessionCatalogSnapshot {
        self.invalidate_sources(working_directory, sources).await;
        self.snapshot(state, working_directory).await
    }

    async fn invalidate_sources(&self, working_directory: &Path, sources: Option<&[String]>) {
        let mut inner = self.inner.lock().await;
        let refresh_all = sources.is_none_or(<[String]>::is_empty);
        let should_refresh = |source_id: &str| {
            refresh_all
                || sources.is_some_and(|sources| sources.iter().any(|source| source == source_id))
        };
        if should_refresh(NATIVE_SOURCE_ID) {
            inner.native.requested = false;
            inner.native.status = SessionCatalogStatus::NotStarted;
        }
        for (key, source) in &mut inner.imports {
            if key.working_directory == working_directory && should_refresh(&key.source_id) {
                source.requested = false;
                source.status = SessionCatalogStatus::NotStarted;
                source.sessions.clear();
            }
        }
        inner.revision = inner.revision.saturating_add(1);
        let _ = self.revision_tx.send(inner.revision);
        drop(inner);
        self.notify.notify_waiters();
    }

    async fn ensure_native_refresh(&self, state: &Arc<ServerState>) {
        let should_spawn = {
            let mut inner = self.inner.lock().await;
            sync_native_status_locked(&mut inner, state);
            if matches!(
                inner.native.status,
                SessionCatalogStatus::Loaded | SessionCatalogStatus::Loading
            ) || inner.native.requested
            {
                false
            } else {
                inner.native.requested = true;
                inner.native.status = SessionCatalogStatus::Loading;
                true
            }
        };
        if !should_spawn {
            return;
        }
        self.bump();
        state.sessions.start_catalog_load();
        let state = Arc::clone(state);
        let catalog = Arc::clone(&state.session_catalog);
        tokio::spawn(async move {
            let result = state.sessions.wait_catalog_loaded().await;
            let status = match result {
                Ok(()) => SessionCatalogStatus::Loaded,
                Err(error) => SessionCatalogStatus::Failed(error.to_string()),
            };
            let sessions = state.sessions.all_session_summaries().await;
            catalog.apply_native(sessions, status).await;
        });
    }

    async fn ensure_import_refresh(&self, state: &Arc<ServerState>, working_directory: &Path) {
        if !bcode_config::load_config().map_or(true, |config| config.session_import.enabled) {
            return;
        }
        let providers = state
            .plugins
            .registry()
            .service_registry()
            .providers_for(bcode_session_import::SESSION_IMPORT_INTERFACE_ID)
            .cloned()
            .unwrap_or_default();
        for plugin_id in providers {
            let source_ids = import_source_ids(state, &plugin_id).await;
            for source_id in source_ids {
                self.ensure_import_source_refresh(
                    state,
                    plugin_id.clone(),
                    source_id,
                    working_directory.to_path_buf(),
                )
                .await;
            }
        }
    }

    #[allow(clippy::significant_drop_tightening)]
    async fn ensure_import_source_refresh(
        &self,
        state: &Arc<ServerState>,
        plugin_id: String,
        source_id: String,
        working_directory: PathBuf,
    ) {
        let key = ImportScopeKey {
            source_id: source_id.clone(),
            working_directory: working_directory.clone(),
        };
        let should_spawn = {
            let mut inner = self.inner.lock().await;
            let source = inner.imports.entry(key.clone()).or_default();
            if source.requested
                || matches!(
                    source.status,
                    SessionCatalogStatus::Loaded | SessionCatalogStatus::Loading
                )
            {
                false
            } else {
                source.requested = true;
                source.status = SessionCatalogStatus::Loading;
                true
            }
        };
        if !should_spawn {
            return;
        }
        self.bump();
        let state = Arc::clone(state);
        let catalog = Arc::clone(&state.session_catalog);
        tokio::spawn(async move {
            let result =
                discover_import_source(&state, &plugin_id, &source_id, &working_directory).await;
            match result {
                Ok(sessions) => {
                    catalog
                        .apply_import(key, sessions, SessionCatalogStatus::Loaded)
                        .await;
                }
                Err(error) => {
                    eprintln!("failed to discover import source {source_id}: {error}");
                    catalog
                        .apply_import(key, Vec::new(), SessionCatalogStatus::Failed(error))
                        .await;
                }
            }
        });
    }

    async fn apply_native(&self, sessions: Vec<SessionSummary>, status: SessionCatalogStatus) {
        {
            let mut inner = self.inner.lock().await;
            inner.native.sessions = sessions;
            inner.native.status = status;
            inner.native.requested = true;
            inner.revision = inner.revision.saturating_add(1);
            let _ = self.revision_tx.send(inner.revision);
        }
        self.notify.notify_waiters();
    }

    async fn apply_import(
        &self,
        key: ImportScopeKey,
        sessions: Vec<SessionSummary>,
        status: SessionCatalogStatus,
    ) {
        {
            let mut inner = self.inner.lock().await;
            let source = inner.imports.entry(key).or_default();
            source.sessions = sessions;
            source.status = status;
            source.requested = true;
            inner.revision = inner.revision.saturating_add(1);
            let _ = self.revision_tx.send(inner.revision);
        }
        self.notify.notify_waiters();
    }

    fn bump(&self) {
        if let Ok(mut inner) = self.inner.try_lock() {
            inner.revision = inner.revision.saturating_add(1);
            let _ = self.revision_tx.send(inner.revision);
        }
        self.notify.notify_waiters();
    }
}

fn sync_native_status_locked(inner: &mut SessionCatalogInner, state: &ServerState) {
    let status = catalog_status_to_ipc(state.sessions.catalog_status());
    if inner.native.status != status {
        inner.native.status = status;
    }
}

fn snapshot_locked(
    inner: &SessionCatalogInner,
    working_directory: &Path,
) -> SessionCatalogSnapshot {
    let native_imports = native_import_identities(&inner.native.sessions);
    let hide_imported = bcode_config::load_config()
        .map_or(true, |config| config.session_import.hide_already_imported);
    let mut sessions = inner
        .native
        .sessions
        .iter()
        .filter(|session| session.working_directory == working_directory)
        .cloned()
        .collect::<Vec<_>>();
    let mut sources = vec![source_status(
        NATIVE_SOURCE_ID,
        "Native Bcode sessions",
        &inner.native.status,
    )];
    for (key, source) in &inner.imports {
        if key.working_directory != working_directory {
            continue;
        }
        sources.push(source_status(
            &key.source_id,
            format!("Imported [{}] sessions", key.source_id),
            &source.status,
        ));
        sessions.extend(
            source
                .sessions
                .iter()
                .filter(|session| {
                    session.import.as_ref().is_none_or(|import| {
                        !hide_imported
                            || !native_imports.contains(&(
                                import.source_id.clone(),
                                import.external_session_id.clone(),
                            ))
                    })
                })
                .cloned(),
        );
    }
    sort_sessions(&mut sessions);
    SessionCatalogSnapshot {
        status: aggregate_status(sources.iter().map(|source| &source.status)),
        sessions,
        sources,
        revision: inner.revision,
    }
}

fn native_import_identities(sessions: &[SessionSummary]) -> BTreeSet<(String, String)> {
    sessions
        .iter()
        .filter_map(|session| {
            session.import.as_ref().and_then(|import| {
                (import.imported_at_ms != 0)
                    .then(|| (import.source_id.clone(), import.external_session_id.clone()))
            })
        })
        .collect()
}

fn source_status(
    source_id: impl Into<String>,
    display_name: impl Into<String>,
    status: &SessionCatalogStatus,
) -> SessionCatalogSourceStatus {
    SessionCatalogSourceStatus {
        source_id: source_id.into(),
        display_name: display_name.into(),
        status: status.clone(),
        updated_at_ms: current_unix_millis(),
    }
}

fn aggregate_status<'a>(
    statuses: impl IntoIterator<Item = &'a SessionCatalogStatus>,
) -> SessionCatalogStatus {
    let mut has_loading = false;
    let mut failures = Vec::new();
    for status in statuses {
        match status {
            SessionCatalogStatus::NotStarted | SessionCatalogStatus::Loading => has_loading = true,
            SessionCatalogStatus::Failed(message) => failures.push(message.clone()),
            SessionCatalogStatus::Loaded => {}
        }
    }
    if has_loading {
        SessionCatalogStatus::Loading
    } else if failures.is_empty() {
        SessionCatalogStatus::Loaded
    } else {
        SessionCatalogStatus::Failed(failures.join("; "))
    }
}

async fn import_source_ids(state: &ServerState, plugin_id: &str) -> Vec<String> {
    state
        .plugins
        .invoke_service_json::<_, bcode_session_import::ListImportSourcesResponse>(
            plugin_id,
            bcode_session_import::SESSION_IMPORT_INTERFACE_ID,
            bcode_session_import::OP_LIST_IMPORT_SOURCES,
            &serde_json::json!({}),
        )
        .await
        .map(|response| {
            response
                .sources
                .into_iter()
                .map(|source| source.source_id)
                .collect()
        })
        .unwrap_or_default()
}

async fn discover_import_source(
    state: &ServerState,
    plugin_id: &str,
    source_id: &str,
    working_directory: &Path,
) -> Result<Vec<SessionSummary>, String> {
    let response = state
        .plugins
        .invoke_service_json::<_, bcode_session_import::DiscoverImportableSessionsResponse>(
            plugin_id,
            bcode_session_import::SESSION_IMPORT_INTERFACE_ID,
            bcode_session_import::OP_DISCOVER_IMPORTABLE_SESSIONS,
            &bcode_session_import::DiscoverImportableSessionsRequest {
                working_directory: Some(working_directory.to_path_buf()),
                include_diagnostics: false,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(response
        .sessions
        .into_iter()
        .filter(|summary| {
            summary.status == ImportableSessionStatus::Available && summary.source_id == source_id
        })
        .map(importable_to_summary)
        .collect())
}

fn importable_to_summary(
    summary: bcode_session_import::ImportableSessionSummary,
) -> SessionSummary {
    let id = crate::session_import::external_session_id(
        &summary.source_id,
        &summary.external_session_id,
    );
    let name = Some(
        summary
            .title
            .clone()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| summary.external_session_id.clone()),
    );
    SessionSummary {
        id,
        name,
        client_count: 0,
        created_at_ms: summary.created_at_ms.unwrap_or(0),
        updated_at_ms: summary.updated_at_ms.or(summary.created_at_ms).unwrap_or(0),
        working_directory: summary.working_directory.unwrap_or_default(),
        import: Some(SessionImportSummary {
            source_id: summary.source_id,
            source_display_name: summary.source_display_name,
            external_session_id: summary.external_session_id,
            imported_at_ms: 0,
        }),
    }
}

fn sort_sessions(sessions: &mut [SessionSummary]) {
    sessions.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}
