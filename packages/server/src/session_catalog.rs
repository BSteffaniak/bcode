//! Unified non-blocking session catalog snapshots.

use crate::ServerState;
use bcode_ipc::{SessionCatalogSourceStatus, SessionCatalogStatus};
use bcode_session::SessionCatalogEntry;
use bcode_session_import::ImportableSessionStatus;
use bcode_session_models::{SessionId, SessionImportSummary, SessionSummary, SessionTitleSource};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Notify, watch};

const NATIVE_SOURCE_ID: &str = "native";
const NATIVE_DISPLAY_NAME: &str = "Native Bcode sessions";

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
    sources: BTreeMap<CatalogSourceKey, SourceCache>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CatalogSourceKey {
    source_id: String,
    scope: CatalogSourceScope,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CatalogSourceScope {
    Global,
    WorkingDirectory(PathBuf),
}

#[derive(Debug, Clone)]
struct SourceMetadata {
    display_name: String,
}

#[derive(Debug, Clone)]
struct SourceCache {
    metadata: SourceMetadata,
    state: SourceCacheState,
}

#[derive(Debug, Clone)]
enum SourceCacheState {
    Empty,
    Loading,
    Loaded {
        sessions: Vec<SessionSummary>,
        diagnostics: SourceDiagnostics,
    },
    Failed {
        message: String,
        sessions: Vec<SessionSummary>,
        diagnostics: SourceDiagnostics,
    },
}

#[derive(Debug, Clone, Default)]
struct SourceDiagnostics {}

#[derive(Debug, Clone)]
enum CatalogSourcePlan {
    Native,
    Import {
        plugin_id: String,
        source_id: String,
        working_directory: PathBuf,
    },
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
        let working_directory = normalize_path(working_directory);
        self.ensure_sources(state, &working_directory).await;
        let inner = self.inner.lock().await;
        snapshot_locked(&inner, &working_directory)
    }

    /// Replace the native source with a fresh view from the session manager.
    pub async fn refresh_native_now(&self, state: &ServerState) {
        let result = load_native_source(state).await;
        self.apply_source_result(native_source_key(), native_metadata(), result)
            .await;
    }

    /// Mark native sessions dirty so the next snapshot reloads them.
    pub async fn invalidate_native(&self) {
        self.invalidate_source_id(NATIVE_SOURCE_ID).await;
    }

    #[allow(clippy::significant_drop_tightening)]
    /// Update a materialized native session cache after a native session mutation.
    pub async fn upsert_native_session(&self, session: SessionSummary) {
        let changed = {
            let mut inner = self.inner.lock().await;
            let Some(source) = inner.sources.get_mut(&native_source_key()) else {
                return;
            };
            let sessions = match &mut source.state {
                SourceCacheState::Loaded { sessions, .. }
                | SourceCacheState::Failed { sessions, .. } => sessions,
                SourceCacheState::Empty | SourceCacheState::Loading => return,
            };
            upsert_session(sessions, session)
        };
        if changed {
            self.bump_revision().await;
        }
    }

    #[allow(clippy::significant_drop_tightening)]
    /// Remove a native session from a materialized native session cache.
    pub async fn remove_native_session(&self, session_id: SessionId) {
        let changed = {
            let mut inner = self.inner.lock().await;
            let Some(source) = inner.sources.get_mut(&native_source_key()) else {
                return;
            };
            let sessions = match &mut source.state {
                SourceCacheState::Loaded { sessions, .. }
                | SourceCacheState::Failed { sessions, .. } => sessions,
                SourceCacheState::Empty | SourceCacheState::Loading => return,
            };
            let original_len = sessions.len();
            sessions.retain(|session| session.id != session_id);
            sessions.len() != original_len
        };
        if changed {
            self.bump_revision().await;
        }
    }

    /// Force a catalog refresh for the selected sources.
    pub async fn refresh(
        &self,
        state: &Arc<ServerState>,
        working_directory: &Path,
        sources: Option<&[String]>,
    ) -> SessionCatalogSnapshot {
        let working_directory = normalize_path(working_directory);
        self.invalidate_sources(&working_directory, sources).await;
        self.snapshot(state, &working_directory).await
    }

    async fn ensure_sources(&self, state: &Arc<ServerState>, working_directory: &Path) {
        for plan in source_plans(state, working_directory).await {
            self.ensure_source(state, plan).await;
        }
    }

    #[allow(clippy::significant_drop_tightening)]
    async fn ensure_source(&self, state: &Arc<ServerState>, plan: CatalogSourcePlan) {
        let key = plan.key();
        let metadata = plan.metadata();
        let should_spawn = {
            let mut inner = self.inner.lock().await;
            let source = inner
                .sources
                .entry(key.clone())
                .or_insert_with(|| SourceCache {
                    metadata: metadata.clone(),
                    state: SourceCacheState::Empty,
                });
            source.metadata = metadata.clone();
            match source.state {
                SourceCacheState::Empty | SourceCacheState::Failed { .. } => {
                    source.state = SourceCacheState::Loading;
                    true
                }
                SourceCacheState::Loading | SourceCacheState::Loaded { .. } => false,
            }
        };
        if !should_spawn {
            return;
        }
        self.bump_revision().await;
        let state = Arc::clone(state);
        let catalog = Arc::clone(&state.session_catalog);
        tokio::spawn(async move {
            let key = plan.key();
            let metadata = plan.metadata();
            let result = load_source(&state, &plan).await;
            catalog.apply_source_result(key, metadata, result).await;
        });
    }

    async fn invalidate_sources(&self, working_directory: &Path, sources: Option<&[String]>) {
        let refresh_all = sources.is_none_or(<[String]>::is_empty);
        let should_refresh = |source_id: &str| {
            refresh_all
                || sources.is_some_and(|sources| sources.iter().any(|source| source == source_id))
        };
        let mut changed = false;
        {
            let mut inner = self.inner.lock().await;
            for (key, source) in &mut inner.sources {
                let in_scope = matches!(key.scope, CatalogSourceScope::Global)
                    || matches!(&key.scope, CatalogSourceScope::WorkingDirectory(path) if path == working_directory);
                if in_scope && should_refresh(&key.source_id) {
                    source.state = SourceCacheState::Empty;
                    changed = true;
                }
            }
        }
        if changed {
            self.bump_revision().await;
        }
    }

    async fn invalidate_source_id(&self, source_id: &str) {
        let mut changed = false;
        {
            let mut inner = self.inner.lock().await;
            for (key, source) in &mut inner.sources {
                if key.source_id == source_id {
                    source.state = SourceCacheState::Empty;
                    changed = true;
                }
            }
        }
        if changed {
            self.bump_revision().await;
        }
    }

    #[allow(clippy::significant_drop_tightening)]
    async fn apply_source_result(
        &self,
        key: CatalogSourceKey,
        metadata: SourceMetadata,
        result: Result<SourceLoadResult, String>,
    ) {
        {
            let mut inner = self.inner.lock().await;
            let source = inner.sources.entry(key).or_insert_with(|| SourceCache {
                metadata: metadata.clone(),
                state: SourceCacheState::Empty,
            });
            source.metadata = metadata;
            source.state = match result {
                Ok(result) => SourceCacheState::Loaded {
                    sessions: result.sessions,
                    diagnostics: result.diagnostics,
                },
                Err(message) => SourceCacheState::Failed {
                    message,
                    sessions: Vec::new(),
                    diagnostics: SourceDiagnostics::default(),
                },
            };
        }
        self.bump_revision().await;
    }

    async fn bump_revision(&self) {
        {
            let mut inner = self.inner.lock().await;
            inner.revision = inner.revision.saturating_add(1);
            let _ = self.revision_tx.send(inner.revision);
        }
        self.notify.notify_waiters();
    }
}

