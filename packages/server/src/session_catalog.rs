//! Unified non-blocking session catalog snapshots.

use crate::ServerState;
use bcode_session::SessionCatalogEntry;
use bcode_session_import::ImportableSessionStatus;
use bcode_session_models::{SessionCatalogSourceStatus, SessionCatalogStatus};
use bcode_session_models::{SessionId, SessionImportSummary, SessionSummary, SessionTitleSource};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Notify, OnceCell, watch};

const NATIVE_SOURCE_ID: &str = "native";
const NATIVE_DISPLAY_NAME: &str = "Native Bcode sessions";

/// Server-owned session catalog cache.
#[derive(Debug)]
pub struct SessionCatalog {
    inner: Mutex<SessionCatalogInner>,
    revision_tx: watch::Sender<u64>,
    revision_rx: watch::Receiver<u64>,
    notify: Notify,
    import_sources: Mutex<BTreeMap<String, Arc<OnceCell<Vec<String>>>>>,
    #[cfg(test)]
    discovery_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
}

#[derive(Debug, Default)]
struct SessionCatalogInner {
    revision: u64,
    sources: BTreeMap<CatalogSourceKey, SourceCache>,
    import_plans: BTreeMap<PathBuf, Option<Vec<CatalogSourcePlan>>>,
    planning_generation: u64,
    retry_import_plans: BTreeMap<PathBuf, std::time::Instant>,
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
    updated_at_ms: u64,
    generation: u64,
}

#[derive(Debug, Clone)]
enum SourceCacheState {
    Empty,
    Loading {
        sessions: Vec<SessionSummary>,
    },
    Loaded {
        sessions: Vec<SessionSummary>,
        diagnostics: SourceDiagnostics,
    },
    Failed {
        retry_at: std::time::Instant,
        message: String,
        sessions: Vec<SessionSummary>,
        diagnostics: SourceDiagnostics,
    },
}

#[derive(Debug, Clone, Default)]
struct SourceDiagnostics {}

#[derive(Debug, Clone)]
enum CatalogSourcePlan {
    /// Sessions owned by an in-memory manager, with no durable location claim.
    InMemory,
    /// Canonical sessions owned by one resolved state location.
    ///
    /// The primary location is loaded through the owning `SessionManager`. Additional
    /// readable locations are discovered read-only: no canonical database is opened, no
    /// lease or lock is taken, and nothing is migrated or repaired. Aggregated discovery
    /// confers no authority.
    Native { location: NativeLocation },
    Import {
        plugin_id: String,
        source_id: String,
        working_directory: PathBuf,
    },
}

/// One state location participating in aggregated native discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NativeLocation {
    location_id: String,
    profile: Option<String>,
    sessions_root: PathBuf,
    primary: bool,
}

impl NativeLocation {
    fn source_id(&self) -> String {
        if self.primary {
            NATIVE_SOURCE_ID.to_owned()
        } else {
            format!("{NATIVE_SOURCE_ID}:{}", self.location_id)
        }
    }

    fn display_name(&self) -> String {
        match (&self.profile, self.primary) {
            (Some(profile), true) => format!("{NATIVE_DISPLAY_NAME} [{profile}]"),
            (Some(profile), false) => format!("Bcode sessions [{profile}]"),
            (None, true) => NATIVE_DISPLAY_NAME.to_owned(),
            (None, false) => format!("Bcode sessions [{}]", self.location_id),
        }
    }

    fn summary(&self) -> bcode_session_models::SessionLocationSummary {
        bcode_session_models::SessionLocationSummary {
            location_id: self.location_id.clone(),
            profile: self.profile.clone(),
            primary: self.primary,
            ambiguous: false,
        }
    }
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
            import_sources: Mutex::default(),
            #[cfg(test)]
            discovery_gate: Mutex::default(),
        }
    }
}

impl SessionCatalog {
    /// Subscribe to catalog revision changes.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision_rx.clone()
    }

    /// Return the current catalog revision without starting discovery or loading storage.
    #[must_use]
    pub fn revision(&self) -> u64 {
        *self.revision_rx.borrow()
    }

    /// Return the current coherent catalog snapshot for a working directory.
    pub async fn snapshot(
        &self,
        state: &Arc<ServerState>,
        working_directory: &Path,
    ) -> SessionCatalogSnapshot {
        let working_directory = normalize_path(working_directory);
        self.ensure_sources(state, &working_directory).await;
        let hide_imported = if state.locations.is_some() {
            state.startup_config.session_import.hide_already_imported
        } else {
            bcode_config::load_config()
                .map_or(true, |config| config.session_import.hide_already_imported)
        };
        let inner = self.inner.lock().await;
        snapshot_locked(&inner, &working_directory, hide_imported)
    }

    /// Return whether more than one readable state location claims this session ID.
    ///
    /// Ambiguity is reported from already-loaded catalog state only; this never triggers
    /// discovery, opens canonical storage, or takes a lease. Callers must refuse to open an
    /// ambiguous session as authoritative until explicit maintenance resolves the conflict.
    pub async fn ambiguous_location_ids(&self, session_id: SessionId) -> Vec<String> {
        let mut locations = BTreeSet::new();
        {
            let inner = self.inner.lock().await;
            for source in inner.sources.values() {
                for session in source.sessions() {
                    if session.id == session_id
                        && let Some(location) = &session.location
                    {
                        locations.insert(location.location_id.clone());
                    }
                }
            }
        }
        if locations.len() > 1 {
            locations.into_iter().collect()
        } else {
            Vec::new()
        }
    }

    /// Replace the primary native source with a fresh view from the session manager.
    pub async fn refresh_native_now(&self, state: &ServerState) {
        let generation = {
            let mut inner = self.inner.lock().await;
            let source = inner
                .sources
                .entry(native_source_key())
                .or_insert_with(|| SourceCache {
                    metadata: native_metadata(),
                    state: SourceCacheState::Empty,
                    updated_at_ms: 0,
                    generation: 0,
                });
            source.generation += 1;
            let generation = source.generation;
            drop(inner);
            generation
        };
        if state.sessions.session_store_root().is_none() {
            self.publish_source_result(
                native_source_key(),
                native_metadata(),
                load_in_memory_source(state).await,
                Some(generation),
            )
            .await;
            return;
        }
        let Some(location) = native_locations(state)
            .into_iter()
            .find(|location| location.primary)
        else {
            return;
        };
        let result = load_native_source(state, &location).await;
        self.publish_source_result(
            native_source_key(),
            native_metadata(),
            result,
            Some(generation),
        )
        .await;
    }

    /// Mark native sessions dirty so the next snapshot reloads them.
    pub async fn invalidate_native(&self) {
        self.invalidate_source_id(NATIVE_SOURCE_ID).await;
    }

    #[allow(clippy::significant_drop_tightening)]
    /// Update a materialized native session cache after a native session mutation.
    pub async fn upsert_native_session(&self, session: SessionSummary) {
        let mut inner = self.inner.lock().await;
        let Some(source) = inner.sources.get_mut(&native_source_key()) else {
            return;
        };
        let sessions = match &mut source.state {
            SourceCacheState::Loaded { sessions, .. }
            | SourceCacheState::Failed { sessions, .. } => sessions,
            SourceCacheState::Empty | SourceCacheState::Loading { .. } => {
                source.generation += 1;
                source.state = SourceCacheState::Empty;
                source.updated_at_ms = current_unix_millis();
                self.bump_revision(&mut inner);
                return;
            }
        };
        if upsert_session(sessions, session) {
            source.generation += 1;
            source.updated_at_ms = current_unix_millis();
            self.bump_revision(&mut inner);
        }
    }

    #[allow(clippy::significant_drop_tightening)]
    /// Remove a native session from a materialized native session cache.
    pub async fn remove_native_session(&self, session_id: SessionId) {
        let mut inner = self.inner.lock().await;
        let Some(source) = inner.sources.get_mut(&native_source_key()) else {
            return;
        };
        let sessions = match &mut source.state {
            SourceCacheState::Loaded { sessions, .. }
            | SourceCacheState::Failed { sessions, .. } => sessions,
            SourceCacheState::Empty | SourceCacheState::Loading { .. } => {
                source.generation += 1;
                source.state = SourceCacheState::Empty;
                source.updated_at_ms = current_unix_millis();
                self.bump_revision(&mut inner);
                return;
            }
        };
        let original_len = sessions.len();
        sessions.retain(|session| session.id != session_id);
        if sessions.len() != original_len {
            source.generation += 1;
            source.updated_at_ms = current_unix_millis();
            self.bump_revision(&mut inner);
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
        // Source enumeration is cached independently of per-directory session discovery.
        // A refresh must discover newly available sources too. In-flight readers retain
        // their old cell, but cannot repopulate the invalidated cache.
        {
            let mut inner = self.inner.lock().await;
            inner.planning_generation += 1;
            inner.import_plans.clear();
            inner.retry_import_plans.clear();
            self.bump_revision(&mut inner);
            drop(inner);
        }
        self.import_sources.lock().await.clear();
        self.invalidate_sources(&working_directory, sources).await;
        self.snapshot(state, &working_directory).await
    }

    async fn import_source_ids(
        &self,
        plugin_id: &str,
        load: impl std::future::Future<Output = Result<Vec<String>, String>>,
    ) -> Vec<String> {
        let cell = {
            let mut sources = self.import_sources.lock().await;
            Arc::clone(sources.entry(plugin_id.to_owned()).or_default())
        };
        // OnceCell coalesces concurrent enumeration, releases initialization on
        // cancellation, and retains only successes so transient failures can retry.
        cell.get_or_try_init(|| load)
            .await
            .cloned()
            .unwrap_or_default()
    }

    async fn ensure_sources(&self, state: &Arc<ServerState>, working_directory: &Path) {
        let directory = working_directory.to_path_buf();
        let load_state = Arc::clone(state);
        let load_directory = directory.clone();
        self.ensure_sources_with_imports(state, directory, async move {
            source_plans(&load_state, &load_directory).await
        })
        .await;
    }

    async fn ensure_sources_with_imports(
        &self,
        state: &Arc<ServerState>,
        working_directory: PathBuf,
        imports: impl std::future::Future<Output = Vec<CatalogSourcePlan>> + Send + 'static,
    ) {
        for plan in native_source_plans(state) {
            self.ensure_source(state, plan).await;
        }
        let mut inner = self.inner.lock().await;
        let retry = inner
            .retry_import_plans
            .get(&working_directory)
            .is_some_and(|deadline| std::time::Instant::now() >= *deadline);
        if retry {
            inner.retry_import_plans.remove(&working_directory);
        }
        if let Some(plans) = inner.import_plans.get(&working_directory) {
            let plans = plans.clone().unwrap_or_default();
            let generation = inner.planning_generation;
            drop(inner);
            for plan in plans {
                self.ensure_source_for_generation(state, plan, Some(generation))
                    .await;
            }
            if !retry {
                return;
            }
            inner = self.inner.lock().await;
            // Another observer may already have started the retry.
            if inner
                .import_plans
                .get(&working_directory)
                .is_some_and(Option::is_none)
            {
                return;
            }
        }
        let generation = inner.planning_generation;
        inner.import_plans.insert(working_directory.clone(), None);
        self.bump_revision(&mut inner);
        drop(inner);
        let state = Arc::clone(state);
        tokio::spawn(async move {
            let plans = imports.await;
            let catalog = &state.session_catalog;
            let enumeration_complete = catalog
                .import_sources
                .lock()
                .await
                .values()
                .all(|cell| cell.initialized());
            let mut inner = catalog.inner.lock().await;
            let mut retry_deadline = None;
            if inner.planning_generation == generation {
                if !enumeration_complete {
                    // Install successful providers' plans before retrying failed enumeration.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    inner
                        .retry_import_plans
                        .insert(working_directory.clone(), deadline);
                    retry_deadline = Some(deadline);
                }
                inner
                    .import_plans
                    .insert(working_directory.clone(), Some(plans));
                catalog.bump_revision(&mut inner);
            }
            drop(inner);
            let weak_state = Arc::downgrade(&state);
            drop(state);
            if let Some(deadline) = retry_deadline {
                notify_planning_retry(&weak_state, working_directory, generation, deadline).await;
            }
            drop(weak_state);
        });
    }

    async fn ensure_source(&self, state: &Arc<ServerState>, plan: CatalogSourcePlan) {
        self.ensure_source_for_generation(state, plan, None).await;
    }

    #[allow(clippy::significant_drop_tightening)]
    async fn ensure_source_for_generation(
        &self,
        state: &Arc<ServerState>,
        plan: CatalogSourcePlan,
        planning_generation: Option<u64>,
    ) {
        let key = plan.key();
        let metadata = plan.metadata();
        let generation = {
            let mut inner = self.inner.lock().await;
            if planning_generation.is_some_and(|generation| generation != inner.planning_generation)
            {
                return;
            }
            let source = inner
                .sources
                .entry(key.clone())
                .or_insert_with(|| SourceCache {
                    metadata: metadata.clone(),
                    state: SourceCacheState::Empty,
                    updated_at_ms: 0,
                    generation: 0,
                });
            source.metadata = metadata.clone();
            match source.state {
                SourceCacheState::Failed { retry_at, .. }
                    if std::time::Instant::now() < retry_at =>
                {
                    None
                }
                SourceCacheState::Empty | SourceCacheState::Failed { .. } => {
                    source.state = SourceCacheState::Loading {
                        sessions: source.sessions().to_vec(),
                    };
                    source.updated_at_ms = current_unix_millis();
                    source.generation += 1;
                    let generation = source.generation;
                    self.bump_revision(&mut inner);
                    Some(generation)
                }
                SourceCacheState::Loading { .. } | SourceCacheState::Loaded { .. } => None,
            }
        };
        let Some(generation) = generation else {
            return;
        };
        let state = Arc::clone(state);
        let catalog = Arc::clone(&state.session_catalog);
        tokio::spawn(async move {
            let key = plan.key();
            let metadata = plan.metadata();
            let labels = catalog_source_metric_labels(&key);
            if let CatalogSourcePlan::Native { location } = &plan
                && location.primary
                && !state.sessions.catalog_loaded()
            {
                let store = bcode_session::SessionStore::new(&location.sessions_root);
                let (mut pages, completion) = store.stream_catalog_pages();
                while let Some(mut page) = pages.recv().await {
                    for summary in &mut page {
                        summary.location = Some(location.summary());
                    }
                    let mut inner = catalog.inner.lock().await;
                    let Some(source) = inner.sources.get_mut(&key) else {
                        break;
                    };
                    if source.generation != generation {
                        break;
                    }
                    let SourceCacheState::Loading { sessions } = &mut source.state else {
                        break;
                    };
                    if !page.is_empty() {
                        merge_catalog_page(sessions, page);
                        source.updated_at_ms = current_unix_millis();
                        catalog.bump_revision(&mut inner);
                    }
                }
                drop(pages);
                // Missing or damaged disposable catalogs fall back to normal discovery.
                if let Err(error) = completion.await {
                    tracing::warn!(%error, "catalog page reader task failed");
                }
            }
            let result = state
                .metrics
                .time_result_async(
                    "session.catalog.source_load",
                    labels.clone(),
                    load_source(&state, &plan),
                )
                .await;
            if let Ok(source) = &result {
                state.metrics.record_histogram_with_labels(
                    "session.catalog.source_load.sessions",
                    source.sessions.len() as u64,
                    labels,
                );
            }
            catalog
                .publish_source_result(key, metadata, result, Some(generation))
                .await;
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
                    source.generation += 1;
                    source.state = SourceCacheState::Empty;
                    source.updated_at_ms = current_unix_millis();
                    changed = true;
                }
            }
            if changed {
                self.bump_revision(&mut inner);
            }
            drop(inner);
        }
    }

    async fn invalidate_source_id(&self, source_id: &str) {
        let mut changed = false;
        {
            let mut inner = self.inner.lock().await;
            for (key, source) in &mut inner.sources {
                if key.source_id == source_id {
                    source.generation += 1;
                    source.state = SourceCacheState::Empty;
                    source.updated_at_ms = current_unix_millis();
                    changed = true;
                }
            }
            if changed {
                self.bump_revision(&mut inner);
            }
            drop(inner);
        }
    }

    #[cfg(test)]
    async fn apply_source_result(
        &self,
        key: CatalogSourceKey,
        metadata: SourceMetadata,
        result: Result<SourceLoadResult, String>,
    ) {
        self.publish_source_result(key, metadata, result, None)
            .await;
    }

    async fn publish_source_result(
        &self,
        key: CatalogSourceKey,
        metadata: SourceMetadata,
        result: Result<SourceLoadResult, String>,
        expected_generation: Option<u64>,
    ) {
        {
            let mut inner = self.inner.lock().await;
            if let Some(expected) = expected_generation
                && inner
                    .sources
                    .get(&key)
                    .is_none_or(|source| source.generation != expected)
            {
                return;
            }
            let source = inner.sources.entry(key).or_insert_with(|| SourceCache {
                metadata: metadata.clone(),
                state: SourceCacheState::Empty,
                updated_at_ms: 0,
                generation: 0,
            });
            source.generation += 1;
            source.metadata = metadata;
            source.updated_at_ms = current_unix_millis();
            source.state = match result {
                Ok(result) => SourceCacheState::Loaded {
                    sessions: result.sessions,
                    diagnostics: result.diagnostics,
                },
                Err(message) => SourceCacheState::Failed {
                    retry_at: std::time::Instant::now() + std::time::Duration::from_secs(5),
                    message,
                    // A late discovery error does not invalidate already published
                    // display metadata. Mutations fence and clear it separately.
                    sessions: source.sessions().to_vec(),
                    diagnostics: SourceDiagnostics::default(),
                },
            };
            self.bump_revision(&mut inner);
            drop(inner);
        }
    }

    fn bump_revision(&self, inner: &mut SessionCatalogInner) {
        inner.revision = inner.revision.saturating_add(1);
        self.revision_tx.send_replace(inner.revision);
        self.notify.notify_waiters();
    }
}