#[derive(Debug, Clone)]
struct SourceLoadResult {
    sessions: Vec<SessionSummary>,
    diagnostics: SourceDiagnostics,
}

impl CatalogSourcePlan {
    fn key(&self) -> CatalogSourceKey {
        match self {
            Self::Native => native_source_key(),
            Self::Import {
                source_id,
                working_directory,
                ..
            } => CatalogSourceKey {
                source_id: source_id.clone(),
                scope: CatalogSourceScope::WorkingDirectory(normalize_path(working_directory)),
            },
        }
    }

    fn metadata(&self) -> SourceMetadata {
        match self {
            Self::Native => native_metadata(),
            Self::Import { source_id, .. } => SourceMetadata {
                display_name: format!("Imported [{source_id}] sessions"),
            },
        }
    }
}

fn native_source_key() -> CatalogSourceKey {
    CatalogSourceKey {
        source_id: NATIVE_SOURCE_ID.to_owned(),
        scope: CatalogSourceScope::Global,
    }
}

fn native_metadata() -> SourceMetadata {
    SourceMetadata {
        display_name: NATIVE_DISPLAY_NAME.to_owned(),
    }
}

async fn source_plans(state: &ServerState, working_directory: &Path) -> Vec<CatalogSourcePlan> {
    let mut plans = vec![CatalogSourcePlan::Native];
    if !bcode_config::load_config().map_or(true, |config| config.session_import.enabled) {
        return plans;
    }
    let providers = state
        .plugins
        .registry()
        .service_registry()
        .providers_for(bcode_session_import::SESSION_IMPORT_INTERFACE_ID)
        .cloned()
        .unwrap_or_default();
    for plugin_id in providers {
        for source_id in import_source_ids(state, &plugin_id).await {
            plans.push(CatalogSourcePlan::Import {
                plugin_id: plugin_id.clone(),
                source_id,
                working_directory: working_directory.to_path_buf(),
            });
        }
    }
    plans
}