// Retry pages replace matching retained rows; successful completion replaces the
// entire source, so rows absent from the final discovery do not survive forever.
fn merge_catalog_page(sessions: &mut Vec<SessionSummary>, page: Vec<SessionSummary>) {
    let incoming_ids = page
        .iter()
        .map(|summary| summary.id)
        .collect::<BTreeSet<_>>();
    sessions.retain(|summary| !incoming_ids.contains(&summary.id));
    sessions.extend(page);
}

#[derive(Debug, Clone)]
struct SourceLoadResult {
    sessions: Vec<SessionSummary>,
    diagnostics: SourceDiagnostics,
}

impl CatalogSourcePlan {
    fn key(&self) -> CatalogSourceKey {
        match self {
            Self::InMemory => native_source_key(),
            Self::Native { location } => CatalogSourceKey {
                source_id: location.source_id(),
                scope: CatalogSourceScope::Global,
            },
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
            Self::InMemory => native_metadata(),
            Self::Native { location } => SourceMetadata {
                display_name: location.display_name(),
            },
            Self::Import { source_id, .. } => SourceMetadata {
                display_name: format!("Imported [{source_id}] sessions"),
            },
        }
    }
}

fn catalog_source_metric_labels(key: &CatalogSourceKey) -> bcode_metrics::MetricLabels {
    let mut labels = bcode_metrics::MetricLabels::new();
    labels.insert("source_id".to_owned(), key.source_id.clone());
    labels.insert(
        "scope".to_owned(),
        match &key.scope {
            CatalogSourceScope::Global => "global".to_owned(),
            CatalogSourceScope::WorkingDirectory(_) => "working_directory".to_owned(),
        },
    );
    labels
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

/// Resolve the state locations that participate in aggregated native discovery.
///
/// The primary location always participates. Additional readable locations come from
/// `[state] readable_profiles`. Resolution failures are skipped rather than propagated: a
/// location that cannot be resolved must not prevent the rest of the catalog from loading
/// (`Domain-local durable failures remain isolated`).
fn native_locations(state: &ServerState) -> Vec<NativeLocation> {
    let Some(primary_sessions_root) = state.sessions.session_store_root() else {
        return Vec::new();
    };
    let Some(primary_id) = state.daemon_status.state_location_id.clone() else {
        return Vec::new();
    };
    let mut locations = vec![NativeLocation {
        location_id: primary_id.clone(),
        profile: None,
        sessions_root: primary_sessions_root,
        primary: true,
    }];

    let resolved = if let Some(locations) = &state.locations {
        locations.clone()
    } else {
        let Ok(config) = bcode_config::load_config() else {
            return locations;
        };
        if config.state.readable_profiles.is_empty() {
            return locations;
        }
        let Ok(resolved) = bcode_config::resolve_state_location_set(
            &config.state,
            &bcode_config::StateLocationSelection::default(),
        ) else {
            return locations;
        };
        resolved
    };
    for location in resolved.readable() {
        let location_id = location.id().as_str().to_owned();
        if location_id == primary_id
            || locations
                .iter()
                .any(|existing| existing.location_id == location_id)
        {
            continue;
        }
        locations.push(NativeLocation {
            location_id,
            profile: location.profile().map(str::to_owned),
            sessions_root: location.sessions_root().to_path_buf(),
            primary: false,
        });
    }
    locations
}

async fn load_in_memory_source(state: &ServerState) -> Result<SourceLoadResult, String> {
    let entries = state.sessions.all_session_catalog_entries().await;
    Ok(SourceLoadResult {
        diagnostics: native_source_diagnostics(&entries),
        sessions: entries
            .into_iter()
            .map(|entry| {
                let mut summary = entry.summary;
                summary.location = None;
                summary
            })
            .collect(),
    })
}

fn native_source_plans(state: &ServerState) -> Vec<CatalogSourcePlan> {
    let mut plans = native_locations(state)
        .into_iter()
        .map(|location| CatalogSourcePlan::Native { location })
        .collect::<Vec<_>>();
    if state.sessions.session_store_root().is_none() {
        plans.push(CatalogSourcePlan::InMemory);
    }
    plans
}

async fn source_plans(state: &ServerState, working_directory: &Path) -> Vec<CatalogSourcePlan> {
    let mut plans = Vec::new();
    let imports_enabled = if state.locations.is_some() {
        state.startup_config.session_import.enabled
    } else {
        bcode_config::load_config().map_or(true, |config| config.session_import.enabled)
    };
    if !imports_enabled {
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
        for source_id in state
            .session_catalog
            .import_source_ids(&plugin_id, Box::pin(import_source_ids(state, &plugin_id)))
            .await
        {
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
    #[cfg(test)]
    {
        let gate = state.session_catalog.discovery_gate.lock().await.clone();
        if let Some(gate) = gate {
            gate.acquire().await.expect("discovery gate open").forget();
        }
    }
    match plan {
        CatalogSourcePlan::InMemory => load_in_memory_source(state).await,
        CatalogSourcePlan::Native { location } => {
            if location.primary {
                load_native_source(state, location).await
            } else {
                load_foreign_native_source(location).await
            }
        }
        CatalogSourcePlan::Import {
            plugin_id,
            source_id,
            working_directory,
        } => discover_import_source(state, plugin_id, source_id, working_directory).await,
    }
}

/// Discover sessions in a state location this daemon does not own.
///
/// Bounded and non-mutating by construction: it reads derived manifests plus canonical
/// directory presence through `SessionStore::discover_readable_session_summaries`, so it
/// opens no canonical database and takes no lease. An unavailable root (for example an
/// unmounted volume) yields a degraded source rather than failing the whole catalog.
async fn load_foreign_native_source(location: &NativeLocation) -> Result<SourceLoadResult, String> {
    let sessions_root = location.sessions_root.clone();
    let location_summary = location.summary();
    tokio::task::spawn_blocking(move || {
        let store = bcode_session::SessionStore::new(&sessions_root);
        let sessions = store
            .discover_readable_session_summaries()
            .map_err(|error| error.to_string())?;
        Ok(SourceLoadResult {
            sessions: sessions
                .into_iter()
                .map(|mut summary| {
                    summary.location = Some(location_summary.clone());
                    summary
                })
                .collect(),
            diagnostics: SourceDiagnostics::default(),
        })
    })
    .await
    .map_err(|error| format!("foreign session discovery task failed: {error}"))?
}

async fn load_native_source(
    state: &ServerState,
    location: &NativeLocation,
) -> Result<SourceLoadResult, String> {
    state
        .sessions
        .wait_catalog_loaded()
        .await
        .map_err(|error| error.to_string())?;
    let entries = state.sessions.all_session_catalog_entries().await;
    let diagnostics = native_source_diagnostics(&entries);
    let location_summary = location.summary();
    Ok(SourceLoadResult {
        sessions: entries
            .into_iter()
            .map(|entry| {
                let mut summary = entry.summary;
                summary.location = Some(location_summary.clone());
                summary
            })
            .collect(),
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
    hide_imported: bool,
) -> SessionCatalogSnapshot {
    let native_sessions = inner
        .sources
        .get(&native_source_key())
        .map_or(&[][..], SourceCache::sessions);
    let native_imports = native_import_identities(native_sessions);
    let mut sessions = Vec::new();
    let mut sources = Vec::new();
    // Resolve each distinct directory once per snapshot, not once per session.
    // Keep this cache local so symlink changes are observed on subsequent reads.
    let mut normalized_directories = BTreeMap::new();

    for (key, source) in &inner.sources {
        if !source_relevant_to_working_directory(key, working_directory) {
            continue;
        }
        sources.push(source_status(key, source));
        match &key.scope {
            // Native/global sources are filtered to the caller's working directory. Foreign
            // readable locations are discovered from derived manifests, where a session
            // without manifest metadata falls back to the store root as its working
            // directory, so those entries cannot be working-directory filtered without
            // hiding them entirely. Retain foreign sessions that carry no usable working
            // directory, and filter the ones that do.
            CatalogSourceScope::Global => sessions.extend(
                source
                    .sessions()
                    .iter()
                    .filter(|session| {
                        let is_foreign = session
                            .location
                            .as_ref()
                            .is_some_and(|location| !location.primary);
                        if is_foreign && session.updated_at_ms == 0 {
                            return true;
                        }
                        normalized_directories
                            .entry(session.working_directory.clone())
                            .or_insert_with(|| normalize_path(&session.working_directory))
                            .as_path()
                            == working_directory
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
    mark_ambiguous_locations(&mut sessions);
    SessionCatalogSnapshot {
        status: if inner
            .import_plans
            .get(working_directory)
            .is_some_and(|plans| {
                plans.as_ref().is_none_or(|plans| {
                    plans
                        .iter()
                        .any(|plan| !inner.sources.contains_key(&plan.key()))
                })
            }) {
            SessionCatalogStatus::Loading
        } else if inner.retry_import_plans.contains_key(working_directory) {
            SessionCatalogStatus::Degraded(
                "Some import sources could not be enumerated; discovery will retry.".to_owned(),
            )
        } else {
            aggregate_status(sources.iter().map(|source| &source.status))
        },
        sessions,
        sources,
        revision: inner.revision,
    }
}

/// Flag sessions whose ID is claimed by more than one readable state location.
///
/// Duplicate canonical roots are never merged automatically. Every claim is retained and
/// marked ambiguous so the conflict is visible; callers must refuse to open an ambiguous
/// session as authoritative until an explicit maintenance operation resolves it
/// (`Canonical history is never silently merged`, `Sensitive ambiguity fails closed`).
fn mark_ambiguous_locations(sessions: &mut [SessionSummary]) {
    let mut claims: BTreeMap<SessionId, BTreeSet<String>> = BTreeMap::new();
    for session in sessions.iter() {
        if let Some(location) = &session.location {
            claims
                .entry(session.id)
                .or_default()
                .insert(location.location_id.clone());
        }
    }
    for session in sessions.iter_mut() {
        let ambiguous = claims
            .get(&session.id)
            .is_some_and(|locations| locations.len() > 1);
        if ambiguous && let Some(location) = session.location.as_mut() {
            location.ambiguous = true;
        }
    }
}

impl SourceCache {
    fn status(&self) -> SessionCatalogStatus {
        match &self.state {
            SourceCacheState::Empty => SessionCatalogStatus::NotStarted,
            SourceCacheState::Loading { .. } => SessionCatalogStatus::Loading,
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
            | SourceCacheState::Loading { sessions }
            | SourceCacheState::Failed { sessions, .. } => sessions,
            SourceCacheState::Empty => &[],
        }
    }

    fn diagnostics(&self) -> &SourceDiagnostics {
        static EMPTY: SourceDiagnostics = SourceDiagnostics {};
        match &self.state {
            SourceCacheState::Loaded { diagnostics, .. }
            | SourceCacheState::Failed { diagnostics, .. } => diagnostics,
            SourceCacheState::Empty | SourceCacheState::Loading { .. } => &EMPTY,
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
        updated_at_ms: source.updated_at_ms,
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

async fn import_source_ids(state: &ServerState, plugin_id: &str) -> Result<Vec<String>, String> {
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
        .map_err(|error| error.to_string())
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
        execution: None,
        // Importable external sessions are not owned by a Bcode state location until they
        // are imported, so they carry no owning-location label.
        location: None,
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

async fn notify_planning_retry(
    state: &std::sync::Weak<ServerState>,
    working_directory: PathBuf,
    generation: u64,
    deadline: std::time::Instant,
) {
    tokio::time::sleep_until(deadline.into()).await;
    if let Some(state) = state.upgrade() {
        let catalog = &state.session_catalog;
        let mut inner = catalog.inner.lock().await;
        if inner.planning_generation == generation
            && inner.retry_import_plans.get(&working_directory) == Some(&deadline)
        {
            // Observers perform the retry through normal source planning. No provider
            // work is started here when the catalog has no active observers.
            catalog.bump_revision(&mut inner);
        }
    }
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

#[cfg(test)]
mod tests {
    #[test]
    fn retry_pages_replace_matching_rows_without_losing_other_results() {
        let first = summary(SessionId::new(), None);
        let second = summary(SessionId::new(), None);
        let mut updated = first.clone();
        updated.name = Some("updated".to_owned());
        let mut sessions = vec![first, second.clone()];
        super::merge_catalog_page(&mut sessions, vec![updated.clone()]);
        assert_eq!(sessions, vec![second.clone(), updated.clone()]);
        super::merge_catalog_page(&mut sessions, vec![updated.clone()]);
        assert_eq!(sessions, vec![second, updated]);
    }

    #[tokio::test]
    async fn failed_source_is_observable_and_explicit_refresh_bypasses_backoff() {
        let root = tempfile::tempdir().unwrap();
        let state = std::sync::Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::persistent_lazy(root.path()),
        ));
        let catalog = &state.session_catalog;
        let session = summary(SessionId::new(), None);
        catalog
            .apply_source_result(
                super::native_source_key(),
                super::native_metadata(),
                Ok(SourceLoadResult {
                    sessions: vec![session.clone()],
                    diagnostics: SourceDiagnostics::default(),
                }),
            )
            .await;
        catalog
            .apply_source_result(
                super::native_source_key(),
                super::native_metadata(),
                Err("unavailable".to_owned()),
            )
            .await;
        let revision = catalog.revision();
        catalog
            .ensure_source(&state, super::CatalogSourcePlan::InMemory)
            .await;
        assert_eq!(catalog.revision(), revision);
        let snapshot = {
            let inner = catalog.inner.lock().await;
            super::snapshot_locked(&inner, root.path(), true)
        };
        assert!(matches!(
            snapshot.status,
            bcode_session_models::SessionCatalogStatus::Failed(_)
        ));
        // Display timestamps cannot delay a retry after its monotonic deadline.
        {
            let mut inner = catalog.inner.lock().await;
            let source = inner.sources.get_mut(&super::native_source_key()).unwrap();
            source.updated_at_ms = u64::MAX;
            let super::SourceCacheState::Failed { retry_at, .. } = &mut source.state else {
                panic!("failure must remain visible during backoff");
            };
            *retry_at = std::time::Instant::now();
            drop(inner);
        }
        catalog
            .ensure_source(&state, super::CatalogSourcePlan::InMemory)
            .await;
        let inner = catalog.inner.lock().await;
        assert_eq!(
            inner.sources[&super::native_source_key()].sessions(),
            &[session]
        );
        assert!(matches!(
            inner.sources[&super::native_source_key()].state,
            super::SourceCacheState::Loading { .. }
        ));
        drop(inner);
        catalog
            .apply_source_result(
                super::native_source_key(),
                super::native_metadata(),
                Err("still unavailable".to_owned()),
            )
            .await;
        catalog.invalidate_native().await;
        catalog
            .ensure_source(&state, super::CatalogSourcePlan::InMemory)
            .await;
        let inner = catalog.inner.lock().await;
        let source = inner.sources[&super::native_source_key()].clone();
        drop(inner);
        assert!(matches!(
            source.state,
            super::SourceCacheState::Loading { .. }
        ));
        drop(state);
    }

    #[tokio::test]
    async fn loading_snapshot_retains_partial_results_and_mutations_invalidate_them() {
        let catalog = SessionCatalog::default();
        let session = summary(SessionId::new(), None);
        let directory = super::normalize_path(&session.working_directory);
        {
            let mut inner = catalog.inner.lock().await;
            inner.sources.insert(
                super::native_source_key(),
                super::SourceCache {
                    metadata: super::native_metadata(),
                    state: super::SourceCacheState::Loading {
                        sessions: vec![session.clone()],
                    },
                    generation: 1,
                    updated_at_ms: 1,
                },
            );
            let snapshot = super::snapshot_locked(&inner, &directory, true);
            drop(inner);
            assert_eq!(snapshot.sessions, vec![session.clone()]);
            assert!(matches!(
                snapshot.status,
                bcode_session_models::SessionCatalogStatus::Loading
            ));
        }
        catalog
            .publish_source_result(
                super::native_source_key(),
                super::native_metadata(),
                Err("discovery unavailable".to_owned()),
                Some(1),
            )
            .await;
        {
            let inner = catalog.inner.lock().await;
            let snapshot = super::snapshot_locked(&inner, &directory, true);
            drop(inner);
            assert_eq!(snapshot.sessions, vec![session.clone()]);
            assert!(!matches!(
                snapshot.status,
                bcode_session_models::SessionCatalogStatus::Loaded
                    | bcode_session_models::SessionCatalogStatus::Loading
            ));
        }
        catalog.remove_native_session(session.id).await;
        let inner = catalog.inner.lock().await;
        let source = inner.sources[&super::native_source_key()].clone();
        drop(inner);
        assert_eq!(source.generation, 3);
        assert!(source.sessions().is_empty());
        assert!(matches!(
            source.state,
            super::SourceCacheState::Failed { .. }
        ));
        catalog
            .publish_source_result(
                super::native_source_key(),
                super::native_metadata(),
                Ok(SourceLoadResult {
                    sessions: vec![session],
                    diagnostics: SourceDiagnostics::default(),
                }),
                Some(1),
            )
            .await;
        let inner = catalog.inner.lock().await;
        assert!(
            inner.sources[&super::native_source_key()]
                .sessions()
                .is_empty()
        );
        drop(inner);
    }

    #[tokio::test]
    async fn failed_planning_is_degraded_without_hiding_native_results() {
        let catalog = SessionCatalog::default();
        let session = summary(SessionId::new(), None);
        let directory = super::normalize_path(&session.working_directory);
        catalog
            .apply_source_result(
                super::native_source_key(),
                super::native_metadata(),
                Ok(SourceLoadResult {
                    sessions: vec![session.clone()],
                    diagnostics: SourceDiagnostics::default(),
                }),
            )
            .await;
        let mut inner = catalog.inner.lock().await;
        inner
            .import_plans
            .insert(directory.clone(), Some(Vec::new()));
        inner
            .retry_import_plans
            .insert(directory.clone(), std::time::Instant::now());
        let snapshot = super::snapshot_locked(&inner, &directory, true);
        assert!(matches!(
            snapshot.status,
            bcode_session_models::SessionCatalogStatus::Degraded(_)
        ));
        assert_eq!(snapshot.sessions, vec![session.clone()]);
        inner.import_plans.insert(directory.clone(), None);
        assert!(matches!(
            super::snapshot_locked(&inner, &directory, true).status,
            bcode_session_models::SessionCatalogStatus::Loading
        ));
        inner
            .import_plans
            .insert(directory.clone(), Some(Vec::new()));
        inner.retry_import_plans.remove(&directory);
        let snapshot = super::snapshot_locked(&inner, &directory, true);
        drop(inner);
        assert!(matches!(
            snapshot.status,
            bcode_session_models::SessionCatalogStatus::Loaded
        ));
        assert_eq!(snapshot.sessions, vec![session]);
    }

    #[tokio::test]
    async fn retry_deadline_notifies_observers_but_rejects_obsolete_generation() {
        let root = tempfile::tempdir().unwrap();
        let state = std::sync::Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::persistent_lazy(root.path()),
        ));
        let catalog = &state.session_catalog;
        let deadline = std::time::Instant::now();
        catalog
            .inner
            .lock()
            .await
            .retry_import_plans
            .insert(root.path().to_path_buf(), deadline);
        let mut revisions = catalog.subscribe();
        revisions.borrow_and_update();
        super::notify_planning_retry(
            &std::sync::Arc::downgrade(&state),
            root.path().to_path_buf(),
            0,
            deadline,
        )
        .await;
        assert!(revisions.has_changed().unwrap());
        revisions.borrow_and_update();
        catalog.inner.lock().await.planning_generation = 1;
        super::notify_planning_retry(
            &std::sync::Arc::downgrade(&state),
            root.path().to_path_buf(),
            0,
            deadline,
        )
        .await;
        assert!(!revisions.has_changed().unwrap());
        super::notify_planning_retry(
            &std::sync::Arc::downgrade(&state),
            root.path().to_path_buf(),
            1,
            deadline + std::time::Duration::from_millis(1),
        )
        .await;
        assert!(!revisions.has_changed().unwrap());
        let weak = std::sync::Arc::downgrade(&state);
        drop(state);
        assert!(weak.upgrade().is_none());
        super::notify_planning_retry(&weak, root.path().to_path_buf(), 1, deadline).await;
        drop(weak);
    }

    #[tokio::test]
    async fn stale_planning_generation_cannot_install_source() {
        let root = tempfile::tempdir().unwrap();
        let state = std::sync::Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::persistent_lazy(root.path()),
        ));
        let catalog = &state.session_catalog;
        catalog.inner.lock().await.planning_generation = 1;
        let revision = catalog.revision();
        catalog
            .ensure_source_for_generation(&state, super::CatalogSourcePlan::InMemory, Some(0))
            .await;
        assert_eq!(catalog.revision(), revision);
        assert!(catalog.inner.lock().await.sources.is_empty());
        catalog
            .ensure_source_for_generation(&state, super::CatalogSourcePlan::InMemory, Some(1))
            .await;
        assert!(catalog.revision() > revision);
        assert!(
            catalog
                .inner
                .lock()
                .await
                .sources
                .contains_key(&super::native_source_key())
        );
        drop(state);
    }
    #[tokio::test]
    async fn partial_enumeration_retains_successful_plans_for_retry() {
        let root = tempfile::tempdir().unwrap();
        let state = std::sync::Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::persistent_lazy(root.path()),
        ));
        let catalog = &state.session_catalog;
        catalog
            .import_source_ids("failed", async { Err("unavailable".to_owned()) })
            .await;
        let mut revisions = catalog.subscribe();
        catalog
            .ensure_sources_with_imports(&state, root.path().to_path_buf(), async {
                vec![super::CatalogSourcePlan::InMemory]
            })
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let complete = {
                    let inner = catalog.inner.lock().await;
                    inner.retry_import_plans.contains_key(root.path())
                };
                if complete {
                    break;
                }
                revisions.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        let inner = catalog.inner.lock().await;
        let plans = inner.import_plans[root.path()].as_ref().unwrap();
        assert_eq!(plans.len(), 1);
        assert!(matches!(plans[0], super::CatalogSourcePlan::InMemory));
        drop(inner);
        catalog
            .ensure_sources_with_imports(&state, root.path().to_path_buf(), async {
                panic!("observation must not retry before backoff expires")
            })
            .await;
        tokio::task::yield_now().await;
        assert!(catalog.inner.lock().await.import_plans[root.path()].is_some());
        catalog
            .inner
            .lock()
            .await
            .retry_import_plans
            .insert(root.path().to_path_buf(), std::time::Instant::now());
        let (started, received) = tokio::sync::oneshot::channel();
        catalog
            .ensure_sources_with_imports(&state, root.path().to_path_buf(), async move {
                started.send(()).unwrap();
                Vec::new()
            })
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(5), received)
            .await
            .unwrap()
            .unwrap();
        drop(state);
    }
    #[tokio::test]
    async fn native_discovery_starts_before_blocked_import_enumeration() {
        let root = tempfile::tempdir().unwrap();
        let state = std::sync::Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::persistent_lazy(root.path()),
        ));
        {
            let (release, blocked) = tokio::sync::oneshot::channel();
            let planning = state.session_catalog.ensure_sources_with_imports(
                &state,
                root.path().to_path_buf(),
                async move {
                    blocked.await.unwrap();
                    Vec::new()
                },
            );
            tokio::pin!(planning);
            assert!(futures::poll!(&mut planning).is_ready());
            let inner = state.session_catalog.inner.lock().await;
            assert!(matches!(
                inner.sources[&super::native_source_key()].state,
                super::SourceCacheState::Loading { .. }
            ));
            assert_eq!(
                super::snapshot_locked(&inner, root.path(), true).status,
                bcode_session_models::SessionCatalogStatus::Loading
            );
            drop(inner);
            let mut session = summary(SessionId::new(), None);
            session.working_directory = root.path().canonicalize().unwrap();
            // Seed useful metadata while provider planning is deliberately blocked.
            state
                .session_catalog
                .apply_source_result(
                    super::native_source_key(),
                    super::native_metadata(),
                    Ok(SourceLoadResult {
                        sessions: vec![session.clone()],
                        diagnostics: SourceDiagnostics::default(),
                    }),
                )
                .await;
            // Use the same normalized scope as the public listing operation.
            {
                let mut inner = state.session_catalog.inner.lock().await;
                let pending = inner.import_plans.remove(root.path()).unwrap();
                inner
                    .import_plans
                    .insert(session.working_directory.clone(), pending);
            }
            {
                let listing = crate::session_operations::list(&state, &session.working_directory);
                tokio::pin!(listing);
                let std::task::Poll::Ready(result) = futures::poll!(&mut listing) else {
                    panic!("useful application results waited for import enumeration");
                };
                let snapshot = result.unwrap();
                assert_eq!(snapshot.sessions, vec![session.clone()]);
                assert_eq!(
                    snapshot.status,
                    bcode_session_models::SessionCatalogStatus::Loading
                );
            }
            release.send(()).unwrap();
        }
        drop(state);
    }
    #[tokio::test]
    async fn native_mutations_during_discovery_require_fresh_load() {
        for delete in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let state = std::sync::Arc::new(crate::tests::test_server_state(
                bcode_session::SessionManager::persistent_lazy(root.path()),
            ));
            let catalog = &state.session_catalog;
            let session = summary(SessionId::new(), None);
            catalog.inner.lock().await.sources.insert(
                super::native_source_key(),
                super::SourceCache {
                    metadata: super::native_metadata(),
                    state: super::SourceCacheState::Loading {
                        sessions: Vec::new(),
                    },
                    updated_at_ms: 0,
                    generation: 1,
                },
            );
            let revision = catalog.revision();
            if delete {
                catalog.remove_native_session(session.id).await;
            } else {
                catalog.upsert_native_session(session.clone()).await;
            }
            assert!(catalog.revision() > revision);
            catalog
                .publish_source_result(
                    super::native_source_key(),
                    super::native_metadata(),
                    Ok(SourceLoadResult {
                        sessions: vec![session],
                        diagnostics: SourceDiagnostics::default(),
                    }),
                    Some(1),
                )
                .await;
            let inner = catalog.inner.lock().await;
            assert!(matches!(
                inner.sources[&super::native_source_key()].state,
                super::SourceCacheState::Empty
            ));
            assert!(
                inner.sources[&super::native_source_key()]
                    .sessions()
                    .is_empty()
            );
            drop(inner);
            let mut revisions = catalog.subscribe();
            catalog
                .ensure_source(&state, super::CatalogSourcePlan::InMemory)
                .await;
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let loaded = {
                        let inner = catalog.inner.lock().await;
                        matches!(
                            inner.sources[&super::native_source_key()].state,
                            super::SourceCacheState::Loaded { .. }
                        )
                    };
                    if loaded {
                        break;
                    }
                    revisions.changed().await.unwrap();
                }
            })
            .await
            .expect("invalidated discovery must complete a fresh load");
            let inner = catalog.inner.lock().await;
            assert!(
                inner.sources[&super::native_source_key()]
                    .sessions()
                    .is_empty()
            );
            drop(inner);
            drop(state);
        }
    }
    #[tokio::test]
    async fn native_mutations_fence_pending_refresh_publication() {
        let catalog = SessionCatalog::default();
        let original = summary(SessionId::new(), None);
        catalog
            .apply_source_result(
                super::native_source_key(),
                super::native_metadata(),
                Ok(SourceLoadResult {
                    sessions: vec![original.clone()],
                    diagnostics: SourceDiagnostics::default(),
                }),
            )
            .await;
        let generation = catalog.inner.lock().await.sources[&super::native_source_key()].generation;
        let mut renamed = original.clone();
        renamed.explicit_name = Some("renamed".to_owned());
        catalog.upsert_native_session(renamed.clone()).await;
        catalog
            .publish_source_result(
                super::native_source_key(),
                super::native_metadata(),
                Ok(SourceLoadResult {
                    sessions: vec![original.clone()],
                    diagnostics: SourceDiagnostics::default(),
                }),
                Some(generation),
            )
            .await;
        assert_eq!(
            catalog.inner.lock().await.sources[&super::native_source_key()].sessions(),
            &[renamed]
        );
        let generation = catalog.inner.lock().await.sources[&super::native_source_key()].generation;
        catalog.remove_native_session(original.id).await;
        catalog
            .publish_source_result(
                super::native_source_key(),
                super::native_metadata(),
                Ok(SourceLoadResult {
                    sessions: vec![original],
                    diagnostics: SourceDiagnostics::default(),
                }),
                Some(generation),
            )
            .await;
        assert!(
            catalog.inner.lock().await.sources[&super::native_source_key()]
                .sessions()
                .is_empty()
        );
    }
    #[tokio::test]
    async fn invalidated_source_load_cannot_publish_over_fresh_results() {
        let catalog = SessionCatalog::default();
        let session = summary(SessionId::new(), None);
        catalog
            .apply_source_result(
                super::native_source_key(),
                super::native_metadata(),
                Ok(SourceLoadResult {
                    sessions: vec![session.clone()],
                    diagnostics: SourceDiagnostics::default(),
                }),
            )
            .await;
        let generation = catalog.inner.lock().await.sources[&super::native_source_key()].generation;
        catalog.invalidate_source_id(super::NATIVE_SOURCE_ID).await;
        let revision = catalog.revision();
        catalog
            .publish_source_result(
                super::native_source_key(),
                super::native_metadata(),
                Err("stale failure".to_owned()),
                Some(generation),
            )
            .await;
        assert_eq!(catalog.revision(), revision);
        catalog
            .apply_source_result(
                super::native_source_key(),
                super::native_metadata(),
                Ok(SourceLoadResult {
                    sessions: vec![session.clone()],
                    diagnostics: SourceDiagnostics::default(),
                }),
            )
            .await;
        let revision = catalog.revision();
        catalog
            .publish_source_result(
                super::native_source_key(),
                super::native_metadata(),
                Ok(SourceLoadResult {
                    sessions: Vec::new(),
                    diagnostics: SourceDiagnostics::default(),
                }),
                Some(generation),
            )
            .await;
        assert_eq!(catalog.revision(), revision);
        assert_eq!(
            catalog.inner.lock().await.sources[&super::native_source_key()].sessions(),
            &[session]
        );
    }
    #[tokio::test]
    async fn import_enumeration_coalesces_and_warm_reads_skip_provider_work() {
        let catalog = SessionCatalog::default();
        let (release, blocked) = tokio::sync::oneshot::channel();
        let first = catalog.import_source_ids("provider", async {
            blocked.await.unwrap();
            Ok(vec!["source".to_owned()])
        });
        tokio::pin!(first);
        assert!(futures::poll!(&mut first).is_pending());
        let second = catalog.import_source_ids("provider", async {
            panic!("concurrent enumeration must share initialization")
        });
        tokio::pin!(second);
        assert!(futures::poll!(&mut second).is_pending());
        release.send(()).unwrap();
        assert_eq!(first.await, vec!["source"]);
        assert_eq!(second.await, vec!["source"]);
        assert_eq!(
            catalog
                .import_source_ids("provider", async {
                    panic!("warm observation must not poll provider")
                })
                .await,
            vec!["source"]
        );
    }

    #[tokio::test]
    async fn import_enumeration_retries_errors_and_cancelled_initialization() {
        let catalog = SessionCatalog::default();
        assert!(
            catalog
                .import_source_ids("provider", async { Err("offline".to_owned()) })
                .await
                .is_empty()
        );
        {
            let pending = catalog.import_source_ids("provider", std::future::pending());
            tokio::pin!(pending);
            assert!(futures::poll!(&mut pending).is_pending());
        }
        assert_eq!(
            catalog
                .import_source_ids("provider", async { Ok(vec!["recovered".to_owned()]) })
                .await,
            vec!["recovered"]
        );
        catalog.import_sources.lock().await.clear();
        assert_eq!(
            catalog
                .import_source_ids("provider", async { Ok(vec!["new source".to_owned()]) })
                .await,
            vec!["new source"]
        );
    }

    #[tokio::test]
    async fn import_refresh_does_not_restore_an_in_flight_old_enumeration() {
        let catalog = SessionCatalog::default();
        let (release, blocked) = tokio::sync::oneshot::channel();
        let old = catalog.import_source_ids("provider", async {
            blocked.await.unwrap();
            Ok(vec!["old".to_owned()])
        });
        tokio::pin!(old);
        assert!(futures::poll!(&mut old).is_pending());
        catalog.import_sources.lock().await.clear();
        assert_eq!(
            catalog
                .import_source_ids("provider", async { Ok(vec!["new".to_owned()]) })
                .await,
            vec!["new"]
        );
        release.send(()).unwrap();
        assert_eq!(old.await, vec!["old"]);
        assert_eq!(
            catalog
                .import_source_ids("provider", async {
                    panic!("fresh cache must survive old completion")
                })
                .await,
            vec!["new"]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn public_watcher_receives_cold_rows_and_discovery_completion_over_ipc() {
        let root = tempfile::tempdir().unwrap();
        let directory = std::env::current_dir().unwrap();
        let writer = bcode_session::SessionManager::persistent_lazy(root.path());
        let expected = writer
            .create_session(Some("IPC cold row".to_owned()), directory)
            .await
            .unwrap();
        writer.shutdown_catalog_updates().await;
        drop(writer);
        let mut state = std::sync::Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::persistent_lazy(root.path()),
        ));
        std::sync::Arc::get_mut(&mut state)
            .unwrap()
            .daemon_status
            .state_location_id = Some(bcode_ipc::state_location_id());
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        *state.session_catalog.discovery_gate.lock().await = Some(gate.clone());
        state.start_catalog_event_forwarder().await;
        let socket_dir = tempfile::tempdir().unwrap();
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.path().join("catalog.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            crate::handle_client(stream, server_state).await.unwrap();
        });
        let client = bcode_client::BcodeClient::new(endpoint);
        let mut watcher = client.watch_session_catalog().await.unwrap();
        let initial = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            watcher.initial_snapshot(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(initial.sessions.len(), 1);
        assert_eq!(initial.sessions[0].id, expected.id);
        assert_eq!(
            initial.catalog_status,
            bcode_session_models::SessionCatalogStatus::Loading
        );
        gate.add_permits(1);
        drop(gate);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let next = watcher.next_snapshot().await.unwrap();
                assert_eq!(next.sessions.len(), 1);
                assert_eq!(next.sessions[0].id, expected.id);
                if next.catalog_status == bcode_session_models::SessionCatalogStatus::Loaded {
                    break;
                }
            }
        })
        .await
        .unwrap();
        drop(watcher);
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        state.request_shutdown();
        state.stop_catalog_workers().await.unwrap();
        drop(state);
    }

    #[tokio::test]
    async fn cold_catalog_round_trip_preserves_persisted_metadata() {
        let root = tempfile::tempdir().unwrap();
        let writer = bcode_session::SessionManager::persistent_lazy(root.path());
        let mut expected = std::collections::BTreeMap::new();
        for index in 0..259 {
            let session = writer
                .create_session(
                    Some(format!("cold catalog row {index}")),
                    root.path().to_owned(),
                )
                .await
                .unwrap();
            expected.insert(session.id, session.name);
        }
        writer.flush_catalog_updates().await;
        drop(writer);
        let mut state = std::sync::Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::persistent_lazy(root.path()),
        ));
        std::sync::Arc::get_mut(&mut state)
            .unwrap()
            .daemon_status
            .state_location_id = Some("cold-location".to_owned());
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        *state.session_catalog.discovery_gate.lock().await = Some(gate.clone());
        let mut revisions = state.session_catalog.subscribe();
        let started = std::time::Instant::now();
        let snapshot = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            crate::session_operations::list(&state, root.path()),
        )
        .await
        .expect("cold catalog listing must complete")
        .unwrap();
        let first_useful = started.elapsed();
        assert!(!snapshot.sessions.is_empty());
        assert!(snapshot.sessions.len() <= expected.len());
        for session in &snapshot.sessions {
            assert_eq!(expected.get(&session.id), Some(&session.name));
        }
        assert_eq!(
            snapshot.status,
            bcode_session_models::SessionCatalogStatus::Loading
        );
        assert!(matches!(
            state.sessions.catalog_status(),
            bcode_session::CatalogLoadStatus::NotStarted
        ));
        gate.add_permits(1);
        drop(gate);
        let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                revisions.borrow_and_update();
                let snapshot = state.session_catalog.snapshot(&state, root.path()).await;
                if snapshot.status == bcode_session_models::SessionCatalogStatus::Loaded {
                    break snapshot;
                }
                revisions.changed().await.unwrap();
            }
        })
        .await
        .expect("discovery must finish after release");
        let completion = started.elapsed();
        assert_eq!(completed.sessions.len(), expected.len());
        assert_eq!(
            completed
                .sessions
                .iter()
                .map(|session| (session.id, session.name.clone()))
                .collect::<std::collections::BTreeMap<_, _>>(),
            expected
        );
        let warm_started = std::time::Instant::now();
        let reopened = crate::session_operations::list(&state, root.path())
            .await
            .unwrap();
        let warm = warm_started.elapsed();
        assert_eq!(reopened.sessions, completed.sessions);
        eprintln!(
            "catalog server rows={} first_rows={} first_useful={first_useful:?} completed={completion:?} warm={warm:?}",
            expected.len(),
            snapshot.sessions.len()
        );
        drop(state);
    }

    #[tokio::test]
    async fn available_catalog_results_do_not_wait_for_unstarted_native_discovery() {
        let root = tempfile::tempdir().unwrap();
        let state = std::sync::Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::persistent_lazy(root.path()),
        ));
        let session = summary(SessionId::new(), None);
        state
            .session_catalog
            .apply_source_result(
                super::native_source_key(),
                super::native_metadata(),
                Ok(SourceLoadResult {
                    sessions: vec![session.clone()],
                    diagnostics: SourceDiagnostics::default(),
                }),
            )
            .await;
        // Keep an additional relevant source loading so aggregate status cannot be
        // mistaken for complete. No task is responsible for completing this source.
        let key = super::CatalogSourceKey {
            source_id: "slow".to_owned(),
            scope: super::CatalogSourceScope::Global,
        };
        state.session_catalog.inner.lock().await.sources.insert(
            key,
            super::SourceCache {
                metadata: super::SourceMetadata {
                    display_name: "Slow source".to_owned(),
                },
                state: super::SourceCacheState::Loading {
                    sessions: Vec::new(),
                },
                updated_at_ms: 0,
                generation: 0,
            },
        );
        let snapshot = {
            let listing = crate::session_operations::list(&state, &session.working_directory);
            tokio::pin!(listing);
            match futures::poll!(&mut listing) {
                std::task::Poll::Ready(result) => result.unwrap(),
                std::task::Poll::Pending => panic!("available results waited for discovery"),
            }
        };
        assert_eq!(snapshot.sessions, vec![session.clone()]);
        assert_eq!(
            snapshot.status,
            bcode_session_models::SessionCatalogStatus::Loading
        );
        assert!(matches!(
            state.sessions.catalog_status(),
            bcode_session::CatalogLoadStatus::NotStarted
        ));
        drop(state);
    }

    #[tokio::test]
    #[ignore = "diagnostic timing run; no machine-dependent latency threshold"]
    async fn available_catalog_listing_timing() {
        for count in [100, 1_000, 10_000] {
            let root = tempfile::tempdir().unwrap();
            let state = std::sync::Arc::new(crate::tests::test_server_state(
                bcode_session::SessionManager::persistent_lazy(root.path()),
            ));
            let sessions = (0..count)
                .map(|_| summary(SessionId::new(), None))
                .collect::<Vec<_>>();
            let directory = sessions[0].working_directory.clone();
            state
                .session_catalog
                .apply_source_result(
                    super::native_source_key(),
                    super::native_metadata(),
                    Ok(SourceLoadResult {
                        sessions,
                        diagnostics: SourceDiagnostics::default(),
                    }),
                )
                .await;
            let start = std::time::Instant::now();
            let initial = crate::session_operations::list(&state, &directory)
                .await
                .unwrap();
            let initial_elapsed = start.elapsed();
            assert_eq!(initial.sessions.len(), count);
            let start = std::time::Instant::now();
            for _ in 0..10 {
                assert_eq!(
                    crate::session_operations::list(&state, &directory)
                        .await
                        .unwrap()
                        .sessions
                        .len(),
                    count
                );
            }
            eprintln!(
                "available catalog count={count} first={initial_elapsed:?} warm_mean={:?}",
                start.elapsed() / 10
            );
            drop(state);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn snapshot_directory_resolution_observes_symlink_changes() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        let catalog = SessionCatalog::default();
        let mut session = summary(SessionId::new(), None);
        session.working_directory = alias.clone();
        catalog
            .apply_source_result(
                super::native_source_key(),
                super::native_metadata(),
                Ok(SourceLoadResult {
                    sessions: vec![session],
                    diagnostics: SourceDiagnostics::default(),
                }),
            )
            .await;
        {
            let inner = catalog.inner.lock().await;
            assert_eq!(
                super::snapshot_locked(&inner, &first.canonicalize().unwrap(), true)
                    .sessions
                    .len(),
                1
            );
        }
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&second, &alias).unwrap();
        let inner = catalog.inner.lock().await;
        assert!(
            super::snapshot_locked(&inner, &first.canonicalize().unwrap(), true)
                .sessions
                .is_empty()
        );
        assert_eq!(
            super::snapshot_locked(&inner, &second.canonicalize().unwrap(), true)
                .sessions
                .len(),
            1
        );
    }

    #[test]
    fn native_primary_uses_retained_store_and_identity() {
        let root = tempfile::tempdir().expect("store root");
        let mut state = crate::tests::test_server_state(
            bcode_session::SessionManager::persistent_lazy(root.path()),
        );
        state.daemon_status.state_location_id = Some("retained-location".to_owned());
        let locations = super::native_locations(&state);
        let primary = locations
            .iter()
            .find(|location| location.primary)
            .expect("primary");
        assert_eq!(primary.sessions_root, root.path());
        assert_eq!(primary.location_id, "retained-location");
        state.daemon_status.state_location_id = None;
        assert!(super::native_locations(&state).is_empty());
        drop(state);
    }
    use super::{
        CatalogSourcePlan, NativeLocation, SessionCatalog, SourceDiagnostics, SourceLoadResult,
        mark_ambiguous_locations,
    };
    use bcode_session_models::{
        SessionId, SessionLocationSummary, SessionSummary, SessionTitleSource,
    };
    use std::path::PathBuf;

    fn location(location_id: &str, profile: Option<&str>, primary: bool) -> NativeLocation {
        NativeLocation {
            location_id: location_id.to_owned(),
            profile: profile.map(str::to_owned),
            sessions_root: PathBuf::from("/tmp").join(location_id),
            primary,
        }
    }

    fn summary(id: SessionId, location: Option<SessionLocationSummary>) -> SessionSummary {
        SessionSummary {
            id,
            name: None,
            explicit_name: None,
            derived_title: None,
            title_source: SessionTitleSource::EmptyDraft,
            client_count: 0,
            created_at_ms: 0,
            updated_at_ms: 0,
            working_directory: PathBuf::from("/tmp/workspace"),
            import: None,
            execution: None,
            location,
        }
    }

    #[tokio::test]
    async fn catalog_mutations_publish_matching_revisions() {
        let catalog = SessionCatalog::default();
        let mut revisions = catalog.subscribe();
        let session = summary(SessionId::new(), None);
        catalog
            .apply_source_result(
                super::native_source_key(),
                super::native_metadata(),
                Ok(SourceLoadResult {
                    sessions: vec![session.clone()],
                    diagnostics: SourceDiagnostics::default(),
                }),
            )
            .await;
        assert_eq!(*revisions.borrow_and_update(), 1);
        catalog.upsert_native_session(session.clone()).await;
        assert!(!revisions.has_changed().unwrap());
        let mut renamed = session.clone();
        renamed.name = Some("renamed".into());
        catalog.upsert_native_session(renamed).await;
        let inner = catalog.inner.lock().await;
        let snapshot = super::snapshot_locked(&inner, &session.working_directory, true);
        drop(inner);
        assert_eq!(snapshot.revision, 2);
        assert_eq!(snapshot.sessions[0].name.as_deref(), Some("renamed"));
        assert_eq!(*revisions.borrow_and_update(), snapshot.revision);
        catalog.remove_native_session(session.id).await;
        assert_eq!(*revisions.borrow_and_update(), 3);
        catalog.invalidate_native().await;
        assert_eq!(*revisions.borrow_and_update(), 4);
        let inner = catalog.inner.lock().await;
        let snapshot = super::snapshot_locked(&inner, &session.working_directory, true);
        drop(inner);
        assert_eq!(snapshot.revision, 4);
        assert!(snapshot.sessions.is_empty());
    }

    #[tokio::test]
    async fn source_timestamps_change_on_updates_not_observation() {
        let catalog = SessionCatalog::default();
        let key = super::native_source_key();
        let session = summary(SessionId::new(), None);
        catalog
            .apply_source_result(
                key.clone(),
                super::native_metadata(),
                Ok(SourceLoadResult {
                    sessions: vec![session.clone()],
                    diagnostics: SourceDiagnostics::default(),
                }),
            )
            .await;
        {
            let mut inner = catalog.inner.lock().await;
            let source = inner.sources.get_mut(&key).unwrap();
            source.updated_at_ms = 42;
            let first = super::source_status(&key, source);
            let second = super::source_status(&key, source);
            drop(inner);
            assert_eq!(first.updated_at_ms, 42);
            assert_eq!(first, second);
        }
        catalog.upsert_native_session(session.clone()).await;
        catalog.remove_native_session(SessionId::new()).await;
        assert_eq!(catalog.inner.lock().await.sources[&key].updated_at_ms, 42);
        let mut renamed = session.clone();
        renamed.name = Some("renamed".into());
        catalog.upsert_native_session(renamed).await;
        assert!(catalog.inner.lock().await.sources[&key].updated_at_ms > 42);
        catalog
            .inner
            .lock()
            .await
            .sources
            .get_mut(&key)
            .unwrap()
            .updated_at_ms = 42;
        catalog.remove_native_session(session.id).await;
        assert!(catalog.inner.lock().await.sources[&key].updated_at_ms > 42);
        catalog
            .inner
            .lock()
            .await
            .sources
            .get_mut(&key)
            .unwrap()
            .updated_at_ms = 42;
        catalog.invalidate_native().await;
        assert!(catalog.inner.lock().await.sources[&key].updated_at_ms > 42);
        catalog
            .inner
            .lock()
            .await
            .sources
            .get_mut(&key)
            .unwrap()
            .updated_at_ms = 42;
        catalog
            .apply_source_result(
                key.clone(),
                super::native_metadata(),
                Err("unavailable".into()),
            )
            .await;
        assert!(catalog.inner.lock().await.sources[&key].updated_at_ms > 42);
    }

    #[test]
    fn primary_location_keeps_the_stable_native_source_identity() {
        let primary = location("aaaa", None, true);
        let foreign = location("bbbb", Some("big"), false);

        assert_eq!(primary.source_id(), "native");
        assert_eq!(foreign.source_id(), "native:bbbb");
        assert_ne!(primary.source_id(), foreign.source_id());
    }

    #[test]
    fn location_display_names_prefer_profile_labels_without_leaking_paths() {
        assert_eq!(
            location("aaaa", None, true).display_name(),
            "Native Bcode sessions"
        );
        assert_eq!(
            location("bbbb", Some("big"), false).display_name(),
            "Bcode sessions [big]"
        );

        // An unnamed foreign location falls back to its opaque identity, never its root path.
        let unnamed = location("cccc", None, false);
        let display = unnamed.display_name();
        assert!(display.contains("cccc"), "{display}");
        assert!(!display.contains("/tmp"), "{display}");
    }

    #[test]
    fn duplicate_session_ids_across_locations_are_marked_ambiguous_not_merged() {
        let shared = SessionId::new();
        let unique = SessionId::new();
        let mut sessions = vec![
            summary(shared, Some(location("aaaa", None, true).summary())),
            summary(shared, Some(location("bbbb", Some("big"), false).summary())),
            summary(unique, Some(location("aaaa", None, true).summary())),
        ];

        mark_ambiguous_locations(&mut sessions);

        assert_eq!(
            sessions.len(),
            3,
            "duplicate claims must be retained rather than merged"
        );
        let ambiguous = sessions
            .iter()
            .filter(|session| session.id == shared)
            .collect::<Vec<_>>();
        assert_eq!(ambiguous.len(), 2);
        assert!(
            ambiguous
                .iter()
                .all(|session| session.location.as_ref().is_some_and(|l| l.ambiguous)),
            "every claim on a duplicated session ID must be flagged ambiguous"
        );
        let unaffected = sessions
            .iter()
            .find(|session| session.id == unique)
            .expect("unique session");
        assert!(
            !unaffected.location.as_ref().expect("location").ambiguous,
            "an unambiguous session must not be flagged"
        );
    }

    #[test]
    fn one_location_claiming_a_session_is_never_ambiguous() {
        let id = SessionId::new();
        let mut sessions = vec![summary(id, Some(location("aaaa", None, true).summary()))];

        mark_ambiguous_locations(&mut sessions);

        assert!(!sessions[0].location.as_ref().expect("location").ambiguous);
    }

    #[test]
    fn sessions_without_location_metadata_are_left_untouched() {
        let id = SessionId::new();
        let mut sessions = vec![summary(id, None), summary(id, None)];

        mark_ambiguous_locations(&mut sessions);

        assert!(sessions.iter().all(|session| session.location.is_none()));
    }

    /// Populate one loaded catalog source directly, without discovery or storage access.
    ///
    /// Keys and metadata come from the production `CatalogSourcePlan`, so the seeded state
    /// is identical in shape to what real per-location discovery produces.
    async fn seed_source(catalog: &SessionCatalog, location: &NativeLocation, id: SessionId) {
        let plan = CatalogSourcePlan::Native {
            location: location.clone(),
        };
        catalog
            .apply_source_result(
                plan.key(),
                plan.metadata(),
                Ok(SourceLoadResult {
                    sessions: vec![summary(id, Some(location.summary()))],
                    diagnostics: SourceDiagnostics::default(),
                }),
            )
            .await;
    }

    /// The refusal the attach handlers consult must report every claiming location once
    /// more than one readable location claims the same session ID, and must report
    /// nothing when the session is unambiguous.
    #[tokio::test]
    async fn ambiguous_location_ids_reports_conflicting_claims_from_loaded_sources() {
        let catalog = SessionCatalog::default();
        let primary = location("aaaa", None, true);
        let foreign = location("bbbb", Some("big"), false);

        let unique = SessionId::new();
        seed_source(&catalog, &primary, unique).await;
        assert!(
            catalog.ambiguous_location_ids(unique).await.is_empty(),
            "a session claimed by exactly one location must not be reported ambiguous"
        );

        let shared = SessionId::new();
        seed_source(&catalog, &primary, shared).await;
        seed_source(&catalog, &foreign, shared).await;

        let claims = catalog.ambiguous_location_ids(shared).await;
        assert_eq!(
            claims,
            vec!["aaaa".to_owned(), "bbbb".to_owned()],
            "every claiming location must be surfaced so the conflict is actionable"
        );
        assert!(
            catalog.ambiguous_location_ids(unique).await.is_empty(),
            "ambiguity must not leak across session IDs"
        );
    }
}