async fn load_source(
    state: &ServerState,
    plan: &CatalogSourcePlan,
) -> Result<SourceLoadResult, String> {
    match plan {
        CatalogSourcePlan::Native => load_native_source(state).await,
        CatalogSourcePlan::Import {
            plugin_id,
            source_id,
            working_directory,
        } => discover_import_source(state, plugin_id, source_id, working_directory).await,
    }
}

async fn load_native_source(state: &ServerState) -> Result<SourceLoadResult, String> {
    state
        .sessions
        .wait_catalog_loaded()
        .await
        .map_err(|error| error.to_string())?;
    let entries = state.sessions.all_session_catalog_entries().await;
    let diagnostics = native_source_diagnostics(&entries);
    Ok(SourceLoadResult {
        sessions: entries.into_iter().map(|entry| entry.summary).collect(),
        diagnostics,
    })
}

fn import_source_result(sessions: Vec<SessionSummary>) -> SourceLoadResult {
    SourceLoadResult {
        sessions,
        diagnostics: SourceDiagnostics::default(),
    }
}

fn native_source_diagnostics(_entries: &[SessionCatalogEntry]) -> SourceDiagnostics {
    SourceDiagnostics::default()
}

fn snapshot_locked(
    inner: &SessionCatalogInner,
    working_directory: &Path,
) -> SessionCatalogSnapshot {
    let native_sessions = inner
        .sources
        .get(&native_source_key())
        .map_or(&[][..], SourceCache::sessions);
    let native_imports = native_import_identities(native_sessions);
    let hide_imported = bcode_config::load_config()
        .map_or(true, |config| config.session_import.hide_already_imported);
    let mut sessions = Vec::new();
    let mut sources = Vec::new();

    for (key, source) in &inner.sources {
        if !source_relevant_to_working_directory(key, working_directory) {
            continue;
        }
        sources.push(source_status(key, source));
        match &key.scope {
            CatalogSourceScope::Global => sessions.extend(
                source
                    .sessions()
                    .iter()
                    .filter(|session| {
                        normalize_path(&session.working_directory) == working_directory
                    })
                    .cloned(),
            ),
            CatalogSourceScope::WorkingDirectory(_) => sessions.extend(
                source
                    .sessions()
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
            ),
        }
    }

    sort_sessions(&mut sessions);
    SessionCatalogSnapshot {
        status: aggregate_status(sources.iter().map(|source| &source.status)),
        sessions,
        sources,
        revision: inner.revision,
    }
}

impl SourceCache {
    fn status(&self) -> SessionCatalogStatus {
        match &self.state {
            SourceCacheState::Empty => SessionCatalogStatus::NotStarted,
            SourceCacheState::Loading => SessionCatalogStatus::Loading,
            SourceCacheState::Loaded { .. } => {
                let status = diagnostic_status(&self.metadata.display_name, self.diagnostics());
                status.unwrap_or(SessionCatalogStatus::Loaded)
            }
            SourceCacheState::Failed { message, .. } => {
                SessionCatalogStatus::Failed(message.clone())
            }
        }
    }

    fn sessions(&self) -> &[SessionSummary] {
        match &self.state {
            SourceCacheState::Loaded { sessions, .. }
            | SourceCacheState::Failed { sessions, .. } => sessions,
            SourceCacheState::Empty | SourceCacheState::Loading => &[],
        }
    }

    fn diagnostics(&self) -> &SourceDiagnostics {
        static EMPTY: SourceDiagnostics = SourceDiagnostics {};
        match &self.state {
            SourceCacheState::Loaded { diagnostics, .. }
            | SourceCacheState::Failed { diagnostics, .. } => diagnostics,
            SourceCacheState::Empty | SourceCacheState::Loading => &EMPTY,
        }
    }
}

const fn diagnostic_status(
    _display_name: &str,
    _diagnostics: &SourceDiagnostics,
) -> Option<SessionCatalogStatus> {
    None
}

fn source_relevant_to_working_directory(key: &CatalogSourceKey, working_directory: &Path) -> bool {
    match &key.scope {
        CatalogSourceScope::Global => true,
        CatalogSourceScope::WorkingDirectory(path) => path == working_directory,
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

fn source_status(key: &CatalogSourceKey, source: &SourceCache) -> SessionCatalogSourceStatus {
    SessionCatalogSourceStatus {
        source_id: key.source_id.clone(),
        display_name: source.metadata.display_name.clone(),
        status: source.status(),
        updated_at_ms: current_unix_millis(),
    }
}

fn aggregate_status<'a>(
    statuses: impl IntoIterator<Item = &'a SessionCatalogStatus>,
) -> SessionCatalogStatus {
    let mut has_loading = false;
    let mut failures = Vec::new();
    let mut saw_status = false;
    for status in statuses {
        saw_status = true;
        match status {
            SessionCatalogStatus::NotStarted | SessionCatalogStatus::Loading => has_loading = true,
            SessionCatalogStatus::Failed(message) | SessionCatalogStatus::Degraded(message) => {
                failures.push(message.clone());
            }
            SessionCatalogStatus::Loaded => {}
        }
    }
    if has_loading || !saw_status {
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
) -> Result<SourceLoadResult, String> {
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
    let sessions = response
        .sessions
        .into_iter()
        .filter(|summary| {
            summary.status == ImportableSessionStatus::Available && summary.source_id == source_id
        })
        .map(importable_to_summary)
        .collect();
    Ok(import_source_result(sessions))
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
        name: name.clone(),
        explicit_name: name,
        derived_title: None,
        title_source: SessionTitleSource::Imported,
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
        fork: None,
    }
}

fn upsert_session(sessions: &mut Vec<SessionSummary>, session: SessionSummary) -> bool {
    if let Some(existing) = sessions
        .iter_mut()
        .find(|existing| existing.id == session.id)
    {
        if *existing == session {
            false
        } else {
            *existing = session;
            true
        }
    } else {
        sessions.push(session);
        true
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

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}
