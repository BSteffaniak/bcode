#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
// Session mutations intentionally hold the manager lock while updating in-memory
// state and appending the corresponding event so summaries/history/fanout stay
// consistent in this first implementation.
#![allow(clippy::significant_drop_tightening)]

//! Session lifecycle, attachment management, and append-only event history.
//!
//! Model context is a bounded projection of canonical events. Local and provider-native
//! compaction markers are equivalent boundaries: the newest marker is selected by its own event
//! sequence, while its `compacted_through_sequence` identifies the canonical prefix it replaces.
//! Normal model-context reads return that marker plus later semantic events without replaying or
//! repairing the complete event log.

mod actor;
mod attach;
mod attachment;
mod catalog;
mod context;
mod current_schema;
pub mod db;
pub(crate) mod db_artifact;
pub(crate) mod db_connection;
pub(crate) mod db_context;
pub(crate) mod db_contract;
pub(crate) mod db_event_store;
mod db_path;
pub(crate) mod db_projection;
pub(crate) mod db_projection_row;
pub(crate) mod db_row;
pub(crate) mod db_runtime_work;
pub(crate) mod db_validation;
mod fork;
pub mod lease;
mod manifest;
mod mutation;
pub mod ownership;
pub mod persisted;
pub mod projection;
pub mod repair;
mod runtime_work;
pub(crate) mod state;
mod store;
mod store_executor;
mod subscription;
mod tools;

use actor::{AttachMode, SessionHandle};
pub use attachment::{
    SessionAttachment, SessionEventSubscription, SessionMutationCommitted,
    SessionProjectionWindowAttachment,
};
use bcode_metrics::{MetricLabels, MetricsRegistry};
use bcode_session_models::{
    ClientId, ExecutionSessionContextMode, ExecutionSessionProvenance, ProjectionWindow,
    ProjectionWindowRequest, SessionEvent, SessionEventKind, SessionEventProvenance,
    SessionForkKind, SessionHistoryDirection, SessionHistoryPage, SessionHistoryQuery, SessionId,
    SessionImportSummary, SessionInputHistoryEntry, SessionLiveEvent, SessionLiveEventKind,
    SessionMigrationProgress, SessionMigrationStage, SessionOpenOperationId,
    SessionOpenOperationSnapshot, SessionOpenTerminalOutcome, SessionSummary, SessionTitleSource,
    SessionVisibility,
};
pub use catalog::{
    CatalogLoadStatus, SessionCatalogEntry, SessionCatalogLoadStatus, SessionHealth,
};
use lease::{SessionLeaseGuard, SessionLeaseOwnerContext};
pub use manifest::{
    CURRENT_SESSION_FORMAT_EPOCH, SESSION_FORMAT_FAMILY, SESSION_MANIFEST_SCHEMA_VERSION,
};
use manifest::{SessionFormatMarker, SessionManifest};
use state::{SessionLiveEventBroker, SessionLoadStatusKind, SessionState};
use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicU64};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
pub use store::{SessionStore, SessionStoreError};
use store_executor::SessionStoreExecutor;
use thiserror::Error;
use tokio::sync::{Mutex, broadcast, watch};
use tokio::task::spawn_blocking;

/// Return the stable kind name when a session event is live-only and must not be persisted.
fn live_only_session_event_kind(kind: &SessionEventKind) -> Option<&'static str> {
    match kind {
        SessionEventKind::ToolContribution { event }
            if matches!(
                event.persistence,
                bcode_session_models::ToolContributionPersistence::Transient
            ) =>
        {
            Some("tool_contribution")
        }
        SessionEventKind::ToolContributionPlaced { envelope }
            if matches!(
                envelope.contribution.persistence,
                bcode_session_models::ToolContributionPersistence::Transient
            ) =>
        {
            Some("tool_contribution_placed")
        }
        SessionEventKind::ToolContributionPlaced { envelope }
            if envelope.placement == bcode_session_models::ToolContributionPlacement::Progress =>
        {
            Some("tool_contribution_progress")
        }
        _ => None,
    }
}

const MAX_DURABLE_GENERIC_EVENT_BYTES: usize = 64 * 1024;

fn ensure_durable_session_event_kind(
    kind: &SessionEventKind,
    metrics: Option<&MetricsRegistry>,
) -> Result<(), SessionError> {
    if matches!(kind, SessionEventKind::InertHistory { .. }) {
        return Err(SessionError::EventSerialization(
            "historical compatibility events cannot be appended".to_owned(),
        ));
    }
    if let Some(event_kind) = live_only_session_event_kind(kind) {
        if let Some(metrics) = metrics {
            metrics.increment_counter("session.event.live_persistence_rejected");
        }
        return Err(SessionError::LiveEventPersistenceRejected { event_kind });
    }
    if let SessionEventKind::ToolInvocationResultRecorded { record } = kind
        && record.presentation.as_ref().is_some_and(|presentation| {
            presentation.retention == bcode_session_models::ToolPresentationRetention::ActiveOnly
        })
    {
        return Err(SessionError::EventSerialization(
            "active-only presentation update cannot be persisted".to_owned(),
        ));
    }
    if matches!(
        kind,
        SessionEventKind::ToolContribution { .. } | SessionEventKind::ToolContributionPlaced { .. }
    ) {
        let payload_bytes = serde_json::to_vec(kind)
            .map_err(|error| SessionError::EventSerialization(error.to_string()))?
            .len();
        if payload_bytes > MAX_DURABLE_GENERIC_EVENT_BYTES {
            if let Some(metrics) = metrics {
                metrics.increment_counter("session.event.oversized_persistence_rejected");
                metrics.record_histogram(
                    "session.event.rejected_payload_bytes",
                    u64::try_from(payload_bytes).unwrap_or(u64::MAX),
                );
            }
            return Err(SessionError::DurableEventPayloadTooLarge {
                event_kind: "tool_contribution",
                payload_bytes,
                max_bytes: MAX_DURABLE_GENERIC_EVENT_BYTES,
            });
        }
    }
    Ok(())
}

fn record_session_event_domain_metrics(metrics: &MetricsRegistry, event: &SessionEvent) {
    if let Ok(payload) = serde_json::to_vec(event) {
        metrics.record_histogram("session.event.payload_bytes", payload.len() as u64);
    }
    if matches!(
        event.kind,
        SessionEventKind::UserMessage { .. }
            | SessionEventKind::AssistantMessage { .. }
            | SessionEventKind::AssistantResponseSegment { .. }
            | SessionEventKind::ToolCallRequested { .. }
            | SessionEventKind::ToolInvocationResultRecorded { .. }
            | SessionEventKind::SystemMessage { .. }
            | SessionEventKind::WorkingDirectoryChanged { .. }
            | SessionEventKind::ContextCompacted { .. }
            | SessionEventKind::ProviderContextCompacted { .. }
            | SessionEventKind::RequestContextObserved { .. }
    ) {
        metrics.increment_counter("session.event.semantic_rows");
    }
    match &event.kind {
        SessionEventKind::ToolInvocationResultRecorded { record } => {
            if let Some(bcode_session_models::ToolInvocationResult::Artifact { artifact }) =
                &record.result
            {
                metrics.add_counter(
                    "session.event.artifact_references",
                    u64::try_from(artifact.refs.len()).unwrap_or(u64::MAX),
                );
            }
        }
        SessionEventKind::ContextCompacted { .. }
        | SessionEventKind::ProviderContextCompacted { .. } => {
            metrics.increment_counter("session.event.compaction_boundaries");
        }
        _ => {}
    }
}

fn ensure_loaded_metric_labels(result: &str) -> MetricLabels {
    let mut labels = MetricLabels::new();
    labels.insert("result".to_owned(), result.to_owned());
    labels
}

fn terminal_session_open_snapshot(
    session_id: SessionId,
    outcome: SessionOpenTerminalOutcome,
    message: String,
) -> SessionOpenOperationSnapshot {
    SessionOpenOperationSnapshot {
        operation_id: SessionOpenOperationId::new(),
        revision: 0,
        session_id,
        source_writer_epoch: None,
        target_writer_epoch: u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
        progress: SessionMigrationProgress {
            stage: if matches!(outcome, SessionOpenTerminalOutcome::Ready) {
                SessionMigrationStage::Complete
            } else {
                SessionMigrationStage::Failed
            },
            completed_units: None,
            total_units: None,
            unit: None,
            message,
        },
        outcome: Some(outcome),
        backup_path: None,
    }
}

fn ready_session_open_snapshot(session_id: SessionId) -> SessionOpenOperationSnapshot {
    terminal_session_open_snapshot(
        session_id,
        SessionOpenTerminalOutcome::Ready,
        "Session storage is ready".to_owned(),
    )
}

fn record_ensure_loaded_duration(metrics: &MetricsRegistry, result: &str, elapsed_ms: u64) {
    metrics.record_histogram_with_labels(
        "session.manager.ensure_loaded.duration_ms",
        elapsed_ms,
        ensure_loaded_metric_labels(result),
    );
}

/// Runtime model and reasoning selections restored from a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionRuntimeSelection {
    /// Session-specific agent id, when explicitly selected.
    pub agent_id: Option<String>,
    /// Session-specific provider plugin id, when explicitly selected.
    pub provider_plugin_id: Option<String>,
    /// Session-specific model id, when explicitly selected.
    pub model_id: Option<String>,
    /// Session-specific reasoning effort, when explicitly selected.
    pub reasoning_effort: Option<String>,
    /// Session-specific reasoning summary, when explicitly selected.
    pub reasoning_summary: Option<String>,
}

/// Return a shared-session execution target after enforcing explicitly sequential admission.
///
/// # Errors
///
/// Returns an error when provenance is not shared-sequential or does not identify `parent`.
pub fn shared_execution_session(
    parent: SessionId,
    provenance: &ExecutionSessionProvenance,
) -> Result<SessionId, SessionError> {
    validate_execution_session_provenance(Some(provenance))?;
    if provenance.context_mode != ExecutionSessionContextMode::SharedSequential
        || provenance.parent_session_id != parent
    {
        return Err(SessionError::InvalidExecutionSessionProvenance(
            "shared execution must target its declared parent session".to_string(),
        ));
    }
    Ok(parent)
}

/// Owned admission permit for one explicitly shared-sequential execution session.
///
/// Holding this value guarantees that no other shared execution admitted through the same
/// [`SessionManager`] can use the parent session concurrently.
#[derive(Debug)]
pub struct SharedExecutionSessionPermit {
    session_id: SessionId,
    _permit: tokio::sync::OwnedMutexGuard<()>,
}

impl SharedExecutionSessionPermit {
    /// Return the serialized parent session target.
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }
}

const MAX_EXECUTION_PROVENANCE_ID_BYTES: usize = 512;

fn validate_execution_session_provenance(
    provenance: Option<&ExecutionSessionProvenance>,
) -> Result<(), SessionError> {
    let Some(provenance) = provenance else {
        return Ok(());
    };
    for (label, value) in [
        ("owner", provenance.owner.as_str()),
        ("run_id", provenance.run_id.as_str()),
        ("node_id", provenance.node_id.as_str()),
    ] {
        if value.trim().is_empty() || value.len() > MAX_EXECUTION_PROVENANCE_ID_BYTES {
            return Err(SessionError::InvalidExecutionSessionProvenance(format!(
                "{label} must contain 1..={MAX_EXECUTION_PROVENANCE_ID_BYTES} bytes"
            )));
        }
    }
    if provenance.attempt == 0 {
        return Err(SessionError::InvalidExecutionSessionProvenance(
            "attempt must be greater than zero".to_string(),
        ));
    }
    if provenance
        .workspace_snapshot
        .as_ref()
        .is_none_or(|snapshot| {
            snapshot.trim().is_empty() || snapshot.len() > MAX_EXECUTION_PROVENANCE_ID_BYTES
        })
    {
        return Err(SessionError::InvalidExecutionSessionProvenance(format!(
            "workspace_snapshot must contain 1..={MAX_EXECUTION_PROVENANCE_ID_BYTES} bytes"
        )));
    }
    match (provenance.context_mode, provenance.parent_generation) {
        (ExecutionSessionContextMode::FixedGenerationFork, None) => {
            return Err(SessionError::InvalidExecutionSessionProvenance(
                "fixed-generation fork requires parent_generation".to_string(),
            ));
        }
        (
            ExecutionSessionContextMode::FreshIsolated
            | ExecutionSessionContextMode::SharedSequential,
            Some(_),
        ) => {
            return Err(SessionError::InvalidExecutionSessionProvenance(
                "parent_generation is valid only for fixed-generation fork".to_string(),
            ));
        }
        _ => {}
    }
    Ok(())
}

/// Errors returned by session management operations.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(SessionId),
    #[error("session event store error: {0}")]
    Store(#[from] SessionStoreError),
    #[error("session has connected clients: {0}")]
    ConnectedClients(SessionId),
    #[error("session is being deleted: {0}")]
    Deleting(SessionId),
    /// A live-only event was passed to the durable append boundary.
    #[error("live-only session event cannot be persisted: {event_kind}")]
    LiveEventPersistenceRejected { event_kind: &'static str },
    /// A bounded durable event exceeded its event-kind-specific payload limit.
    #[error(
        "durable session event payload is too large: {event_kind} payload={payload_bytes} max={max_bytes}"
    )]
    DurableEventPayloadTooLarge {
        event_kind: &'static str,
        payload_bytes: usize,
        max_bytes: usize,
    },
    /// A durable event could not be measured before persistence.
    #[error("session event serialization failed before persistence: {0}")]
    EventSerialization(String),
    #[error("unsupported session projection window request")]
    UnsupportedProjectionWindow,
    #[error(
        "session DB projection is stale: {session_id} {projection} checkpoint={checkpoint:?} expected={expected}"
    )]
    ProjectionStale {
        session_id: SessionId,
        projection: &'static str,
        checkpoint: Option<u64>,
        expected: u64,
    },
    /// Turn admission metadata is invalid.
    #[error(transparent)]
    TurnAdmission(#[from] bcode_session_models::TurnAdmissionMetadataError),
    /// Session storage is a known legacy generation that requires an explicit maintenance migration.
    #[error(
        "session storage migration required: writer epoch {actual}, expected {expected}; run an explicit session migration/reindex command"
    )]
    StorageMigrationRequired { actual: u64, expected: u64 },
    /// A verified pre-migration backup could not be created.
    #[error("session migration backup failed for {session_id}: {reason}")]
    MigrationBackup {
        session_id: SessionId,
        reason: String,
    },
    /// Session database error: {0}
    #[error("session database error: {0}")]
    Db(#[from] db::SessionDbError),
    /// Session database is unavailable for this operation.
    #[error("session database is unavailable: {0}")]
    DbUnavailable(SessionId),
    /// Selected fork prompt could not be found.
    #[error("selected fork prompt not found in session {session_id}: sequence {sequence}")]
    ForkPromptNotFound {
        session_id: SessionId,
        sequence: u64,
    },
    #[error(
        "session generation changed before clone snapshot: {session_id} expected={expected} current={current}"
    )]
    CloneGenerationChanged {
        session_id: SessionId,
        expected: u64,
        current: u64,
    },
    /// Background execution-session provenance is malformed or inconsistent.
    #[error("invalid execution session provenance: {0}")]
    InvalidExecutionSessionProvenance(String),
    /// Session is owned by another daemon or cannot be leased.
    #[error(transparent)]
    Lease(#[from] lease::SessionLeaseError),
}

/// Input for appending a tool-call request event.
#[derive(Debug, Clone, Default)]
pub struct AppendToolCallRequestedInput {
    /// Provider tool call identifier.
    pub tool_call_id: String,
    /// Tool name requested by the model.
    pub tool_name: String,
    /// Raw JSON arguments requested by the model.
    pub arguments_json: String,
    /// Producer plugin id, when known.
    pub producer_plugin_id: Option<String>,
    /// Working directory captured for this invocation.
    pub working_directory: Option<std::path::PathBuf>,
}

/// In-memory session manager with optional DB-backed persistence.
#[derive(Debug, Clone)]
pub struct SessionManager {
    inner: Arc<Mutex<SessionManagerInner>>,
    store: Option<SessionStoreExecutor>,
    activity_clock_ms: Arc<AtomicU64>,
    catalog_status_tx: watch::Sender<CatalogLoadStatus>,
    catalog_status_rx: watch::Receiver<CatalogLoadStatus>,
    mutation_tx: broadcast::Sender<SessionMutationCommitted>,
    shared_execution_locks: Arc<Mutex<BTreeMap<SessionId, Arc<Mutex<()>>>>>,
    metrics: MetricsRegistry,
}

#[derive(Debug, Default)]
struct SessionManagerInner {
    sessions: BTreeMap<SessionId, SessionHandle>,
    leases: BTreeMap<SessionId, SessionLeaseGuard>,
    load_gates: BTreeMap<SessionId, Arc<Mutex<()>>>,
}

enum SessionLeaseLoadOutcome {
    Acquired(Box<SessionLeaseGuard>),
    Retry,
}

async fn classify_known_current_session_open(
    session_id: SessionId,
    db: &db::SessionDb,
) -> Result<SessionOpenOperationSnapshot, SessionError> {
    match db.current_open_readiness_known_current().await {
        Ok(()) => Ok(ready_session_open_snapshot(session_id)),
        Err(db::SessionDbError::WriterIncompatible { actual, expected }) => {
            Ok(terminal_session_open_snapshot(
                session_id,
                SessionOpenTerminalOutcome::WriterIncompatible { actual, expected },
                format!(
                    "Session writer epoch {actual:?} is incompatible with expected epoch {expected}"
                ),
            ))
        }
        Err(error) => Ok(terminal_session_open_snapshot(
            session_id,
            SessionOpenTerminalOutcome::RepairRequired {
                reason: error.to_string(),
            },
            "Session storage requires repair".to_owned(),
        )),
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        let (catalog_status_tx, catalog_status_rx) = watch::channel(CatalogLoadStatus::Loaded);
        Self {
            inner: Arc::new(Mutex::new(SessionManagerInner::default())),
            store: None,
            activity_clock_ms: Arc::new(AtomicU64::new(current_unix_millis())),
            catalog_status_tx,
            catalog_status_rx,
            mutation_tx: broadcast::channel(1024).0,
            shared_execution_locks: Arc::new(Mutex::new(BTreeMap::new())),
            metrics: MetricsRegistry::default(),
        }
    }
}

impl SessionManager {
    /// Create a session manager backed by a session store root.
    ///
    /// # Errors
    ///
    /// Returns an error if persisted session history cannot be loaded.
    pub fn persistent(root: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        Self::persistent_with_metrics(root, MetricsRegistry::default())
    }

    /// Create a session manager backed by a session store root with metrics instrumentation.
    ///
    /// # Errors
    ///
    /// Returns an error if persisted session history cannot be loaded.
    pub fn persistent_with_metrics(
        root: impl Into<PathBuf>,
        metrics: MetricsRegistry,
    ) -> Result<Self, SessionStoreError> {
        let store = SessionStore::with_metrics(root, metrics);
        let sessions = store.load_catalog()?;
        Ok(Self::from_store(store, sessions, true))
    }

    /// Create a session manager backed by a session store root with metrics and shared migration
    /// operation coordination.
    ///
    /// # Errors
    ///
    /// Create a session manager backed by a session store root with lease owner metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if persisted session history cannot be loaded.
    pub fn persistent_with_metrics_and_lease_owner(
        root: impl Into<PathBuf>,
        metrics: MetricsRegistry,
        lease_owner: SessionLeaseOwnerContext,
    ) -> Result<Self, SessionStoreError> {
        let store = SessionStore::with_metrics(root, metrics).with_lease_owner(lease_owner);
        let sessions = store.load_catalog()?;
        Ok(Self::from_store(store, sessions, true))
    }

    /// Create a session manager whose catalog is loaded on demand.
    #[must_use]
    pub fn persistent_lazy(root: impl Into<PathBuf>) -> Self {
        Self::persistent_lazy_with_metrics(root, MetricsRegistry::default())
    }

    /// Create a lazy persistent session manager with metrics instrumentation.
    #[must_use]
    pub fn persistent_lazy_with_metrics(
        root: impl Into<PathBuf>,
        metrics: MetricsRegistry,
    ) -> Self {
        let store = SessionStore::with_metrics(root, metrics);
        Self::from_store(store, BTreeMap::new(), false)
    }

    /// Create a lazy persistent session manager with lease owner metadata.
    #[must_use]
    pub fn persistent_lazy_with_metrics_and_lease_owner(
        root: impl Into<PathBuf>,
        metrics: MetricsRegistry,
        lease_owner: SessionLeaseOwnerContext,
    ) -> Self {
        let store = SessionStore::with_metrics(root, metrics).with_lease_owner(lease_owner);
        Self::from_store(store, BTreeMap::new(), false)
    }

    /// Create a lazy persistent session manager with lease owner metadata and shared migration
    fn from_store(
        store: SessionStore,
        sessions: BTreeMap<SessionId, SessionState>,
        catalog_loaded: bool,
    ) -> Self {
        let executor = SessionStoreExecutor::new(store);
        let metrics = executor.metrics();
        let catalog_status = if catalog_loaded {
            CatalogLoadStatus::Loaded
        } else {
            CatalogLoadStatus::NotStarted
        };
        let (catalog_status_tx, catalog_status_rx) = watch::channel(catalog_status);
        let (mutation_tx, _) = broadcast::channel(1024);
        Self {
            inner: Arc::new(Mutex::new(SessionManagerInner {
                sessions: sessions
                    .into_iter()
                    .map(|(session_id, state)| {
                        (
                            session_id,
                            SessionHandle::new(state, Some(executor.clone())),
                        )
                    })
                    .collect(),
                leases: BTreeMap::new(),
                load_gates: BTreeMap::new(),
            })),
            store: Some(executor),
            activity_clock_ms: Arc::new(AtomicU64::new(current_unix_millis())),
            catalog_status_tx,
            catalog_status_rx,
            mutation_tx,
            shared_execution_locks: Arc::new(Mutex::new(BTreeMap::new())),
            metrics,
        }
    }

    /// Start or join server-owned preparation for opening one persistent session.
    ///
    /// Current storage returns a terminal ready snapshot without spawning work. Known legacy
    /// storage starts one detached, per-session migration operation that survives observer loss.
    ///
    /// # Errors
    ///
    /// Returns an error if bounded storage classification fails or the session does not exist.
    pub async fn prepare_session_open(
        &self,
        session_id: SessionId,
    ) -> Result<SessionOpenOperationSnapshot, SessionError> {
        let Some(store) = &self.store else {
            return if self.inner.lock().await.sessions.contains_key(&session_id) {
                Ok(ready_session_open_snapshot(session_id))
            } else {
                Err(SessionError::NotFound(session_id))
            };
        };
        let root = store.root_path();
        if !db::session_db_path(&root, session_id).exists() {
            return Err(SessionError::NotFound(session_id));
        }
        if self.inner.lock().await.leases.contains_key(&session_id) {
            let db = db::SessionDb::open_existing_turso_in_root(session_id, &root).await?;
            return classify_known_current_session_open(session_id, &db).await;
        }
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &root).await?;
        let compatibility = match db.storage_compatibility().await {
            Ok(compatibility) => compatibility,
            Err(db::SessionDbError::WriterIncompatible { actual, expected }) => {
                return Ok(terminal_session_open_snapshot(
                    session_id,
                    SessionOpenTerminalOutcome::WriterIncompatible { actual, expected },
                    format!(
                        "Session writer epoch {actual:?} is incompatible with expected epoch {expected}"
                    ),
                ));
            }
            Err(error) => {
                return Ok(terminal_session_open_snapshot(
                    session_id,
                    SessionOpenTerminalOutcome::RepairRequired {
                        reason: error.to_string(),
                    },
                    "Session storage requires repair".to_owned(),
                ));
            }
        };
        if let db::SessionStorageCompatibility::KnownLegacy { writer_epoch } = compatibility {
            return Err(SessionError::StorageMigrationRequired {
                actual: writer_epoch,
                expected: u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
            });
        }
        classify_known_current_session_open(session_id, &db).await
    }

    /// Load a session through the strict current runtime.
    ///
    /// # Errors
    ///
    /// Returns an error when storage is not current-ready or loading fails.
    pub async fn load_current_session(&self, session_id: SessionId) -> Result<(), SessionError> {
        self.ensure_session_loaded(session_id).await
    }

    /// Return the persistent session store root, when this manager is store-backed.
    #[must_use]
    pub fn session_store_root(&self) -> Option<PathBuf> {
        self.store.as_ref().map(SessionStoreExecutor::root_path)
    }

    async fn load_db_session_state(
        &self,
        session_id: SessionId,
        db: &db::SessionDb,
    ) -> Result<SessionState, SessionError> {
        let Some(db_state) = db.session_state().await? else {
            return Err(SessionError::ProjectionStale {
                session_id,
                projection: "session_state",
                checkpoint: None,
                expected: db.last_event_sequence().await?.unwrap_or(0),
            });
        };
        let expected_last_sequence = db
            .last_event_sequence()
            .await?
            .unwrap_or(db_state.last_event_seq);
        if db_state.last_event_seq < expected_last_sequence {
            return Err(SessionError::ProjectionStale {
                session_id,
                projection: "session_state",
                checkpoint: Some(db_state.last_event_seq),
                expected: expected_last_sequence,
            });
        }
        let activity_bounds = db.activity_bounds().await?;
        let created_at_ms = activity_bounds
            .map(|(created_at_ms, _)| created_at_ms)
            .or(db_state.updated_at_ms)
            .unwrap_or_else(current_unix_millis);
        let updated_at_ms = db_state
            .updated_at_ms
            .or_else(|| activity_bounds.map(|(_, updated_at_ms)| updated_at_ms))
            .unwrap_or(created_at_ms);
        Ok(SessionState::from_db_state(
            db_state,
            created_at_ms,
            updated_at_ms,
        ))
    }

    async fn session_handle(&self, session_id: SessionId) -> Result<SessionHandle, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        self.inner
            .lock()
            .await
            .sessions
            .get(&session_id)
            .cloned()
            .ok_or(SessionError::NotFound(session_id))
    }

    async fn session_load_gate(&self, session_id: SessionId) -> Arc<Mutex<()>> {
        Arc::clone(
            self.inner
                .lock()
                .await
                .load_gates
                .entry(session_id)
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    async fn ensure_session_loaded(&self, session_id: SessionId) -> Result<(), SessionError> {
        let gate = self.session_load_gate(session_id).await;
        let _guard = gate.lock().await;
        self.ensure_session_loaded_inner(session_id).await
    }

    async fn ensure_session_loaded_inner(&self, session_id: SessionId) -> Result<(), SessionError> {
        let total_timer = self.metrics.timer();
        let cached_handle = self.inner.lock().await.sessions.get(&session_id).cloned();
        if let Some(handle) = cached_handle {
            return self
                .ensure_cached_session_loaded(session_id, handle, total_timer)
                .await;
        }
        let Some(store) = &self.store else {
            record_ensure_loaded_duration(&self.metrics, "missing", total_timer.elapsed_ms());
            return Err(SessionError::NotFound(session_id));
        };
        if db::session_db_path(&store.root_path(), session_id).exists() {
            self.load_persistent_session(session_id, store, total_timer)
                .await?;
            return Ok(());
        }
        record_ensure_loaded_duration(&self.metrics, "missing", total_timer.elapsed_ms());
        Err(SessionError::NotFound(session_id))
    }

    async fn ensure_cached_session_loaded(
        &self,
        session_id: SessionId,
        handle: SessionHandle,
        total_timer: bcode_metrics::MetricsTimer,
    ) -> Result<(), SessionError> {
        let Some(store) = &self.store else {
            record_ensure_loaded_duration(&self.metrics, "cached", total_timer.elapsed_ms());
            return Ok(());
        };
        if !db::session_db_path(&store.root_path(), session_id).exists() {
            record_ensure_loaded_duration(&self.metrics, "cached", total_timer.elapsed_ms());
            return Ok(());
        }
        let snapshot = handle.snapshot();
        let inserted_lease = self
            .acquire_missing_session_lease(session_id, store)
            .await?;
        let refreshed_summary = snapshot.load_status == SessionLoadStatusKind::SummaryOnly;
        if refreshed_summary {
            let result = self
                .refresh_summary_session(session_id, store, &handle)
                .await;
            if result.is_err() && inserted_lease {
                self.inner.lock().await.leases.remove(&session_id);
            }
            result?;
        } else if inserted_lease {
            let readiness = async {
                let db = db::SessionDb::open_existing_turso_in_root(session_id, &store.root_path())
                    .await?;
                db.validate_write_readiness().await
            }
            .await;
            if readiness.is_err() {
                self.inner.lock().await.leases.remove(&session_id);
            }
            readiness?;
        }
        record_ensure_loaded_duration(
            &self.metrics,
            if refreshed_summary {
                "summary_refreshed"
            } else {
                "cached"
            },
            total_timer.elapsed_ms(),
        );
        Ok(())
    }

    async fn acquire_session_lease_for_load(
        &self,
        session_id: SessionId,
        store: &SessionStoreExecutor,
    ) -> Result<SessionLeaseGuard, SessionError> {
        use db::SessionStorageCompatibility::{Current, KnownLegacy};

        let root = store.root_path();
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &root).await?;
        let compatibility = db.storage_compatibility().await?;
        drop(db);
        match compatibility {
            Current { .. } => match self
                .acquire_current_session_lease(session_id, store, &root)
                .await?
            {
                SessionLeaseLoadOutcome::Acquired(lease) => Ok(*lease),
                SessionLeaseLoadOutcome::Retry => {
                    let compatibility =
                        db::SessionDb::open_existing_turso_in_root(session_id, &root)
                            .await?
                            .storage_compatibility()
                            .await?;
                    match compatibility {
                        Current { .. } => Err(db::SessionDbError::MigrationHistoryIncompatible {
                            reason: "session storage changed while acquiring ownership".to_owned(),
                        }
                        .into()),
                        KnownLegacy { writer_epoch } => {
                            Err(SessionError::StorageMigrationRequired {
                                actual: writer_epoch,
                                expected: u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
                            })
                        }
                    }
                }
            },
            KnownLegacy { writer_epoch } => Err(SessionError::StorageMigrationRequired {
                actual: writer_epoch,
                expected: u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
            }),
        }
    }

    async fn acquire_current_session_lease(
        &self,
        session_id: SessionId,
        store: &SessionStoreExecutor,
        root: &Path,
    ) -> Result<SessionLeaseLoadOutcome, SessionError> {
        use db::SessionStorageCompatibility::{Current, KnownLegacy};

        let lease = lease::acquire_session_lease(root, session_id, store.lease_owner())?;
        let rechecked = db::SessionDb::open_existing_turso_in_root(session_id, root)
            .await?
            .storage_compatibility()
            .await?;
        match rechecked {
            Current { .. } => Ok(SessionLeaseLoadOutcome::Acquired(Box::new(lease))),
            KnownLegacy { .. } => {
                drop(lease);
                self.metrics
                    .increment_counter("session.manager.storage_migration.race_retry_total");
                Ok(SessionLeaseLoadOutcome::Retry)
            }
        }
    }

    async fn acquire_missing_session_lease(
        &self,
        session_id: SessionId,
        store: &SessionStoreExecutor,
    ) -> Result<bool, SessionError> {
        if self.inner.lock().await.leases.contains_key(&session_id) {
            return Ok(false);
        }
        let lease = self
            .acquire_session_lease_for_load(session_id, store)
            .await?;
        let mut inner = self.inner.lock().await;
        if let std::collections::btree_map::Entry::Vacant(entry) = inner.leases.entry(session_id) {
            entry.insert(lease);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn refresh_summary_session(
        &self,
        session_id: SessionId,
        store: &SessionStoreExecutor,
        handle: &SessionHandle,
    ) -> Result<(), SessionError> {
        let db_open_timer = self.metrics.timer();
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &store.root_path()).await?;
        db.validate_write_readiness().await?;
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.summary_refresh_db_open_duration_ms",
            db_open_timer.elapsed_ms(),
        );
        let state_load_timer = self.metrics.timer();
        let state = self.load_db_session_state(session_id, &db).await?;
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.summary_refresh_state_load_duration_ms",
            state_load_timer.elapsed_ms(),
        );
        let replace_timer = self.metrics.timer();
        handle.replace_state(state).await?;
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.summary_refresh_replace_state_duration_ms",
            replace_timer.elapsed_ms(),
        );
        Ok(())
    }

    async fn load_persistent_session(
        &self,
        session_id: SessionId,
        store: &SessionStoreExecutor,
        total_timer: bcode_metrics::MetricsTimer,
    ) -> Result<(), SessionError> {
        let load_timer = self.metrics.timer();
        let lease_timer = self.metrics.timer();
        let lease = self
            .acquire_session_lease_for_load(session_id, store)
            .await?;
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.lease_acquire_duration_ms",
            lease_timer.elapsed_ms(),
        );
        let db_open_timer = self.metrics.timer();
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &store.root_path()).await?;
        db.validate_write_readiness().await?;
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.db_open_duration_ms",
            db_open_timer.elapsed_ms(),
        );
        let state_load_timer = self.metrics.timer();
        let state = self.load_db_session_state(session_id, &db).await?;
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.state_load_duration_ms",
            state_load_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.load_db_session_duration_ms",
            load_timer.elapsed_ms(),
        );
        let insert_timer = self.metrics.timer();
        let mut inner = self.inner.lock().await;
        inner
            .sessions
            .insert(session_id, SessionHandle::new(state, Some(store.clone())));
        inner.leases.insert(session_id, lease);
        self.metrics.record_histogram(
            "session.manager.ensure_loaded.insert_handle_duration_ms",
            insert_timer.elapsed_ms(),
        );
        record_ensure_loaded_duration(&self.metrics, "db_loaded", total_timer.elapsed_ms());
        Ok(())
    }

    async fn release_persistent_idle_session_resources(&self, session_id: SessionId) {
        if self.store.is_some() {
            let _ = self.release_idle_session_resources(session_id).await;
        }
    }

    /// Return the current persistent catalog discovery status.
    #[must_use]
    pub fn catalog_status(&self) -> CatalogLoadStatus {
        self.catalog_status_rx.borrow().clone()
    }

    /// Subscribe to persistent catalog status changes.
    #[must_use]
    pub fn subscribe_catalog_status(&self) -> watch::Receiver<CatalogLoadStatus> {
        self.catalog_status_rx.clone()
    }

    /// Start loading the persistent catalog in the background if it has not loaded yet.
    pub fn start_catalog_load(&self) {
        let Some(store) = self.store.clone() else {
            let _ = self.catalog_status_tx.send(CatalogLoadStatus::Loaded);
            return;
        };
        match self.catalog_status() {
            CatalogLoadStatus::Loaded | CatalogLoadStatus::Loading => return,
            CatalogLoadStatus::NotStarted | CatalogLoadStatus::Failed(_) => {}
        }
        let _ = self.catalog_status_tx.send(CatalogLoadStatus::Loading);
        let registry = Arc::clone(&self.inner);
        let status = self.catalog_status_tx.clone();
        tokio::spawn(async move {
            let sessions = match store.load_catalog().await {
                Ok(sessions) => sessions,
                Err(error) => {
                    let _ = status.send(CatalogLoadStatus::Failed(error.to_string()));
                    eprintln!("failed to load session catalog: {error}");
                    return;
                }
            };
            let mut inner = registry.lock().await;
            for (session_id, state) in sessions {
                inner
                    .sessions
                    .entry(session_id)
                    .or_insert_with(|| SessionHandle::new(state, Some(store.clone())));
            }
            drop(inner);
            let _ = status.send(CatalogLoadStatus::Loaded);
        });
    }

    /// Wait until background catalog loading completes.
    ///
    /// # Errors
    ///
    /// Returns an error if catalog loading fails or the catalog status channel closes.
    pub async fn wait_catalog_loaded(&self) -> Result<(), SessionStoreError> {
        self.start_catalog_load();
        let mut status = self.catalog_status_rx.clone();
        loop {
            let value = status.borrow().clone();
            match value {
                CatalogLoadStatus::Loaded => return Ok(()),
                CatalogLoadStatus::Failed(message) => {
                    return Err(SessionStoreError::CatalogLoad(message));
                }
                CatalogLoadStatus::NotStarted | CatalogLoadStatus::Loading => {}
            }
            status.changed().await.map_err(|_| {
                SessionStoreError::CatalogLoad("session catalog status channel closed".to_string())
            })?;
        }
    }

    /// Backfill the current catalog DB from current manifest sidecars and canonical directories.
    ///
    /// This does not open per-session databases or replay event logs.
    ///
    /// # Errors
    ///
    /// Returns an error if catalog backfill fails.
    pub async fn backfill_catalog(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let Some(store) = self.store.clone() else {
            return Ok(Vec::new());
        };
        let summaries = store.backfill_catalog().await?;
        if summaries.is_empty() {
            return Ok(summaries);
        }
        let mut inner = self.inner.lock().await;
        for summary in &summaries {
            inner.sessions.entry(summary.id).or_insert_with(|| {
                SessionHandle::new(
                    SessionState::from_catalog_summary(summary.clone()),
                    Some(store.clone()),
                )
            });
        }
        Ok(summaries)
    }

    /// Return first-class health for one session without event-log replay or repair.
    #[allow(clippy::too_many_lines)]
    pub async fn session_health(&self, session_id: SessionId) -> SessionHealth {
        let Some(store) = &self.store else {
            return if self.inner.lock().await.sessions.contains_key(&session_id) {
                SessionHealth::Ready
            } else {
                SessionHealth::NotFound
            };
        };
        let root = store.root_path();
        let db_path = db::session_db_path(&root, session_id);
        if !db_path.exists() {
            return SessionHealth::NotFound;
        }
        let db = match db::SessionDb::open_existing_turso_in_root(session_id, &root).await {
            Ok(db) => db,
            Err(error) => {
                return SessionHealth::RepairRequired {
                    reason: error.to_string(),
                };
            }
        };
        let expected_writer_epoch = u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH);
        match db.storage_compatibility().await {
            Ok(db::SessionStorageCompatibility::Current { .. }) => {}
            Ok(db::SessionStorageCompatibility::KnownLegacy { writer_epoch }) => {
                let owners = match lease::active_session_owners(&root, session_id) {
                    Ok(owners) => owners,
                    Err(error) => {
                        return SessionHealth::RepairRequired {
                            reason: error.to_string(),
                        };
                    }
                };
                if owners.is_empty() {
                    return SessionHealth::Migratable {
                        source: writer_epoch,
                        target: expected_writer_epoch,
                    };
                }
                return SessionHealth::BlockedOwner {
                    source: writer_epoch,
                    target: expected_writer_epoch,
                    owners,
                };
            }
            Err(db::SessionDbError::WriterIncompatible { actual, expected }) => {
                return SessionHealth::WriterIncompatible { actual, expected };
            }
            Err(error) => {
                return SessionHealth::RepairRequired {
                    reason: error.to_string(),
                };
            }
        }
        let expected = match db.last_event_sequence().await {
            Ok(Some(sequence)) => sequence,
            Ok(None) => 0,
            Err(error) => {
                return SessionHealth::RepairRequired {
                    reason: error.to_string(),
                };
            }
        };
        let session_state = match db.session_state().await {
            Ok(Some(state)) if state.last_event_seq >= expected => state,
            Ok(Some(state)) => {
                return SessionHealth::ProjectionStale {
                    projection: "session_state",
                    checkpoint: Some(state.last_event_seq),
                    expected,
                };
            }
            Ok(None) => {
                return SessionHealth::ProjectionStale {
                    projection: "session_state",
                    checkpoint: None,
                    expected,
                };
            }
            Err(error) => {
                return SessionHealth::RepairRequired {
                    reason: error.to_string(),
                };
            }
        };
        debug_assert!(session_state.last_event_seq >= expected);
        match db
            .materialized_projection_checkpoint(db::MaterializedProjection::ArtifactReferences)
            .await
        {
            Ok(Some(checkpoint)) if checkpoint == expected => SessionHealth::Ready,
            Ok(checkpoint) => SessionHealth::ProjectionStale {
                projection: "artifact_references",
                checkpoint,
                expected,
            },
            Err(error) => SessionHealth::RepairRequired {
                reason: error.to_string(),
            },
        }
    }

    /// Require this session to be ready for a durable turn-admission append.
    ///
    /// # Errors
    ///
    /// Returns a session-specific lease, writer-contract, projection, or database error before
    /// user input is persisted.
    pub async fn require_write_readiness(&self, session_id: SessionId) -> Result<(), SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.validate_write_readiness().await
    }

    /// Create a new session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session-created event cannot be persisted.
    pub async fn create_session(
        &self,
        name: Option<String>,
        working_directory: PathBuf,
    ) -> Result<SessionSummary, SessionError> {
        self.create_session_with_execution(name, working_directory, None)
            .await
    }

    async fn create_session_with_execution(
        &self,
        name: Option<String>,
        working_directory: PathBuf,
        execution: Option<ExecutionSessionProvenance>,
    ) -> Result<SessionSummary, SessionError> {
        validate_execution_session_provenance(execution.as_ref())?;
        if let Some(provenance) = &execution {
            match provenance.context_mode {
                ExecutionSessionContextMode::FreshIsolated => {}
                ExecutionSessionContextMode::FixedGenerationFork => {
                    return self
                        .clone_execution_session_at_generation(
                            provenance.clone(),
                            name,
                            working_directory,
                        )
                        .await;
                }
                ExecutionSessionContextMode::SharedSequential => {
                    return Err(SessionError::InvalidExecutionSessionProvenance(
                        "shared-sequential execution reuses the parent session and must not create a child"
                            .to_string(),
                    ));
                }
            }
        }
        self.create_session_record(name, working_directory, execution)
            .await
    }

    #[allow(clippy::too_many_lines)]
    async fn create_session_record(
        &self,
        name: Option<String>,
        working_directory: PathBuf,
        execution: Option<ExecutionSessionProvenance>,
    ) -> Result<SessionSummary, SessionError> {
        let started_at = std::time::Instant::now();
        self.metrics
            .increment_counter("session.manager.create.total");
        let working_directory = normalize_working_directory(&working_directory);
        let id = SessionId::new();
        let (sender, _) = broadcast::channel(512);
        let live_events = SessionLiveEventBroker::new(512);
        let now_ms = self.next_activity_timestamp_ms();
        let summary = SessionSummary {
            id,
            name: name.clone(),
            explicit_name: name.clone(),
            derived_title: None,
            title_source: if name.is_some() {
                SessionTitleSource::Explicit
            } else {
                SessionTitleSource::EmptyDraft
            },
            client_count: 0,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            working_directory: working_directory.clone(),
            import: None,
            fork: None,
            execution: execution.map(|provenance| {
                Box::new(bcode_session_models::ExecutionSessionSummary {
                    provenance,
                    visibility: SessionVisibility::Background,
                })
            }),
        };
        let state = SessionState {
            summary: summary.clone(),
            working_directory: working_directory.clone(),
            clients: BTreeSet::new(),
            events: Some(Vec::new()),
            next_sequence: 0,
            event_count: 0,
            has_user_message: false,
            current_provider: None,
            current_model: None,
            reasoning_effort: None,
            reasoning_summary: None,
            current_agent: None,
            latest_compaction_sequence: None,
            context_epoch: 0,
            context_occupancy: None,
            turn_receipts: BTreeMap::new(),
            total_metered_tokens: 0,
            load_status: SessionLoadStatusKind::Current,
            sender,
            live_events,
            live_text_checkpoints: BTreeMap::new(),
            live_text_checkpoint_order: Vec::new(),
            live_text_tombstones: BTreeMap::new(),
            live_text_tombstone_order: Vec::new(),
        };
        let lease = self
            .store
            .as_ref()
            .map(|store| lease::acquire_session_lease(&store.root_path(), id, store.lease_owner()))
            .transpose()?;
        let handle = SessionHandle::new(state, self.store.clone());
        let event = handle
            .append_event(
                SessionEventKind::SessionCreated {
                    name,
                    working_directory,
                },
                now_ms,
            )
            .await?;
        let execution_event = if let Some(execution) = summary.execution.clone() {
            Some(
                handle
                    .append_event(
                        SessionEventKind::ExecutionSessionCreated {
                            provenance: Box::new(execution.provenance),
                            visibility: execution.visibility,
                        },
                        now_ms,
                    )
                    .await?,
            )
        } else {
            None
        };
        {
            let mut inner = self.inner.lock().await;
            inner.sessions.insert(id, handle);
            if let Some(lease) = lease {
                inner.leases.insert(id, lease);
            }
        }
        self.release_persistent_idle_session_resources(id).await;
        self.publish_committed_mutation(event, summary.clone());
        if let Some(event) = execution_event {
            self.publish_committed_mutation(event, summary.clone());
        }
        self.metrics
            .record_histogram("session.manager.create.duration_ms", elapsed_ms(started_at));
        Ok(summary)
    }

    async fn clone_execution_session_at_generation(
        &self,
        provenance: ExecutionSessionProvenance,
        name: Option<String>,
        working_directory: PathBuf,
    ) -> Result<SessionSummary, SessionError> {
        let expected = provenance
            .parent_generation
            .expect("validated fixed-generation provenance has a generation");
        let source = self.session_summary(provenance.parent_session_id).await?;
        let expected_working_directory = normalize_working_directory(&working_directory);
        if normalize_working_directory(&source.working_directory) != expected_working_directory {
            return Err(SessionError::InvalidExecutionSessionProvenance(
                "fixed-generation child working directory must match its parent or declared worktree"
                    .to_string(),
            ));
        }
        let events = self.session_history(provenance.parent_session_id).await?;
        let current = events.last().map_or(0, |event| event.sequence);
        if current != expected {
            return Err(SessionError::CloneGenerationChanged {
                session_id: provenance.parent_session_id,
                expected,
                current,
            });
        }
        let marker = SessionEventKind::SessionForked {
            source_session_id: provenance.parent_session_id,
            source_title: Some(source.display_title().to_string()),
            source_cutoff_sequence: events.last().map(|event| event.sequence),
            source_prompt_sequence: None,
            forked_at_ms: self.next_activity_timestamp_ms(),
            kind: SessionForkKind::Clone,
        };
        let session = self
            .copy_session_events_with_execution(
                name,
                working_directory,
                events,
                marker,
                Some(provenance),
            )
            .await?;
        Ok(session)
    }

    /// Admit one shared-session execution under an exclusive per-parent permit.
    ///
    /// The returned permit must remain alive for the full execution. This is the only supported
    /// admission boundary for `shared_sequential` workflow work.
    ///
    /// # Errors
    ///
    /// Returns an error when provenance is invalid or targets a different parent.
    pub async fn admit_shared_execution_session(
        &self,
        parent: SessionId,
        provenance: &ExecutionSessionProvenance,
    ) -> Result<SharedExecutionSessionPermit, SessionError> {
        let session_id = shared_execution_session(parent, provenance)?;
        self.session_summary(session_id).await?;
        let lock = {
            let mut locks = self.shared_execution_locks.lock().await;
            Arc::clone(
                locks
                    .entry(session_id)
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        Ok(SharedExecutionSessionPermit {
            session_id,
            _permit: lock.lock_owned().await,
        })
    }

    /// Create a fresh isolated background execution session.
    ///
    /// The child inherits the parent's normalized working directory. Call
    /// [`Self::create_fresh_execution_session_in_worktree`] for an explicitly declared worktree.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent is unavailable, provenance is invalid, or persistence
    /// fails.
    pub async fn create_fresh_execution_session(
        &self,
        name: Option<String>,
        mut provenance: ExecutionSessionProvenance,
        working_directory: Option<PathBuf>,
    ) -> Result<SessionSummary, SessionError> {
        provenance.context_mode = ExecutionSessionContextMode::FreshIsolated;
        provenance.parent_generation = None;
        let parent = self.session_summary(provenance.parent_session_id).await?;
        let working_directory = match working_directory {
            None => parent.working_directory.clone(),
            Some(working_directory)
                if normalize_working_directory(&working_directory)
                    == normalize_working_directory(&parent.working_directory) =>
            {
                parent.working_directory.clone()
            }
            Some(_) => {
                return Err(SessionError::InvalidExecutionSessionProvenance(
                    "fresh child working directory must inherit its parent; use the declared-worktree API for isolation"
                        .to_string(),
                ));
            }
        };
        self.create_session_with_execution(name, working_directory, Some(provenance))
            .await
    }

    /// Create a fresh isolated execution session in an explicitly declared worktree.
    ///
    /// This low-level boundary accepts only a path already validated by the owning worktree
    /// domain. Product hosts must call `bcode_worktree::validate_registered_worktree` first.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent is unavailable, the worktree directory is not an existing
    /// directory, provenance is invalid, or persistence fails.
    pub async fn create_fresh_execution_session_in_worktree(
        &self,
        name: Option<String>,
        mut provenance: ExecutionSessionProvenance,
        worktree_directory: &Path,
    ) -> Result<SessionSummary, SessionError> {
        provenance.context_mode = ExecutionSessionContextMode::FreshIsolated;
        provenance.parent_generation = None;
        self.session_summary(provenance.parent_session_id).await?;
        self.create_session_with_execution(name, worktree_directory.to_path_buf(), Some(provenance))
            .await
    }

    /// Clone a background execution session from one exact parent generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent generation changed, provenance is invalid, the requested
    /// directory is inconsistent, or persistence fails.
    pub async fn create_fixed_generation_execution_session(
        &self,
        name: Option<String>,
        mut provenance: ExecutionSessionProvenance,
        parent_generation: u64,
        working_directory: Option<PathBuf>,
    ) -> Result<SessionSummary, SessionError> {
        provenance.context_mode = ExecutionSessionContextMode::FixedGenerationFork;
        provenance.parent_generation = Some(parent_generation);
        let parent = self.session_summary(provenance.parent_session_id).await?;
        let working_directory = match working_directory {
            None => parent.working_directory.clone(),
            Some(working_directory)
                if normalize_working_directory(&working_directory)
                    == normalize_working_directory(&parent.working_directory) =>
            {
                parent.working_directory.clone()
            }
            Some(_) => {
                return Err(SessionError::InvalidExecutionSessionProvenance(
                    "fixed-generation child working directory must inherit its parent".to_string(),
                ));
            }
        };
        self.create_session_with_execution(name, working_directory, Some(provenance))
            .await
    }

    /// Clone a fixed-generation execution session into an explicitly declared worktree.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent generation changed, the worktree directory is invalid,
    /// provenance is invalid, or persistence fails.
    pub async fn create_fixed_generation_execution_session_in_worktree(
        &self,
        name: Option<String>,
        mut provenance: ExecutionSessionProvenance,
        parent_generation: u64,
        worktree_directory: &Path,
    ) -> Result<SessionSummary, SessionError> {
        provenance.context_mode = ExecutionSessionContextMode::FixedGenerationFork;
        provenance.parent_generation = Some(parent_generation);
        let working_directory = worktree_directory.to_path_buf();
        let events = self.session_history(provenance.parent_session_id).await?;
        let current = events.last().map_or(0, |event| event.sequence);
        if current != parent_generation {
            return Err(SessionError::CloneGenerationChanged {
                session_id: provenance.parent_session_id,
                expected: parent_generation,
                current,
            });
        }
        let source = self.session_summary(provenance.parent_session_id).await?;
        let marker = SessionEventKind::SessionForked {
            source_session_id: provenance.parent_session_id,
            source_title: Some(source.display_title().to_string()),
            source_cutoff_sequence: events.last().map(|event| event.sequence),
            source_prompt_sequence: None,
            forked_at_ms: self.next_activity_timestamp_ms(),
            kind: SessionForkKind::Clone,
        };
        self.copy_session_events_with_execution(
            name,
            working_directory,
            events,
            marker,
            Some(provenance),
        )
        .await
    }

    /// Set or clear a persisted composer draft for a session.
    ///
    /// Empty text clears the persisted draft without appending a session event.
    ///
    /// # Errors
    ///
    /// Returns an error if the session does not exist or the draft cannot be written.
    pub async fn set_session_composer_draft(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<(), SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle
            .set_composer_draft(text, self.next_activity_timestamp_ms())
            .await
    }

    /// Return a persisted composer draft for a session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session does not exist or the draft cannot be read.
    pub async fn session_composer_draft(
        &self,
        session_id: SessionId,
    ) -> Result<Option<String>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.composer_draft().await
    }

    /// Set or clear a launch-cwd-scoped draft-session composer draft.
    ///
    /// Empty text clears the persisted draft without creating a session.
    ///
    /// # Errors
    ///
    /// Returns an error if the draft cannot be written.
    pub async fn set_draft_session_composer_draft(
        &self,
        launch_working_directory: PathBuf,
        text: String,
    ) -> Result<(), SessionError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let launch_working_directory = normalize_working_directory(&launch_working_directory);
        let db = db::GlobalSessionDb::open_turso_in_root(&store.root_path()).await?;
        db.set_draft_session_composer_draft(
            &launch_working_directory,
            &text,
            self.next_activity_timestamp_ms(),
        )
        .await?;
        Ok(())
    }

    /// Return a launch-cwd-scoped draft-session composer draft.
    ///
    /// # Errors
    ///
    /// Returns an error if the draft cannot be read.
    pub async fn draft_session_composer_draft(
        &self,
        launch_working_directory: PathBuf,
    ) -> Result<Option<String>, SessionError> {
        let Some(store) = &self.store else {
            return Ok(None);
        };
        let launch_working_directory = normalize_working_directory(&launch_working_directory);
        let db = db::GlobalSessionDb::open_turso_in_root(&store.root_path()).await?;
        Ok(db
            .draft_session_composer_draft(&launch_working_directory)
            .await?)
    }

    /// List known sessions from the session catalog.
    pub async fn list_sessions(&self, working_directory: &Path) -> Vec<SessionSummary> {
        self.list_sessions_with_background(working_directory, false)
            .await
    }

    /// List sessions for a working directory, optionally including background execution sessions.
    pub async fn list_sessions_with_background(
        &self,
        working_directory: &Path,
        include_background: bool,
    ) -> Vec<SessionSummary> {
        self.start_catalog_load();
        let sessions = self.cached_sessions(working_directory).await;
        if include_background {
            sessions
        } else {
            sessions
                .into_iter()
                .filter(SessionSummary::is_picker_visible)
                .collect()
        }
    }

    /// List already-loaded sessions without touching persistent storage.
    pub async fn cached_sessions(&self, working_directory: &Path) -> Vec<SessionSummary> {
        let working_directory = normalize_working_directory(working_directory);
        let handles = {
            let inner = self.inner.lock().await;
            inner.sessions.values().cloned().collect::<Vec<_>>()
        };
        sorted_session_summaries(handles, &working_directory)
    }

    /// List all already-loaded sessions, including inspectable background execution sessions.
    pub async fn all_session_summaries(&self) -> Vec<SessionSummary> {
        self.all_session_catalog_entries()
            .await
            .into_iter()
            .map(|entry| entry.summary)
            .collect()
    }

    pub async fn all_session_catalog_entries(&self) -> Vec<SessionCatalogEntry> {
        self.start_catalog_load();
        let handles = {
            let inner = self.inner.lock().await;
            inner.sessions.values().cloned().collect::<Vec<_>>()
        };
        handles
            .into_iter()
            .map(|handle| SessionCatalogEntry::from_snapshot(handle.snapshot()))
            .collect()
    }

    /// Return true once the persistent session catalog has been discovered.
    #[must_use]
    pub fn catalog_loaded(&self) -> bool {
        matches!(self.catalog_status(), CatalogLoadStatus::Loaded)
    }

    /// Rename a session.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the rename event cannot be persisted
    pub async fn rename_session(
        &self,
        session_id: SessionId,
        name: Option<String>,
    ) -> Result<SessionEvent, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let normalized_name = normalize_session_name(name);
        let handle = self.session_handle(session_id).await?;
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        let event = handle
            .append_event(
                SessionEventKind::SessionRenamed {
                    name: normalized_name,
                },
                activity_timestamp_ms,
            )
            .await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        Ok(event)
    }

    /// Change a session's canonical working directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn change_session_working_directory(
        &self,
        session_id: SessionId,
        new_working_directory: PathBuf,
    ) -> Result<Option<SessionEvent>, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let handle = self.session_handle(session_id).await?;
        let old_working_directory = handle.working_directory().await?;
        let new_working_directory = normalize_working_directory(&new_working_directory);
        if old_working_directory == new_working_directory {
            return Ok(None);
        }
        let activity_timestamp_ms = self.next_activity_timestamp_ms();
        let event = handle
            .append_event(
                SessionEventKind::WorkingDirectoryChanged {
                    old_working_directory,
                    new_working_directory,
                },
                activity_timestamp_ms,
            )
            .await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        Ok(Some(event))
    }

    /// Import a fully normalized external session as a native Bcode session.
    ///
    /// # Errors
    ///
    /// Returns an error if session creation or event persistence fails.
    pub async fn import_session(
        &self,
        name: Option<String>,
        working_directory: PathBuf,
        import: SessionImportSummary,
        events: Vec<(SessionEventKind, Option<SessionEventProvenance>)>,
    ) -> Result<SessionSummary, SessionError> {
        let session = self.create_session(name, working_directory).await?;
        self.append_event(
            session.id,
            SessionEventKind::SessionImported {
                source_id: import.source_id,
                source_display_name: import.source_display_name,
                external_session_id: import.external_session_id,
                imported_at_ms: import.imported_at_ms,
            },
        )
        .await?;
        for (event, provenance) in events {
            self.append_event_with_provenance(session.id, event, provenance)
                .await?;
        }
        self.session_summary(session.id).await
    }

    /// Delete a session.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the session has connected clients
    /// * the persistent session data cannot be removed
    pub async fn delete_session(
        &self,
        session_id: SessionId,
    ) -> Result<SessionSummary, SessionError> {
        let handle = self.session_handle(session_id).await?;
        let session = handle.summary().await?;
        if handle.client_count() != 0 {
            return Err(SessionError::ConnectedClients(session_id));
        }
        let _lease = {
            let mut inner = self.inner.lock().await;
            inner
                .sessions
                .remove(&session_id)
                .ok_or(SessionError::NotFound(session_id))?;
            let lease = inner.leases.remove(&session_id);
            inner.load_gates.remove(&session_id);
            lease
        };
        if let Some(store) = &self.store {
            let catalog = db::GlobalSessionDb::open_turso_in_root(&store.root_path()).await;
            if let Ok(catalog) = catalog
                && let Err(error) = catalog.delete_session(session_id).await
            {
                eprintln!("failed to remove session from canonical catalog: {error}");
            }
            let session_dir = db::session_dir_path(&store.root_path(), session_id);
            if session_dir.exists() {
                match std::fs::remove_dir_all(&session_dir) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(SessionStoreError::Io(error).into()),
                }
            }
        }
        handle.shutdown().await?;
        Ok(session)
    }

    /// Return a summary for one session.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn session_summary(
        &self,
        session_id: SessionId,
    ) -> Result<SessionSummary, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.summary().await
    }

    /// Return the durable working directory associated with a session.
    ///
    /// This is the canonical cwd for all session-scoped server runtime,
    /// including prompts, policy checks, and tool execution.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn session_working_directory(
        &self,
        session_id: SessionId,
    ) -> Result<PathBuf, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.working_directory().await
    }

    /// Return the complete durable event history for explicit export/debug/history commands only.
    ///
    /// This API performs a full canonical event read. Do not call it from normal UI, attach,
    /// prompt/model-context, catalog, or background maintenance paths. Use bounded pages,
    /// projection windows, or typed read models for runtime flows.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn session_history(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.history().await
    }

    /// Return a bounded page of replayable history for a session.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn session_history_page(
        &self,
        session_id: SessionId,
        query: SessionHistoryQuery,
    ) -> Result<SessionHistoryPage, SessionError> {
        let Some(store) = &self.store else {
            return Err(SessionError::NotFound(session_id));
        };
        let db_path = db::session_db_path(&store.root_path(), session_id);
        if !db_path.exists() {
            return Err(SessionError::NotFound(session_id));
        }
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &store.root_path()).await?;
        Ok(db.history_page(query).await?)
    }

    /// Return canonical plugin status-note events for one stable note identity.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn plugin_status_note_events(
        &self,
        session_id: SessionId,
        plugin_id: &str,
        note_id: &str,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let Some(store) = &self.store else {
            return Err(SessionError::NotFound(session_id));
        };
        let db_path = db::session_db_path(&store.root_path(), session_id);
        if !db_path.exists() {
            return Err(SessionError::NotFound(session_id));
        }
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &store.root_path()).await?;
        Ok(db.plugin_status_note_events(plugin_id, note_id).await?)
    }

    /// Return a semantic projection window for a session.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    /// Returns [`SessionError::UnsupportedProjectionWindow`] when the request shape is not supported
    /// by the first-pass projection implementation.
    pub async fn session_projection_window(
        &self,
        session_id: SessionId,
        request: ProjectionWindowRequest,
    ) -> Result<ProjectionWindow, SessionError> {
        let handle = self.session_handle(session_id).await?;
        let projection_window = match handle.projection_window(request.clone()).await {
            Ok(window) => {
                self.metrics
                    .increment_counter("session.manager.projection_window.fast_path_total");
                Ok(window)
            }
            Err(SessionError::UnsupportedProjectionWindow) => {
                self.metrics
                    .increment_counter("session.manager.projection_window.fallback_total");
                self.projection_window_from_recent_history(session_id, request)
                    .await
            }
            Err(error) => Err(error),
        }?;
        Ok(projection_window)
    }

    async fn projection_window_from_recent_history(
        &self,
        session_id: SessionId,
        request: ProjectionWindowRequest,
    ) -> Result<ProjectionWindow, SessionError> {
        let limit = request.limits.max_events_scanned.max(1);
        let page = self
            .session_history_page(
                session_id,
                SessionHistoryQuery {
                    cursor: None,
                    limit,
                    direction: SessionHistoryDirection::Backward,
                },
            )
            .await?;
        crate::projection::projection_window_from_events(&page.events, &request)
            .ok_or(SessionError::UnsupportedProjectionWindow)
    }

    /// Return source events in an inclusive sequence range.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn session_events_range(
        &self,
        session_id: SessionId,
        start_sequence: u64,
        end_sequence: u64,
        max_events: usize,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle
            .events_range(start_sequence, end_sequence, max_events)
            .await
    }

    /// Return user-submitted prompts for input-history navigation.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn session_input_history(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionInputHistoryEntry>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.input_history().await
    }

    /// Return the latest session-specific runtime selection state.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn current_runtime_selection(
        &self,
        session_id: SessionId,
    ) -> Result<SessionRuntimeSelection, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.current_runtime_selection().await
    }

    /// Return the latest session-specific model selection if one has been set.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn current_model_selection(
        &self,
        session_id: SessionId,
    ) -> Result<(Option<String>, Option<String>), SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.current_model_selection().await
    }

    /// Return the latest session-specific reasoning selection if one has been set.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn current_reasoning_selection(
        &self,
        session_id: SessionId,
    ) -> Result<(Option<String>, Option<String>), SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.current_reasoning_selection().await
    }

    /// Return the latest session-specific agent selection if one has been set.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn current_agent_selection(
        &self,
        session_id: SessionId,
    ) -> Result<Option<String>, SessionError> {
        let handle = self.session_handle(session_id).await?;
        handle.current_agent_selection().await
    }

    /// Release cached per-session resources when no clients remain attached.
    ///
    /// The session stays visible through its lightweight summary, and its compatibility lease
    /// remains held for the loaded actor lifetime. Only cached database/event state is released;
    /// this prevents an incompatible daemon from claiming the session between idle operations.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session actor is unavailable.
    pub async fn release_idle_session_resources(
        &self,
        session_id: SessionId,
    ) -> Result<bool, SessionError> {
        let started_at = Instant::now();
        let handle = self
            .inner
            .lock()
            .await
            .sessions
            .get(&session_id)
            .cloned()
            .ok_or(SessionError::NotFound(session_id))?;
        let released = handle.release_idle_resources().await?;
        self.metrics.record_histogram(
            "session.manager.release_idle.duration_ms",
            elapsed_ms(started_at),
        );
        if released {
            self.metrics
                .increment_counter("session.manager.release_idle.released_total");
        }
        Ok(released)
    }

    /// Release the cached database handle without detaching clients.
    ///
    /// The actor remains attached and reopens the database lazily on its next durable operation.
    /// Callers must ensure no model turn, runtime work, migration, or queued write is active.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session actor is unavailable.
    pub async fn release_session_database_resources(
        &self,
        session_id: SessionId,
    ) -> Result<bool, SessionError> {
        let handle = self
            .inner
            .lock()
            .await
            .sessions
            .get(&session_id)
            .cloned()
            .ok_or(SessionError::NotFound(session_id))?;
        handle.release_database_resources().await
    }
}

fn sorted_session_summaries(
    handles: Vec<SessionHandle>,
    working_directory: &Path,
) -> Vec<SessionSummary> {
    let mut sessions = handles
        .into_iter()
        .map(|handle| handle.snapshot())
        .filter(|snapshot| {
            normalize_working_directory(&snapshot.working_directory) == working_directory
        })
        .map(|snapshot| snapshot.summary)
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
            .then_with(|| left.id.cmp(&right.id))
    });
    sessions
}

fn input_history_from_events(history: &[SessionEvent]) -> Vec<SessionInputHistoryEntry> {
    history
        .iter()
        .filter_map(|event| {
            if let SessionEventKind::UserMessage { text, .. } = &event.kind {
                Some(SessionInputHistoryEntry {
                    sequence: event.sequence,
                    timestamp_ms: event.timestamp_ms,
                    text: text.clone(),
                })
            } else {
                None
            }
        })
        .collect()
}

fn model_context_events_from_history(history: &[SessionEvent]) -> Vec<SessionEvent> {
    let latest_compaction = history
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                SessionEventKind::ContextCompacted { .. }
                    | SessionEventKind::ProviderContextCompacted { .. }
            )
        })
        .max_by_key(|event| event.sequence);
    let Some(marker) = latest_compaction else {
        return history.to_vec();
    };
    let compacted_through_sequence = match &marker.kind {
        SessionEventKind::ContextCompacted {
            compacted_through_sequence,
            ..
        }
        | SessionEventKind::ProviderContextCompacted {
            compacted_through_sequence,
            ..
        } => *compacted_through_sequence,
        _ => unreachable!("marker selection accepts only compaction events"),
    };
    let mut retained = history
        .iter()
        .filter(|event| event.sequence > compacted_through_sequence)
        .filter(|event| event.sequence != marker.sequence)
        .filter(|event| {
            !matches!(
                event.kind,
                SessionEventKind::ContextCompacted { .. }
                    | SessionEventKind::ProviderContextCompacted { .. }
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    retained.sort_by_key(|event| event.sequence);
    std::iter::once(marker.clone()).chain(retained).collect()
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn canonical_session_id_from_dir(path: &Path) -> Option<SessionId> {
    path.is_dir()
        .then(|| path.file_name()?.to_str()?.parse::<SessionId>().ok())
        .flatten()
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn normalize_session_name(name: Option<String>) -> Option<String> {
    name.map(|value| squish_whitespace(&value))
        .filter(|value| !value.is_empty())
}

fn normalize_working_directory(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn title_from_first_prompt(prompt: &str) -> String {
    let first_content_line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("```") && !line.starts_with("---"))
        .unwrap_or(prompt);
    let cleaned = first_content_line
        .trim_start_matches(|character: char| {
            matches!(character, '#' | '-' | '*' | '>' | '`' | ':' | ';')
                || character.is_whitespace()
        })
        .trim();
    let squished = squish_whitespace(cleaned);
    if squished.is_empty() {
        return "New session".to_string();
    }
    truncate_title(&squished, 64)
}

fn squish_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_title(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AppendToolCallRequestedInput, CURRENT_SESSION_FORMAT_EPOCH, SESSION_FORMAT_FAMILY,
        SESSION_MANIFEST_SCHEMA_VERSION, SessionCatalogLoadStatus, SessionError, SessionHealth,
        SessionLeaseOwnerContext, SessionLoadStatusKind, SessionManager, SessionMigrationStage,
        SessionOpenTerminalOutcome, SessionStore, db, lease, persisted, shared_execution_session,
    };
    use bcode_metrics::MetricsRegistry;
    use bcode_session_models::{
        ExecutionSessionContextMode, ExecutionSessionProvenance, SessionVisibility,
    };
    use std::time::Duration;
    use switchy::database::query::FilterableQuery;

    fn session_database_files(
        root: &std::path::Path,
        session_id: SessionId,
    ) -> Vec<(String, Vec<u8>)> {
        let path = db::session_db_path(root, session_id);
        let file_name = path
            .file_name()
            .expect("database filename")
            .to_string_lossy();
        let mut files = std::fs::read_dir(path.parent().expect("database parent"))
            .expect("database directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(file_name.as_ref())
            })
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    std::fs::read(entry.path()).expect("database bytes"),
                )
            })
            .collect::<Vec<_>>();
        files.sort_by(|left, right| left.0.cmp(&right.0));
        files
    }

    #[derive(Clone, Copy)]
    enum MigrationBenchmarkProfile {
        Small,
        Medium,
        Large,
    }

    impl MigrationBenchmarkProfile {
        const fn event_count(self) -> usize {
            match self {
                Self::Small => 100,
                Self::Medium => 5_000,
                Self::Large => 50_000,
            }
        }

        const fn name(self) -> &'static str {
            match self {
                Self::Small => "small",
                Self::Medium => "medium",
                Self::Large => "large-50k",
            }
        }
    }

    async fn generate_legacy_migration_benchmark_store(
        root: &std::path::Path,
        profile: MigrationBenchmarkProfile,
    ) -> SessionId {
        generate_migration_benchmark_store(root, profile, 3).await
    }

    async fn generate_current_migration_benchmark_store(
        root: &std::path::Path,
        profile: MigrationBenchmarkProfile,
    ) -> SessionId {
        let session_id = generate_migration_benchmark_store(root, profile, 3).await;
        let maintenance = lease::acquire_session_maintenance_guard(root, session_id)
            .expect("benchmark maintenance guard");
        let write = lease::acquire_maintenance_session_write_lock(&maintenance, root, session_id)
            .expect("benchmark write guard");
        db::SessionDb::migrate_turso_in_root(session_id, root, &maintenance, &write)
            .await
            .expect("benchmark current migration");
        drop(write);
        drop(maintenance);
        session_id
    }

    async fn generate_migration_benchmark_store(
        root: &std::path::Path,
        profile: MigrationBenchmarkProfile,
        writer_epoch: u32,
    ) -> SessionId {
        let session_id = SessionId::new();
        let db = db::SessionDb::open_turso_in_root(session_id, root)
            .await
            .expect("benchmark DB");
        let tx = db
            .database()
            .begin_transaction()
            .await
            .expect("benchmark transaction");
        for sequence in 0..profile.event_count() {
            let sequence = u64::try_from(sequence).expect("benchmark sequence fits");
            let kind = if sequence == 0 {
                SessionEventKind::SessionCreated {
                    name: Some(format!("migration benchmark {}", profile.name())),
                    working_directory: test_working_directory(),
                }
            } else if sequence % 2 == 0 {
                SessionEventKind::AssistantMessage {
                    text: format!("synthetic assistant message {sequence}"),
                }
            } else {
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: format!("synthetic user message {sequence}"),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                }
            };
            let event = SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence,
                timestamp_ms: sequence,
                session_id,
                provenance: None,
                kind,
            };
            let payload = persisted::encode_session_event(&event).expect("benchmark event encode");
            let event_type = match event.kind {
                SessionEventKind::SessionCreated { .. } => "session_created",
                SessionEventKind::UserMessage { .. } => "user_message",
                SessionEventKind::AssistantMessage { .. } => "assistant_message",
                _ => unreachable!("benchmark generator uses three event kinds"),
            };
            tx.insert("events")
                .value(
                    "event_seq",
                    switchy::database::DatabaseValue::Int64(
                        i64::try_from(sequence).expect("benchmark sequence fits i64"),
                    ),
                )
                .value("event_type", event_type)
                .value(
                    "schema_version",
                    switchy::database::DatabaseValue::Int32(i32::from(
                        CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    )),
                )
                .value(
                    "created_at_ms",
                    switchy::database::DatabaseValue::Int64(
                        i64::try_from(sequence).expect("benchmark timestamp fits i64"),
                    ),
                )
                .value("payload", payload)
                .execute(&*tx)
                .await
                .expect("benchmark canonical insert");
        }
        tx.update("session_storage_contract")
            .value(
                "writer_epoch",
                switchy::database::DatabaseValue::Int64(i64::from(writer_epoch)),
            )
            .where_eq("contract_id", switchy::database::DatabaseValue::Int32(1))
            .execute(&*tx)
            .await
            .expect("legacy writer epoch");
        tx.commit().await.expect("benchmark transaction commit");
        drop(db);
        session_id
    }

    const MIGRATION_PROGRESS_OVERHEAD_PERCENT_BUDGET: u128 = 10;
    const MIGRATION_PROGRESS_OVERHEAD_FIXED_BUDGET_MS: u128 = 25;
    const CURRENT_SESSION_PREPARE_P95_BUDGET_MS: u128 = 25;

    #[tokio::test]
    async fn session_health_is_byte_for_byte_non_mutating() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("health immutability".to_string()),
                test_working_directory(),
            )
            .await
            .expect("session should create");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let before = session_database_files(&root, session.id);

        assert_eq!(
            manager.session_health(session.id).await,
            SessionHealth::Ready
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        let after = session_database_files(&root, session.id);
        assert_eq!(
            after, before,
            "session health must not mutate DB or sidecars"
        );
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn session_health_reports_incompatible_storage_writer_epoch() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("writer health".to_string()), test_working_directory())
            .await
            .expect("session should create");
        let db = db::SessionDb::open_turso_in_root(session.id, &root)
            .await
            .expect("open session db");
        let future_epoch = u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH).saturating_add(1);
        db.database()
            .update("session_storage_contract")
            .value(
                "writer_epoch",
                switchy::database::DatabaseValue::Int64(
                    i64::try_from(future_epoch).expect("epoch fits"),
                ),
            )
            .execute(db.database())
            .await
            .expect("set future writer epoch");

        assert_eq!(
            manager.session_health(session.id).await,
            SessionHealth::WriterIncompatible {
                actual: Some(future_epoch),
                expected: u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
            }
        );
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn bounded_history_does_not_require_runtime_lease_or_writer_compatibility() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("read-only incompatible history".to_owned()),
                test_working_directory(),
            )
            .await
            .expect("session should create");
        let db = db::SessionDb::open_turso_in_root(session.id, &root)
            .await
            .expect("open session db");
        let future_epoch = u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH).saturating_add(1);
        db.database()
            .update("session_storage_contract")
            .value(
                "writer_epoch",
                switchy::database::DatabaseValue::Int64(
                    i64::try_from(future_epoch).expect("epoch fits"),
                ),
            )
            .execute(db.database())
            .await
            .expect("set future writer epoch");
        manager
            .inner
            .lock()
            .await
            .sessions
            .remove(&session.id)
            .expect("remove cached actor handle");
        manager.inner.lock().await.leases.remove(&session.id);

        let page = manager
            .session_history_page(
                session.id,
                SessionHistoryQuery {
                    cursor: None,
                    direction: bcode_session_models::SessionHistoryDirection::Forward,
                    limit: 10,
                },
            )
            .await
            .expect("bounded history should remain inspectable");
        assert_eq!(page.events.len(), 1);
        assert!(matches!(
            page.events[0].kind,
            SessionEventKind::SessionCreated { .. }
        ));
        assert!(matches!(
            manager.ensure_session_loaded(session.id).await,
            Err(SessionError::Db(
                db::SessionDbError::WriterIncompatible { .. }
            ))
        ));
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn session_health_composes_migratable_and_owner_blocked_historical_state() {
        let root = unique_temp_dir();
        let session_id =
            generate_legacy_migration_benchmark_store(&root, MigrationBenchmarkProfile::Small)
                .await;
        let manager = SessionManager::persistent(&root).expect("manager");

        assert_eq!(
            manager.session_health(session_id).await,
            SessionHealth::Migratable {
                source: 3,
                target: u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
            }
        );
        let owner = lease::acquire_session_lease(
            &root,
            session_id,
            &lease::SessionLeaseOwnerContext {
                daemon_namespace: Some("older-daemon".to_owned()),
                build_fingerprint: Some("older-build".to_owned()),
                storage_writer_epoch: Some(3),
                daemon_instance_id: Some("older-instance".to_owned()),
                endpoint: Some("older.sock".to_owned()),
                ..lease::SessionLeaseOwnerContext::default()
            },
        )
        .expect("older owner");
        let health = manager.session_health(session_id).await;
        let SessionHealth::BlockedOwner {
            source,
            target,
            owners,
        } = health
        else {
            panic!("expected blocked-owner health");
        };
        assert_eq!(source, 3);
        assert_eq!(target, u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH));
        assert_eq!(owners, vec![owner.owner().clone()]);
        drop(owner);
        std::fs::remove_dir_all(root).expect("temp dir cleanup");
    }

    #[tokio::test]
    async fn session_health_reports_missing_artifact_projection_as_stale() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should create");
        let session = manager
            .create_session(Some("health".to_owned()), test_working_directory())
            .await
            .expect("session should create");
        assert_eq!(
            manager.session_health(session.id).await,
            SessionHealth::Ready
        );

        let db = db::SessionDb::open_turso_in_root(session.id, &root)
            .await
            .expect("session DB should open");
        db.database()
            .delete("projection_checkpoints")
            .where_eq("projection_name", "artifact_references")
            .execute(db.database())
            .await
            .expect("remove checkpoint");

        assert_eq!(
            manager.session_health(session.id).await,
            SessionHealth::ProjectionStale {
                projection: "artifact_references",
                checkpoint: None,
                expected: 0,
            }
        );
    }

    async fn persistent_artifact_session_bytes(
        root: &std::path::Path,
        artifact_bytes: u64,
        transient_updates: usize,
    ) -> u64 {
        let manager = SessionManager::persistent(root).expect("manager should create");
        let session = manager
            .create_session(Some("artifact-size".to_owned()), test_working_directory())
            .await
            .expect("session should create");
        let _attachment = manager
            .attach_session(session.id, ClientId::new())
            .await
            .expect("session should attach");
        manager
            .append_tool_call_requested(
                session.id,
                AppendToolCallRequestedInput {
                    tool_call_id: "call-1".to_owned(),
                    producer_plugin_id: Some("fixture.plugin".to_owned()),
                    tool_name: "fixture.run".to_owned(),
                    arguments_json: "{}".to_owned(),
                    working_directory: None,
                },
            )
            .await
            .expect("request should append");
        for sequence in 0..transient_updates {
            manager
                .publish_live_event(
                    session.id,
                    SessionLiveEventKind::ToolContributionPlaced {
                        envelope: bcode_session_models::ToolContributionEnvelope::new(
                            bcode_session_models::ToolContributionPlacement::Hidden,
                            bcode_session_models::ToolContributionEvent {
                                invocation_id: "call-1".to_owned(),
                                contribution_id: "transient-volume".to_owned(),
                                sequence: u64::try_from(sequence).expect("sequence"),
                                producer_id: "fixture.plugin".to_owned(),
                                schema: "fixture.transient-volume".to_owned(),
                                schema_version: 1,
                                operation: bcode_session_models::ToolContributionOperation::Upsert,
                                persistence:
                                    bcode_session_models::ToolContributionPersistence::Transient,
                                artifact: None,
                                payload: serde_json::json!({"chunk": "x".repeat(4_096)}),
                            },
                        ),
                    },
                )
                .await
                .expect("transient contribution should publish");
        }
        manager
            .append_event(
                session.id,
                SessionEventKind::ToolInvocationResultRecorded {
                    record: bcode_session_models::ToolInvocationResultRecord {
                        invocation_id: "call-1".to_owned(),
                        model_output: "bounded result".to_owned(),
                        is_error: false,
                        presentation: None,
                        result: Some(ToolInvocationResult::Artifact {
                            artifact: Box::new(bcode_session_models::ToolArtifact {
                                artifact_id: "artifact-1".to_owned(),
                                producer_plugin_id: "fixture.plugin".to_owned(),
                                schema: "fixture.artifact".to_owned(),
                                schema_version: 1,
                                tool_call_id: Some("call-1".to_owned()),
                                title: None,
                                metadata: serde_json::Value::Null,
                                refs: vec![bcode_session_models::ToolArtifactRef {
                                    key: "complete_output".to_owned(),
                                    content_type: Some("application/octet-stream".to_owned()),
                                    storage_uri: Some("file:///external/artifact".to_owned()),
                                    byte_len: Some(artifact_bytes),
                                    metadata: None,
                                }],
                            }),
                        }),
                    },
                },
            )
            .await
            .expect("completion should append");
        drop(manager);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let path = db::session_db_path(root, session.id);
        let file_name = path.file_name().expect("database filename");
        std::fs::read_dir(path.parent().expect("database parent"))
            .expect("database directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(file_name.to_string_lossy().as_ref())
            })
            .filter_map(|entry| entry.metadata().ok())
            .map(|metadata| metadata.len())
            .sum()
    }

    #[tokio::test]
    async fn session_database_growth_is_independent_of_artifact_volume_and_transient_updates() {
        let low_root = unique_temp_dir();
        let high_root = unique_temp_dir();
        let low = persistent_artifact_session_bytes(&low_root, 100_000, 1).await;
        let high = persistent_artifact_session_bytes(&high_root, 900_000, 1_000).await;

        assert_eq!(low, high, "low={low} high={high}");
    }

    #[test]
    fn domain_metrics_count_payload_semantics_artifacts_and_compaction_boundaries() {
        let metrics = MetricsRegistry::in_memory();
        let session_id = SessionId::new();
        let artifact = bcode_session_models::ToolArtifact {
            artifact_id: "artifact".to_owned(),
            producer_plugin_id: "plugin".to_owned(),
            schema: "schema".to_owned(),
            schema_version: 1,
            tool_call_id: Some("call".to_owned()),
            title: None,
            metadata: serde_json::Value::Null,
            refs: vec![bcode_session_models::ToolArtifactRef {
                key: "recording".to_owned(),
                content_type: None,
                storage_uri: Some("artifact://recording".to_owned()),
                byte_len: Some(12),
                metadata: None,
            }],
        };
        let events = [
            SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 1,
                timestamp_ms: 1,
                session_id,
                provenance: None,
                kind: SessionEventKind::ToolInvocationResultRecorded {
                    record: bcode_session_models::ToolInvocationResultRecord {
                        invocation_id: "call".to_owned(),
                        model_output: "done".to_owned(),
                        is_error: false,
                        presentation: None,
                        result: Some(ToolInvocationResult::Artifact {
                            artifact: Box::new(artifact),
                        }),
                    },
                },
            },
            SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 2,
                timestamp_ms: 2,
                session_id,
                provenance: None,
                kind: SessionEventKind::ContextCompacted {
                    summary: "summary".to_owned(),
                    compacted_through_sequence: 1,
                },
            },
        ];
        for event in &events {
            super::record_session_event_domain_metrics(&metrics, event);
        }

        let snapshot = metrics.snapshot();
        assert_eq!(
            snapshot.counters.get("session.event.semantic_rows"),
            Some(&2)
        );
        assert_eq!(
            snapshot.counters.get("session.event.artifact_references"),
            Some(&1)
        );
        assert_eq!(
            snapshot.counters.get("session.event.compaction_boundaries"),
            Some(&1)
        );
        assert_eq!(
            snapshot
                .histograms
                .get("session.event.payload_bytes")
                .map(|histogram| histogram.count),
            Some(2)
        );
    }

    use bcode_session_models::{
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, ProjectionWindowAnchor,
        ProjectionWindowDirection, ProjectionWindowLimits, ProjectionWindowRequest,
        ProjectionWindowTarget, ProviderContextSnapshot, ProviderContextSnapshotOrigin,
        ProviderStreamEvent, RuntimeWorkKind, RuntimeWorkStatus, SessionEvent, SessionEventKind,
        SessionEventProvenance, SessionForkKind, SessionHistoryQuery, SessionId, SessionLiveEvent,
        SessionLiveEventKind, SessionProjectionKind, SessionTraceEvent, SessionTracePayload,
        SessionTracePhase, ToolInvocationResult, TraceBlobRef, WorkId,
    };
    use bcode_skill_models::{SkillActivationMode, SkillId};
    use serde::Serialize;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn test_session_event(
        session_id: SessionId,
        sequence: u64,
        kind: SessionEventKind,
    ) -> SessionEvent {
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind,
        }
    }

    fn provider_snapshot() -> ProviderContextSnapshot {
        ProviderContextSnapshot {
            format_version: 1,
            request_fingerprint: None,
            request_id: None,
            provider_plugin_id: "provider".to_string(),
            model_id: "model".to_string(),
            compatibility_key: "surface".to_string(),
            auth_profile: None,
            origin: ProviderContextSnapshotOrigin::Explicit,
            messages_json: "[]".to_string(),
            portable_summary: "portable".to_string(),
        }
    }

    #[test]
    fn in_memory_projection_selects_newest_marker_by_sequence_not_storage_order() {
        let id = SessionId::new();
        let history = vec![
            test_session_event(
                id,
                8,
                SessionEventKind::ContextCompacted {
                    summary: "newest".to_string(),
                    compacted_through_sequence: 2,
                },
            ),
            test_session_event(
                id,
                4,
                SessionEventKind::ProviderContextCompacted {
                    snapshot: provider_snapshot(),
                    compacted_through_sequence: 1,
                },
            ),
            test_session_event(
                id,
                5,
                SessionEventKind::AssistantMessage {
                    text: "retained".to_string(),
                },
            ),
        ];
        let projected = super::model_context_events_from_history(&history);
        assert_eq!(
            projected
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![8, 5]
        );
    }

    #[test]
    fn copied_local_boundary_is_rewritten_to_destination_sequence() {
        let rewritten = super::fork::rewrite_copied_event_kind(
            SessionEventKind::ContextCompacted {
                summary: "summary".to_string(),
                compacted_through_sequence: 10,
            },
            &BTreeMap::from([(10, 4)]),
        );
        assert!(matches!(
            rewritten,
            SessionEventKind::ContextCompacted {
                compacted_through_sequence: 4,
                ..
            }
        ));
    }

    #[test]
    fn copied_provider_boundary_is_rewritten_to_destination_sequence() {
        let rewritten = super::fork::rewrite_copied_event_kind(
            SessionEventKind::ProviderContextCompacted {
                snapshot: provider_snapshot(),
                compacted_through_sequence: 10,
            },
            &BTreeMap::from([(10, 4)]),
        );
        assert!(matches!(
            rewritten,
            SessionEventKind::ProviderContextCompacted {
                compacted_through_sequence: 4,
                ..
            }
        ));
    }

    #[test]
    fn fork_cut_before_boundary_contains_no_future_marker() {
        let id = SessionId::new();
        let history = vec![
            test_session_event(
                id,
                1,
                SessionEventKind::AssistantMessage {
                    text: "old".to_string(),
                },
            ),
            test_session_event(
                id,
                3,
                SessionEventKind::ContextCompacted {
                    summary: "summary".to_string(),
                    compacted_through_sequence: 1,
                },
            ),
        ];
        let forked = history
            .into_iter()
            .filter(|event| event.sequence < 2)
            .collect::<Vec<_>>();
        assert!(
            !super::model_context_events_from_history(&forked)
                .iter()
                .any(|event| matches!(
                    event.kind,
                    SessionEventKind::ContextCompacted { .. }
                        | SessionEventKind::ProviderContextCompacted { .. }
                ))
        );
    }

    #[test]
    fn fork_cut_after_boundary_preserves_marker_and_retained_tail() {
        let id = SessionId::new();
        let history = vec![
            test_session_event(
                id,
                1,
                SessionEventKind::AssistantMessage {
                    text: "old".to_string(),
                },
            ),
            test_session_event(
                id,
                3,
                SessionEventKind::ContextCompacted {
                    summary: "summary".to_string(),
                    compacted_through_sequence: 1,
                },
            ),
            test_session_event(
                id,
                4,
                SessionEventKind::AssistantMessage {
                    text: "tail".to_string(),
                },
            ),
        ];
        let forked = history
            .into_iter()
            .filter(|event| event.sequence < 5)
            .collect::<Vec<_>>();
        assert_eq!(
            super::model_context_events_from_history(&forked)
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
    }

    #[tokio::test]
    async fn ordered_live_text_operations_never_enter_history_and_restart_drops_state() {
        let root = unique_temp_dir();
        let session_id = {
            let manager = SessionManager::persistent(&root).expect("manager should create");
            let session = manager
                .create_session(Some("live streams".to_owned()), test_working_directory())
                .await
                .expect("session should create");
            let durable_before = manager
                .session_history(session.id)
                .await
                .expect("history before live updates");
            let assistant =
                |revision, operation| SessionLiveEventKind::AssistantTextStreamUpdated {
                    turn_id: "turn-1".to_owned(),
                    segment_id: "segment-0".to_owned(),
                    segment_order: 0,
                    update: bcode_session_models::TextStreamUpdate {
                        generation: 0,
                        first_revision: revision,
                        revision,
                        operation,
                    },
                };
            for event in [
                assistant(
                    1,
                    bcode_session_models::TextStreamOperation::Append {
                        expected_offset: 0,
                        text: "partial".to_owned(),
                    },
                ),
                assistant(
                    2,
                    bcode_session_models::TextStreamOperation::Checkpoint {
                        start_offset: 0,
                        text: "partial".to_owned(),
                        total_bytes: 7,
                        truncated: false,
                    },
                ),
                SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                    turn_id: "turn-1".to_owned(),
                    activity_id: "activity-0".to_owned(),
                    activity_order: 0,
                    part_id: "part-0".to_owned(),
                    kind: bcode_session_models::ReasoningContentKind::Summary,
                    role: bcode_session_models::ReasoningContentRole::Milestone,
                    part_order: 0,
                    update: bcode_session_models::TextStreamUpdate {
                        generation: 0,
                        first_revision: 1,
                        revision: 1,
                        operation: bcode_session_models::TextStreamOperation::Append {
                            expected_offset: 0,
                            text: "thought".to_owned(),
                        },
                    },
                },
                assistant(
                    3,
                    bcode_session_models::TextStreamOperation::Terminal {
                        status: bcode_session_models::TextStreamTerminalStatus::Cancelled,
                    },
                ),
            ] {
                let _ = manager.publish_live_event(session.id, event).await;
            }

            assert_eq!(
                manager
                    .session_history(session.id)
                    .await
                    .expect("history after live updates"),
                durable_before
            );
            let attachment = manager
                .attach_session(session.id, ClientId::new())
                .await
                .expect("active session should attach");
            assert_eq!(attachment.live_checkpoints.len(), 1);
            assert!(matches!(
                attachment.live_checkpoints[0].kind,
                SessionLiveEventKind::AssistantReasoningTextStreamUpdated { .. }
            ));
            session.id
        };

        let restarted = SessionManager::persistent(&root).expect("manager should restart");
        let attachment = restarted
            .attach_session(session_id, ClientId::new())
            .await
            .expect("restarted session should attach");
        assert!(attachment.live_checkpoints.is_empty());
        assert!(!attachment.history.iter().any(|event| matches!(
            event.kind,
            SessionEventKind::AssistantDelta { .. }
                | SessionEventKind::AssistantReasoningDelta { .. }
        )));
        std::fs::remove_dir_all(root).expect("temp session dir should be removed");
    }

    #[tokio::test]
    async fn live_assistant_text_delta_is_not_persisted() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should create");
        let session = manager
            .create_session(Some("test".to_string()), test_working_directory())
            .await
            .expect("session should create");
        let mut attachment = manager
            .attach_session(session.id, ClientId::new())
            .await
            .expect("session should attach");

        manager
            .publish_live_event(
                session.id,
                SessionLiveEventKind::AssistantTextDelta {
                    turn_id: "turn-1".to_string(),
                    segment_id: "segment-0".to_owned(),
                    segment_order: 0,
                    text: "live text".to_string(),
                },
            )
            .await
            .expect("live event should publish");

        let received = attachment
            .live_events
            .recv()
            .await
            .expect("subscriber should receive live event");
        assert_eq!(
            received.kind,
            SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn-1".to_string(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                text: "live text".to_string(),
            }
        );
        let persisted = manager
            .session_history(session.id)
            .await
            .expect("history should read");
        assert!(
            !persisted
                .iter()
                .any(|event| matches!(event.kind, SessionEventKind::AssistantDelta { .. }))
        );
        std::fs::remove_dir_all(root).expect("temp session dir should be removed");
    }

    #[tokio::test]
    async fn live_assistant_reasoning_delta_is_not_persisted() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should create");
        let session = manager
            .create_session(Some("test".to_string()), test_working_directory())
            .await
            .expect("session should create");
        let mut attachment = manager
            .attach_session(session.id, ClientId::new())
            .await
            .expect("session should attach");

        manager
            .publish_live_event(
                session.id,
                SessionLiveEventKind::AssistantReasoningDelta {
                    turn_id: "turn-1".to_string(),
                    text: "live reasoning".to_string(),
                },
            )
            .await
            .expect("live event should publish");

        let received = attachment
            .live_events
            .recv()
            .await
            .expect("subscriber should receive live event");
        assert_eq!(
            received.kind,
            SessionLiveEventKind::AssistantReasoningDelta {
                turn_id: "turn-1".to_string(),
                text: "live reasoning".to_string(),
            }
        );
        let persisted = manager
            .session_history(session.id)
            .await
            .expect("history should read");
        assert!(
            !persisted.iter().any(|event| matches!(
                event.kind,
                SessionEventKind::AssistantReasoningDelta { .. }
            ))
        );
        std::fs::remove_dir_all(root).expect("temp session dir should be removed");
    }

    #[tokio::test]
    async fn persisted_semantic_result_session_reopens_and_attaches() {
        let root = unique_temp_dir();
        let session_id = {
            let manager = SessionManager::persistent(&root).expect("manager should create");
            let session = manager
                .create_session(
                    Some("semantic reopen".to_string()),
                    test_working_directory(),
                )
                .await
                .expect("session should create");
            manager
                .append_tool_call_requested(
                    session.id,
                    crate::AppendToolCallRequestedInput {
                        tool_call_id: "call-1".to_string(),
                        tool_name: "shell.run".to_string(),
                        arguments_json: "{}".to_string(),
                        ..crate::AppendToolCallRequestedInput::default()
                    },
                )
                .await
                .expect("request should append");
            manager
                .append_event(
                    session.id,
                    SessionEventKind::ToolInvocationResultRecorded {
                        record: bcode_session_models::ToolInvocationResultRecord {
                            invocation_id: "call-1".to_string(),
                            model_output: "model fallback".to_string(),
                            is_error: false,
                            presentation: None,
                            result: Some(ToolInvocationResult::Artifact {
                                artifact: Box::new(bcode_session_models::ToolArtifact {
                                    artifact_id: "call-1-shell-run".to_string(),
                                    producer_plugin_id: "test.shell".to_string(),
                                    schema: "test.shell-artifact".to_string(),
                                    schema_version: 1,
                                    tool_call_id: Some("call-1".to_string()),
                                    title: Some("Shell run".to_string()),
                                    metadata: serde_json::json!({
                                        "mode": "terminal",
                                        "exit_code": 0,
                                        "timed_out": false,
                                        "cancelled": false,
                                        "duration_ms": null,
                                        "output_tail": "hello\n",
                                        "output_truncated": false,
                                        "output_bytes": 6,
                                        "retained_output_bytes": 6,
                                        "columns": 120,
                                        "rows": 30,
                                    }),
                                    refs: Vec::new(),
                                }),
                            }),
                        },
                    },
                )
                .await
                .expect("finish should append");
            session.id
        };

        let reopened = SessionManager::persistent(&root).expect("manager should reopen");
        let attachment = reopened
            .attach_session(session_id, ClientId::new())
            .await
            .expect("session should attach after reopen");

        assert!(attachment.history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    presentation: None,
                    result: Some(ToolInvocationResult::Artifact { artifact }),
                    ..
                },
            } if artifact.schema == "test.shell-artifact"
                && artifact.metadata["mode"] == "terminal"
                && artifact.metadata["output_tail"] == "hello\n"
        )));
        std::fs::remove_dir_all(root).expect("temp session dir should be removed");
    }

    #[allow(clippy::too_many_lines)] // Exercises append, durable retirement, late rejection, and turn cleanup as one lifecycle.
    #[tokio::test]
    async fn active_assistant_checkpoint_hydrates_attach_and_retires_at_boundaries() {
        let manager = SessionManager::default();
        let session = manager
            .create_session(Some("test".to_owned()), test_working_directory())
            .await
            .expect("session should create");
        let publish = |revision, expected_offset, text: &str| {
            SessionLiveEventKind::AssistantTextStreamUpdated {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                update: bcode_session_models::TextStreamUpdate {
                    generation: 0,
                    first_revision: revision,
                    revision,
                    operation: bcode_session_models::TextStreamOperation::Append {
                        expected_offset,
                        text: text.to_owned(),
                    },
                },
            }
        };
        let _ = manager
            .publish_live_event(session.id, publish(1, 0, "hello "))
            .await;
        let _ = manager
            .publish_live_event(session.id, publish(2, 6, "world"))
            .await;

        let attachment = manager
            .attach_session(session.id, ClientId::new())
            .await
            .expect("session should attach");
        assert_eq!(attachment.live_checkpoints.len(), 1);
        assert!(matches!(
            &attachment.live_checkpoints[0].kind,
            SessionLiveEventKind::AssistantTextStreamUpdated {
                update: bcode_session_models::TextStreamUpdate {
                    first_revision: 2,
                    revision: 2,
                    operation: bcode_session_models::TextStreamOperation::Checkpoint {
                        start_offset: 0,
                        text,
                        total_bytes: 11,
                        truncated: false,
                    },
                    ..
                },
                ..
            } if text == "hello world"
        ));

        manager
            .append_assistant_response_segment(
                session.id,
                "turn-1".to_owned(),
                "segment-0".to_owned(),
                0,
                "hello world".to_owned(),
            )
            .await
            .expect("durable segment should append");
        let attachment = manager
            .attach_session(session.id, ClientId::new())
            .await
            .expect("session should reattach");
        assert!(attachment.live_checkpoints.is_empty());

        let _ = manager
            .publish_live_event(session.id, publish(3, 11, "late"))
            .await;
        let attachment = manager
            .attach_session(session.id, ClientId::new())
            .await
            .expect("late update should not revive checkpoint");
        assert!(attachment.live_checkpoints.is_empty());

        manager
            .append_model_turn_finished(
                session.id,
                "turn-1".to_owned(),
                bcode_session_models::ModelTurnOutcome::Completed,
                None,
            )
            .await
            .expect("turn finish should clear tombstone");
        let _ = manager
            .publish_live_event(
                session.id,
                SessionLiveEventKind::AssistantTextStreamUpdated {
                    turn_id: "turn-1".to_owned(),
                    segment_id: "segment-0".to_owned(),
                    segment_order: 0,
                    update: bcode_session_models::TextStreamUpdate {
                        generation: 1,
                        first_revision: 1,
                        revision: 1,
                        operation: bcode_session_models::TextStreamOperation::Append {
                            expected_offset: 0,
                            text: "new generation".to_owned(),
                        },
                    },
                },
            )
            .await;
        let attachment = manager
            .attach_session(session.id, ClientId::new())
            .await
            .expect("new generation should hydrate after turn boundary");
        assert_eq!(attachment.live_checkpoints.len(), 1);
    }

    #[tokio::test]
    async fn active_assistant_checkpoint_is_utf8_safe_and_per_key_bounded() {
        let manager = SessionManager::default();
        let session = manager
            .create_session(Some("test".to_owned()), test_working_directory())
            .await
            .expect("session should create");
        let text = "é".repeat((256 * 1024 / 2) + 10);
        let _ = manager
            .publish_live_event(
                session.id,
                SessionLiveEventKind::AssistantTextStreamUpdated {
                    turn_id: "turn-1".to_owned(),
                    segment_id: "segment-0".to_owned(),
                    segment_order: 0,
                    update: bcode_session_models::TextStreamUpdate {
                        generation: 0,
                        first_revision: 1,
                        revision: 1,
                        operation: bcode_session_models::TextStreamOperation::Append {
                            expected_offset: 0,
                            text: text.clone(),
                        },
                    },
                },
            )
            .await;
        let attachment = manager
            .attach_session(session.id, ClientId::new())
            .await
            .expect("session should attach");
        assert!(matches!(
            &attachment.live_checkpoints[0].kind,
            SessionLiveEventKind::AssistantTextStreamUpdated {
                update: bcode_session_models::TextStreamUpdate {
                    operation: bcode_session_models::TextStreamOperation::Checkpoint {
                        start_offset,
                        text: retained,
                        total_bytes,
                        truncated: true,
                    },
                    ..
                },
                ..
            } if *start_offset > 0
                && retained.len() <= 256 * 1024
                && *total_bytes == text.len()
        ));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // Covers independent keys, attach hydration, durable retirement, terminal absorption, and turn cleanup.
    async fn active_reasoning_checkpoints_hydrate_attach_and_retire_at_boundaries() {
        let manager = SessionManager::default();
        let session = manager
            .create_session(Some("test".to_owned()), test_working_directory())
            .await
            .expect("session should create");
        let publish = |part_id: &str, generation, revision, expected_offset, text: &str| {
            SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                turn_id: "turn-1".to_owned(),
                activity_id: "activity-1".to_owned(),
                activity_order: 0,
                part_id: part_id.to_owned(),
                kind: bcode_session_models::ReasoningContentKind::Summary,
                role: bcode_session_models::ReasoningContentRole::Milestone,
                part_order: u32::from(part_id == "part-1"),
                update: bcode_session_models::TextStreamUpdate {
                    generation,
                    first_revision: revision,
                    revision,
                    operation: bcode_session_models::TextStreamOperation::Append {
                        expected_offset,
                        text: text.to_owned(),
                    },
                },
            }
        };
        let _ = manager
            .publish_live_event(session.id, publish("part-0", 0, 1, 0, "first"))
            .await;
        let _ = manager
            .publish_live_event(session.id, publish("part-1", 0, 1, 0, "second"))
            .await;
        let _ = manager
            .publish_live_event(
                session.id,
                SessionLiveEventKind::AssistantTextStreamUpdated {
                    turn_id: "turn-1".to_owned(),
                    segment_id: "part-0".to_owned(),
                    segment_order: 0,
                    update: bcode_session_models::TextStreamUpdate {
                        generation: 0,
                        first_revision: 1,
                        revision: 1,
                        operation: bcode_session_models::TextStreamOperation::Append {
                            expected_offset: 0,
                            text: "assistant".to_owned(),
                        },
                    },
                },
            )
            .await;

        let attachment = manager
            .attach_session(session.id, ClientId::new())
            .await
            .expect("session should attach");
        assert_eq!(attachment.live_checkpoints.len(), 3);
        assert!(attachment.live_checkpoints.iter().any(|event| matches!(
            &event.kind,
            SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                part_id,
                update: bcode_session_models::TextStreamUpdate {
                    operation: bcode_session_models::TextStreamOperation::Checkpoint {
                        text,
                        total_bytes: 5,
                        truncated: false,
                        ..
                    },
                    ..
                },
                ..
            } if part_id == "part-0" && text == "first"
        )));

        manager
            .append_assistant_reasoning_activity(
                session.id,
                "turn-1".to_owned(),
                bcode_session_models::ReasoningActivity {
                    activity_id: "activity-1".to_owned(),
                    order: 0,
                    status: bcode_session_models::ReasoningActivityStatus::Completed,
                    parts: vec![bcode_session_models::ReasoningPart {
                        part_id: "part-0".to_owned(),
                        kind: bcode_session_models::ReasoningContentKind::Summary,
                        role: bcode_session_models::ReasoningContentRole::Milestone,
                        order: 0,
                        text: "first".to_owned(),
                    }],
                    opaque: false,
                },
            )
            .await
            .expect("durable reasoning should append");
        let _ = manager
            .publish_live_event(session.id, publish("part-0", 0, 2, 5, " late"))
            .await;
        let attachment = manager
            .attach_session(session.id, ClientId::new())
            .await
            .expect("session should reattach");
        assert!(!attachment.live_checkpoints.iter().any(|event| matches!(
            &event.kind,
            SessionLiveEventKind::AssistantReasoningTextStreamUpdated { part_id, .. }
                if part_id == "part-0"
        )));
        assert!(attachment.live_checkpoints.iter().any(|event| matches!(
            &event.kind,
            SessionLiveEventKind::AssistantReasoningTextStreamUpdated { part_id, .. }
                if part_id == "part-1"
        )));

        let _ = manager
            .publish_live_event(
                session.id,
                SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                    turn_id: "turn-1".to_owned(),
                    activity_id: "activity-1".to_owned(),
                    activity_order: 0,
                    part_id: "part-1".to_owned(),
                    kind: bcode_session_models::ReasoningContentKind::Summary,
                    role: bcode_session_models::ReasoningContentRole::Milestone,
                    part_order: 1,
                    update: bcode_session_models::TextStreamUpdate {
                        generation: 0,
                        first_revision: 2,
                        revision: 2,
                        operation: bcode_session_models::TextStreamOperation::Terminal {
                            status: bcode_session_models::TextStreamTerminalStatus::Cancelled,
                        },
                    },
                },
            )
            .await;
        let _ = manager
            .publish_live_event(session.id, publish("part-1", 0, 3, 6, " late"))
            .await;
        let attachment = manager
            .attach_session(session.id, ClientId::new())
            .await
            .expect("terminal reasoning should remain retired");
        assert_eq!(attachment.live_checkpoints.len(), 1);
        assert!(matches!(
            attachment.live_checkpoints[0].kind,
            SessionLiveEventKind::AssistantTextStreamUpdated { .. }
        ));

        manager
            .append_model_turn_finished(
                session.id,
                "turn-1".to_owned(),
                bcode_session_models::ModelTurnOutcome::Cancelled,
                None,
            )
            .await
            .expect("turn finish should clear checkpoints and tombstones");
        let _ = manager
            .publish_live_event(session.id, publish("part-0", 1, 1, 0, "new"))
            .await;
        let attachment = manager
            .attach_session(session.id, ClientId::new())
            .await
            .expect("new generation should hydrate after turn cleanup");
        assert_eq!(attachment.live_checkpoints.len(), 1);
        assert!(matches!(
            &attachment.live_checkpoints[0].kind,
            SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                part_id,
                update: bcode_session_models::TextStreamUpdate { generation: 1, .. },
                ..
            } if part_id == "part-0"
        ));
    }

    #[tokio::test]
    async fn active_reasoning_checkpoint_is_utf8_safe_and_per_key_bounded() {
        let manager = SessionManager::default();
        let session = manager
            .create_session(Some("test".to_owned()), test_working_directory())
            .await
            .expect("session should create");
        let text = "é".repeat((256 * 1024 / 2) + 10);
        let _ = manager
            .publish_live_event(
                session.id,
                SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                    turn_id: "turn-1".to_owned(),
                    activity_id: "activity-1".to_owned(),
                    activity_order: 0,
                    part_id: "part-0".to_owned(),
                    kind: bcode_session_models::ReasoningContentKind::Raw,
                    role: bcode_session_models::ReasoningContentRole::Detail,
                    part_order: 0,
                    update: bcode_session_models::TextStreamUpdate {
                        generation: 0,
                        first_revision: 1,
                        revision: 1,
                        operation: bcode_session_models::TextStreamOperation::Append {
                            expected_offset: 0,
                            text: text.clone(),
                        },
                    },
                },
            )
            .await;
        let attachment = manager
            .attach_session(session.id, ClientId::new())
            .await
            .expect("session should attach");
        assert!(matches!(
            &attachment.live_checkpoints[0].kind,
            SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                update: bcode_session_models::TextStreamUpdate {
                    operation: bcode_session_models::TextStreamOperation::Checkpoint {
                        start_offset,
                        text: retained,
                        total_bytes,
                        truncated: true,
                    },
                    ..
                },
                ..
            } if *start_offset > 0
                && retained.len() <= 256 * 1024
                && retained.is_char_boundary(0)
                && *total_bytes == text.len()
        ));
    }

    #[test]
    fn live_event_broker_drops_without_receivers_and_tracks_publish_counts() {
        let broker = super::SessionLiveEventBroker::new(4);
        let session_id = SessionId::new();
        let event = SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn-1".to_string(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                text: "hello".to_string(),
            },
        };

        assert_eq!(broker.publish(event.clone()), None);
        assert_eq!(broker.published.load(Ordering::Relaxed), 0);
        assert_eq!(broker.dropped_no_receivers.load(Ordering::Relaxed), 1);

        let mut receiver = broker.subscribe();
        assert_eq!(broker.publish(event.clone()), Some(event.clone()));
        assert_eq!(broker.published.load(Ordering::Relaxed), 1);
        assert_eq!(broker.dropped_no_receivers.load(Ordering::Relaxed), 1);
        assert_eq!(
            receiver.try_recv().expect("event should be available"),
            event
        );
    }

    #[test]
    fn trace_event_round_trips_through_bmux_codec() {
        let mut metadata = BTreeMap::new();
        metadata.insert("conversation_hash".to_string(), "abc123".to_string());
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 0,
            timestamp_ms: 1,
            session_id: bcode_session_models::SessionId::new(),
            provenance: None,
            kind: SessionEventKind::TraceEvent {
                trace: Box::new(SessionTraceEvent {
                    timestamp_ms: 1,
                    turn_id: Some("turn-1".to_string()),
                    phase: SessionTracePhase::ModelRequestBuilt,
                    payload: SessionTracePayload::ModelRequestBuilt {
                        provider: "provider".to_string(),
                        model: "model".to_string(),
                        agent_id: "build".to_string(),
                        message_count: 1,
                        tool_count: 2,
                        system_prompt_chars: 3,
                        prompt_cache_mode: "auto".to_string(),
                        conversation_reuse_mode: "auto".to_string(),
                        uses_previous_provider_response: false,
                        metadata,
                        request: None,
                    },
                }),
            },
        };

        let bytes = bmux_codec::to_vec(&event).expect("trace event should encode");
        let decoded: SessionEvent =
            bmux_codec::from_bytes(&bytes).expect("trace event should decode");

        assert_eq!(decoded, event);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn session_event_kind_binary_tags_are_append_only() {
        let cases = session_event_kind_tag_cases();
        for (expected_tag, name, kind) in cases {
            assert_eq!(
                encoded_variant_tag(&kind),
                expected_tag,
                "persisted SessionEventKind tag changed for {name}; append new variants only or add compatibility decoding plus binary fixtures"
            );
        }
    }

    #[test]
    fn session_trace_phase_binary_tags_are_append_only() {
        let cases = session_trace_phase_tag_cases();
        for (expected_tag, name, phase) in cases {
            assert_eq!(
                encoded_variant_tag(&phase),
                expected_tag,
                "persisted SessionTracePhase tag changed for {name}; append new variants only or add compatibility decoding plus binary fixtures"
            );
        }
    }

    #[test]
    fn session_trace_payload_binary_tags_are_append_only() {
        let cases = session_trace_payload_tag_cases();
        for (expected_tag, name, payload) in cases {
            assert_eq!(
                encoded_variant_tag(&payload),
                expected_tag,
                "persisted SessionTracePayload tag changed for {name}; append new variants only or add compatibility decoding plus binary fixtures"
            );
        }
    }

    #[test]
    fn old_order_trace_payload_tool_events_decode_as_same_variant() {
        #[allow(dead_code)]
        #[derive(Serialize)]
        enum OldOrderSessionTracePayload {
            ModelRequestBuilt,
            ProviderRound,
            ProviderEvent,
            ToolInvocationStarted {
                tool_call_id: String,
                plugin_id: String,
                tool_name: String,
                side_effect: String,
                requires_permission: bool,
                arguments: Option<TraceBlobRef>,
            },
        }

        let old_payload = OldOrderSessionTracePayload::ToolInvocationStarted {
            tool_call_id: "call".to_string(),
            plugin_id: "plugin".to_string(),
            tool_name: "tool".to_string(),
            side_effect: "read_only".to_string(),
            requires_permission: false,
            arguments: None,
        };

        let bytes = bmux_codec::to_positional_vec(&old_payload).expect("old payload should encode");
        let decoded: SessionTracePayload =
            bmux_codec::from_positional_bytes(&bytes).expect("old payload should decode");

        assert!(matches!(
            decoded,
            SessionTracePayload::ToolInvocationStarted { tool_call_id, .. }
                if tool_call_id == "call"
        ));
    }

    #[test]
    fn all_trace_payload_variants_round_trip_through_bmux_codec() {
        let payloads = vec![
            SessionTracePayload::ProviderRound {
                provider_turn_id: Some("provider-turn".to_string()),
                provider: "provider".to_string(),
                round: Some(1),
                stop_reason: Some("EndTurn".to_string()),
                duration_ms: Some(42),
                error: None,
            },
            SessionTracePayload::ProviderEvent {
                event_type: "text_delta".to_string(),
                detail: Some("detail".to_string()),
            },
            SessionTracePayload::ProviderStreamEvent(ProviderStreamEvent::ToolCallProgress {
                tool_call_id: "call".to_string(),
                tool_name: "tool".to_string(),
                argument_bytes: 12,
            }),
            SessionTracePayload::ToolInvocationStarted {
                tool_call_id: "call".to_string(),
                plugin_id: "plugin".to_string(),
                tool_name: "tool".to_string(),
                side_effect: "read_only".to_string(),
                requires_permission: false,
                arguments: None,
            },
            SessionTracePayload::ToolPolicyEvaluated {
                tool_call_id: "call".to_string(),
                agent_id: "build".to_string(),
                decision: "allow".to_string(),
                reason: None,
            },
            SessionTracePayload::ToolPermissionWait {
                permission_id: "perm".to_string(),
                tool_call_id: "call".to_string(),
                approved: Some(true),
                duration_ms: Some(7),
            },
            SessionTracePayload::ToolInvocationFinished {
                tool_call_id: "call".to_string(),
                duration_ms: 9,
                is_error: false,
                output_bytes: 12,
                output: None,
            },
        ];

        for payload in payloads {
            let bytes = bmux_codec::to_vec(&payload).expect("payload should encode");
            let decoded: SessionTracePayload =
                bmux_codec::from_bytes(&bytes).expect("payload should decode");
            assert_eq!(decoded, payload);
        }
    }

    #[tokio::test]
    async fn terminal_tool_result_retry_returns_original_event_without_duplicate_history() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("terminal retry".to_owned()), test_working_directory())
            .await
            .expect("session should be created");
        manager
            .append_tool_call_requested(
                session.id,
                AppendToolCallRequestedInput {
                    tool_call_id: "tool-retry".to_owned(),
                    tool_name: "fixture.run".to_owned(),
                    arguments_json: "{}".to_owned(),
                    ..AppendToolCallRequestedInput::default()
                },
            )
            .await
            .expect("tool request should append");
        let record = bcode_session_models::ToolInvocationResultRecord {
            invocation_id: "tool-retry".to_owned(),
            model_output: "first durable result".to_owned(),
            is_error: false,
            presentation: None,
            result: None,
        };
        let first = manager
            .append_tool_invocation_result(session.id, record)
            .await
            .expect("first result should append");
        let retry = manager
            .append_tool_invocation_result(
                session.id,
                bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "tool-retry".to_owned(),
                    model_output: "retry must not replace durable result".to_owned(),
                    is_error: true,
                    presentation: None,
                    result: None,
                },
            )
            .await
            .expect("retry should be idempotent");

        assert_eq!(retry, first);
        let history = manager
            .session_history(session.id)
            .await
            .expect("history should load");
        assert_eq!(
            history
                .iter()
                .filter(|event| matches!(
                    event.kind,
                    SessionEventKind::ToolInvocationResultRecorded { .. }
                ))
                .count(),
            1
        );
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ToolInvocationResultRecorded { record }
                if record.model_output == "first durable result" && !record.is_error
        )));

        let memory_manager = SessionManager::default();
        let memory_session = memory_manager
            .create_session(Some("memory retry".to_owned()), test_working_directory())
            .await
            .expect("memory session");
        let memory_record = bcode_session_models::ToolInvocationResultRecord {
            invocation_id: "memory-tool".to_owned(),
            model_output: "memory result".to_owned(),
            is_error: false,
            presentation: None,
            result: None,
        };
        let memory_first = memory_manager
            .append_tool_invocation_result(memory_session.id, memory_record.clone())
            .await
            .expect("memory result");
        let memory_retry = memory_manager
            .append_tool_invocation_result(memory_session.id, memory_record)
            .await
            .expect("memory retry");
        assert_eq!(memory_retry, memory_first);
        assert_eq!(
            memory_manager
                .session_history(memory_session.id)
                .await
                .expect("memory history")
                .iter()
                .filter(|event| matches!(
                    event.kind,
                    SessionEventKind::ToolInvocationResultRecorded { .. }
                ))
                .count(),
            1
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn persistent_manager_restores_session_history() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("test".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "hello".to_string())
            .await
            .expect("message should append");
        manager
            .append_assistant_delta(session.id, "partial".to_string())
            .await
            .expect("assistant delta should append");
        manager
            .append_assistant_message(session.id, "complete".to_string())
            .await
            .expect("assistant message should append");
        manager
            .append_tool_call_requested(
                session.id,
                crate::AppendToolCallRequestedInput {
                    tool_call_id: "tool-1".to_string(),
                    tool_name: "read".to_string(),
                    arguments_json: r#"{"path":"README.md"}"#.to_string(),
                    ..crate::AppendToolCallRequestedInput::default()
                },
            )
            .await
            .expect("tool request should append");
        manager
            .append_event(
                session.id,
                SessionEventKind::ToolInvocationResultRecorded {
                    record: bcode_session_models::ToolInvocationResultRecord {
                        invocation_id: "tool-1".to_string(),
                        model_output: "ok".to_string(),
                        is_error: false,
                        presentation: None,
                        result: None,
                    },
                },
            )
            .await
            .expect("tool result should append");
        manager
            .append_model_changed(session.id, "provider".to_string(), "model".to_string())
            .await
            .expect("model change should append");
        manager
            .append_agent_changed(session.id, "plan".to_string())
            .await
            .expect("agent change should append");
        manager
            .append_model_turn_started(session.id, "turn-1".to_string())
            .await
            .expect("turn start should append");
        manager
            .append_model_turn_finished(
                session.id,
                "turn-1".to_string(),
                bcode_session_models::ModelTurnOutcome::Completed,
                None,
            )
            .await
            .expect("turn finish should append");
        manager
            .append_model_usage(
                session.id,
                "turn-1".to_string(),
                bcode_session_models::SessionTokenUsage {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    total_tokens: Some(15),
                    cached_input_tokens: Some(3),
                    cache_write_input_tokens: Some(4),
                    reasoning_tokens: Some(2),
                },
            )
            .await
            .expect("model usage should append");
        manager
            .append_system_message(session.id, "system".to_string())
            .await
            .expect("system message should append");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let sessions = restored.list_sessions(&test_working_directory()).await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session.id);
        assert_eq!(sessions[0].name.as_deref(), Some("test"));

        let history = restored
            .session_history(session.id)
            .await
            .expect("history should load");
        assert!(history.iter().all(|event| event.schema_version
            == bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. } if text == "hello"
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::AssistantDelta { text } if text == "partial"
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::AssistantMessage { text } if text == "complete"
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ToolCallRequested { tool_call_id, tool_name, .. }
                if tool_call_id == "tool-1" && tool_name == "read"
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ToolInvocationResultRecorded { record }
                if record.invocation_id == "tool-1"
                    && record.model_output == "ok"
                    && !record.is_error
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ModelChanged { provider, model }
                if provider == "provider" && model == "model"
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::AgentChanged { agent_id } if agent_id == "plan"
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ModelTurnStarted { turn_id } if turn_id == "turn-1"
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ModelTurnFinished { turn_id, outcome, .. }
                if turn_id == "turn-1" && *outcome == bcode_session_models::ModelTurnOutcome::Completed
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ModelUsage { turn_id, usage }
                if turn_id == "turn-1" && usage.metered_total_tokens() == Some(15)
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::SystemMessage { text } if text == "system"
        )));
        let runtime_selection = restored
            .current_runtime_selection(session.id)
            .await
            .expect("runtime selection should restore");
        assert_eq!(runtime_selection.agent_id.as_deref(), Some("plan"));
        assert_eq!(
            runtime_selection.provider_plugin_id.as_deref(),
            Some("provider")
        );
        assert_eq!(runtime_selection.model_id.as_deref(), Some("model"));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn future_writer_preparation_is_incompatible_without_migration() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager");
        let session = manager
            .create_session(Some("future".to_owned()), test_working_directory())
            .await
            .expect("current session");
        let db = db::SessionDb::open_existing_turso_in_root(session.id, &root)
            .await
            .expect("session DB");
        db.database()
            .update("session_storage_contract")
            .value("writer_epoch", switchy::database::DatabaseValue::Int64(99))
            .where_eq("contract_id", switchy::database::DatabaseValue::Int32(1))
            .execute(db.database())
            .await
            .expect("future writer epoch");
        drop(db);

        let snapshot = manager
            .prepare_session_open(session.id)
            .await
            .expect("classify future writer");

        assert_eq!(
            snapshot.outcome,
            Some(SessionOpenTerminalOutcome::WriterIncompatible {
                actual: Some(99),
                expected: u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
            })
        );
        let durable_epoch = db::SessionDb::open_existing_turso_in_root(session.id, &root)
            .await
            .expect("unchanged future session DB")
            .storage_writer_epoch()
            .await
            .expect("future writer epoch after classification");
        assert_eq!(durable_epoch, 99);
        std::fs::remove_dir_all(root).expect("temp dir cleanup");
    }

    #[tokio::test]
    async fn current_session_preparation_is_immediately_ready_without_operation() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager");
        let session = manager
            .create_session(Some("current".to_owned()), test_working_directory())
            .await
            .expect("current session");

        let snapshot = manager
            .prepare_session_open(session.id)
            .await
            .expect("prepare current session");

        assert_eq!(snapshot.outcome, Some(SessionOpenTerminalOutcome::Ready));
        assert_eq!(snapshot.progress.stage, SessionMigrationStage::Complete);
        let second = manager
            .prepare_session_open(session.id)
            .await
            .expect("prepare current session again");
        assert_eq!(second.outcome, Some(SessionOpenTerminalOutcome::Ready));
        assert_eq!(second.progress.stage, SessionMigrationStage::Complete);
        std::fs::remove_dir_all(root).expect("temp dir cleanup");
    }

    #[tokio::test]
    async fn generated_migration_benchmark_store_is_contiguous_and_legacy() {
        let root = unique_temp_dir();
        let session_id =
            generate_legacy_migration_benchmark_store(&root, MigrationBenchmarkProfile::Small)
                .await;
        let db = db::SessionDb::open_existing_turso_in_root(session_id, &root)
            .await
            .expect("generated DB");
        assert_eq!(db.last_event_sequence().await.expect("tail"), Some(99));
        assert_eq!(db.storage_writer_epoch().await.expect("epoch"), 3);
        assert_eq!(db.all_events_strict().await.expect("history").len(), 100);
        drop(db);
        let maintenance =
            lease::acquire_session_maintenance_guard(&root, session_id).expect("maintenance guard");
        let write = lease::acquire_maintenance_session_write_lock(&maintenance, &root, session_id)
            .expect("write guard");
        let migrated =
            db::SessionDb::migrate_turso_in_root(session_id, &root, &maintenance, &write)
                .await
                .expect("multi-page migration");
        assert_eq!(
            migrated
                .storage_writer_epoch()
                .await
                .expect("current epoch"),
            u64::from(db::CURRENT_SESSION_STORAGE_WRITER_EPOCH)
        );
        migrated
            .validate_write_readiness()
            .await
            .expect("multi-page migration readiness");
        assert_eq!(
            migrated.all_events_strict().await.expect("history").len(),
            100
        );
        std::fs::remove_dir_all(root).expect("temp dir cleanup");
    }

    #[tokio::test]
    async fn migration_pages_more_than_one_thousand_events_without_gaps_or_duplicates() {
        let root = unique_temp_dir();
        let session_id =
            generate_legacy_migration_benchmark_store(&root, MigrationBenchmarkProfile::Medium)
                .await;
        let maintenance =
            lease::acquire_session_maintenance_guard(&root, session_id).expect("maintenance guard");
        let write = lease::acquire_maintenance_session_write_lock(&maintenance, &root, session_id)
            .expect("write guard");
        let migrated =
            db::SessionDb::migrate_turso_in_root(session_id, &root, &maintenance, &write)
                .await
                .expect("multi-page migration");
        assert_eq!(
            migrated.last_event_sequence().await.expect("tail"),
            Some(4_999)
        );
        assert_eq!(
            migrated.all_events_strict().await.expect("history").len(),
            5_000
        );
        migrated
            .validate_write_readiness()
            .await
            .expect("multi-page migration readiness");
        std::fs::remove_dir_all(root).expect("temp dir cleanup");
    }

    #[tokio::test]
    #[ignore = "manual release acceptance for migration progress overhead"]
    async fn benchmark_migration_progress_reporting_overhead() {
        const RUNS: usize = 5;

        async fn measure() -> (Vec<u128>, Vec<u128>) {
            let mut without_progress = Vec::with_capacity(RUNS);
            let mut with_progress = Vec::with_capacity(RUNS);
            for index in 0..RUNS * 2 {
                let report_progress = if index % 2 == 0 {
                    index % 4 == 2
                } else {
                    index % 4 == 1
                };
                let root = unique_temp_dir();
                let session_id = generate_legacy_migration_benchmark_store(
                    &root,
                    MigrationBenchmarkProfile::Small,
                )
                .await;
                let maintenance = lease::acquire_session_maintenance_guard(&root, session_id)
                    .expect("maintenance guard");
                let write =
                    lease::acquire_maintenance_session_write_lock(&maintenance, &root, session_id)
                        .expect("write guard");
                let updates = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                let progress = report_progress.then(|| {
                    let updates = updates.clone();
                    std::sync::Arc::new(move |_| {
                        updates.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }) as db::SessionMigrationProgressCallback
                });
                let started = std::time::Instant::now();
                db::SessionDb::migrate_turso_in_root_observed(
                    session_id,
                    &root,
                    &maintenance,
                    &write,
                    MetricsRegistry::disabled(),
                    progress,
                )
                .await
                .expect("benchmark migration");
                let elapsed = started.elapsed().as_millis();
                if report_progress {
                    with_progress.push(elapsed);
                } else {
                    without_progress.push(elapsed);
                }
                if report_progress {
                    assert!(
                        updates.load(std::sync::atomic::Ordering::Relaxed) > 0,
                        "observed run must publish progress"
                    );
                }
                drop(write);
                drop(maintenance);
                std::fs::remove_dir_all(root).expect("benchmark cleanup");
            }
            without_progress.sort_unstable();
            with_progress.sort_unstable();
            (without_progress, with_progress)
        }

        let (without_progress, with_progress) = measure().await;
        let baseline_median = without_progress[RUNS / 2];
        let observed_median = with_progress[RUNS / 2];
        let budget_ms = baseline_median.saturating_mul(MIGRATION_PROGRESS_OVERHEAD_PERCENT_BUDGET)
            / 100
            + MIGRATION_PROGRESS_OVERHEAD_FIXED_BUDGET_MS;
        let overhead_ms = observed_median.saturating_sub(baseline_median);
        eprintln!(
            "migration_progress_overhead without_ms={without_progress:?} with_ms={with_progress:?} baseline_median_ms={baseline_median} observed_median_ms={observed_median} overhead_ms={overhead_ms} budget_ms={budget_ms}"
        );
        assert!(
            overhead_ms <= budget_ms,
            "progress overhead {overhead_ms} ms exceeds budget {budget_ms} ms"
        );
    }

    #[tokio::test]
    #[ignore = "manual release acceptance for current-session preparation latency"]
    async fn benchmark_current_session_preparation_latency() {
        const RUNS: usize = 100;

        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager");
        let session = manager
            .create_session(Some("current latency".to_owned()), test_working_directory())
            .await
            .expect("current session");
        let mut durations = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let started = std::time::Instant::now();
            let snapshot = manager
                .prepare_session_open(session.id)
                .await
                .expect("prepare current session");
            durations.push(started.elapsed().as_micros());
            assert_eq!(snapshot.outcome, Some(SessionOpenTerminalOutcome::Ready));
        }
        durations.sort_unstable();
        let median_us = durations[RUNS / 2];
        let p95_us = durations[RUNS * 95 / 100];
        let max_us = durations[RUNS - 1];
        eprintln!(
            "current_session_prepare_latency runs={RUNS} median_us={median_us} p95_us={p95_us} max_us={max_us} budget_p95_ms={CURRENT_SESSION_PREPARE_P95_BUDGET_MS}"
        );
        assert!(
            p95_us <= CURRENT_SESSION_PREPARE_P95_BUDGET_MS * 1_000,
            "current-session preparation p95 {p95_us} us exceeds budget"
        );
        std::fs::remove_dir_all(root).expect("benchmark cleanup");
    }

    #[tokio::test]
    #[ignore = "manual release acceptance for current-session preparation event-count independence"]
    async fn benchmark_current_session_preparation_is_event_count_independent() {
        const RUNS: usize = 30;

        for profile in [
            MigrationBenchmarkProfile::Small,
            MigrationBenchmarkProfile::Medium,
            MigrationBenchmarkProfile::Large,
        ] {
            let root = unique_temp_dir();
            let session_id = generate_current_migration_benchmark_store(&root, profile).await;
            let manager = SessionManager::persistent(&root).expect("manager");
            let store = manager.store.as_ref().expect("persistent store");
            let lease = manager
                .acquire_session_lease_for_load(session_id, store)
                .await
                .expect("current runtime lease");
            manager.inner.lock().await.leases.insert(session_id, lease);
            let _current_db = db::SessionDb::open_existing_turso_in_root(session_id, &root)
                .await
                .expect("current benchmark DB");
            let mut durations = Vec::with_capacity(RUNS);
            for _ in 0..RUNS {
                let started = std::time::Instant::now();
                let snapshot = manager
                    .prepare_session_open(session_id)
                    .await
                    .expect("prepare current session");
                durations.push(started.elapsed().as_micros());
                assert_eq!(snapshot.outcome, Some(SessionOpenTerminalOutcome::Ready));
            }
            durations.sort_unstable();
            let p95_us = durations[RUNS * 95 / 100];
            assert!(
                p95_us <= CURRENT_SESSION_PREPARE_P95_BUDGET_MS * 1_000,
                "{} current-open p95 {p95_us} us exceeds {} ms gate",
                profile.name(),
                CURRENT_SESSION_PREPARE_P95_BUDGET_MS
            );
            eprintln!(
                "current_session_prepare_by_events profile={} events={} p95_us={p95_us}",
                profile.name(),
                profile.event_count()
            );
            drop(manager);
            std::fs::remove_dir_all(root).expect("benchmark cleanup");
        }
    }

    #[tokio::test]
    async fn write_readiness_uses_actor_connection_before_followup_append() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("followup".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "first".to_string())
            .await
            .expect("first message should append");

        manager
            .set_session_composer_draft(session.id, "draft".to_string())
            .await
            .expect("draft should persist on actor connection");
        assert_eq!(
            manager
                .session_composer_draft(session.id)
                .await
                .expect("draft should load"),
            Some("draft".to_string())
        );
        manager
            .require_write_readiness(session.id)
            .await
            .expect("followup should be ready");
        manager
            .append_user_message(session.id, ClientId::new(), "second".to_string())
            .await
            .expect("followup should append");

        let history = manager
            .session_history(session.id)
            .await
            .expect("history should load");
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. } if text == "second"
        )));
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn fork_session_from_prompt_copies_history_before_prompt_and_returns_draft() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("source".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        manager
            .append_model_changed(session.id, "provider".to_string(), "model".to_string())
            .await
            .expect("model should append");
        manager
            .append_user_message(session.id, ClientId::new(), "first prompt".to_string())
            .await
            .expect("first prompt should append");
        manager
            .append_assistant_message(session.id, "first response".to_string())
            .await
            .expect("assistant response should append");
        let second_prompt = manager
            .append_user_message(session.id, ClientId::new(), "second prompt".to_string())
            .await
            .expect("second prompt should append")
            .into_iter()
            .find(|event| matches!(event.kind, SessionEventKind::UserMessage { .. }))
            .expect("user message event should exist");
        manager
            .append_assistant_message(session.id, "second response".to_string())
            .await
            .expect("second response should append");

        let result = manager
            .fork_session_from_prompt(session.id, second_prompt.sequence, None)
            .await
            .expect("session should fork");

        assert_ne!(result.session.id, session.id);
        assert_eq!(result.session.name.as_deref(), Some("[fork] source"));
        assert_eq!(result.draft.as_deref(), Some("second prompt"));
        assert_eq!(
            result.session.fork.as_ref().map(|fork| fork.kind),
            Some(SessionForkKind::Fork)
        );
        assert_eq!(
            result
                .session
                .fork
                .as_ref()
                .and_then(|fork| fork.source_prompt_sequence),
            Some(second_prompt.sequence)
        );

        let fork_history = manager
            .session_history(result.session.id)
            .await
            .expect("fork history should load");
        assert!(fork_history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ModelChanged { provider, model }
                if provider == "provider" && model == "model"
        )));
        assert!(fork_history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. } if text == "first prompt"
        )));
        assert!(fork_history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::AssistantMessage { text } if text == "first response"
        )));
        assert!(!fork_history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. } if text == "second prompt"
        )));
        assert!(fork_history.iter().any(|event| {
            matches!(
                &event.kind,
                SessionEventKind::SessionForked {
                    source_session_id,
                    kind: SessionForkKind::Fork,
                    ..
                } if *source_session_id == session.id
            )
        }));
        let copied = fork_history
            .iter()
            .find(|event| matches!(event.kind, SessionEventKind::AssistantMessage { .. }))
            .expect("copied assistant message should exist");
        assert!(copied.provenance.is_some());

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let restored_sessions = restored.list_sessions(&test_working_directory()).await;
        let restored_fork = restored_sessions
            .iter()
            .find(|summary| summary.id == result.session.id)
            .expect("fork should be listed after restore");
        assert_eq!(
            restored_fork.fork.as_ref().map(|fork| fork.kind),
            Some(SessionForkKind::Fork)
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn clone_session_at_generation_rejects_stale_snapshot_without_creating_clone() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let source = manager
            .create_session(Some("source".to_owned()), test_working_directory())
            .await
            .expect("source session");
        manager
            .append_user_message(source.id, ClientId::new(), "prompt".to_owned())
            .await
            .expect("prompt");
        let generation = manager
            .session_history(source.id)
            .await
            .expect("history")
            .last()
            .expect("source event")
            .sequence;
        manager
            .append_assistant_message(source.id, "changed".to_owned())
            .await
            .expect("source change");
        let session_count = manager.list_sessions(&test_working_directory()).await.len();

        let error = manager
            .clone_session_at_generation(source.id, None, Some(generation))
            .await
            .expect_err("stale generation must fail");
        assert!(matches!(
            error,
            SessionError::CloneGenerationChanged {
                session_id,
                expected,
                current,
            } if session_id == source.id && expected == generation && current > expected
        ));
        assert_eq!(
            manager.list_sessions(&test_working_directory()).await.len(),
            session_count,
            "a rejected snapshot must not leave a clone behind"
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn clone_session_at_generation_copies_exact_accepted_snapshot() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let source = manager
            .create_session(Some("source".to_owned()), test_working_directory())
            .await
            .expect("source session");
        manager
            .append_user_message(source.id, ClientId::new(), "prompt".to_owned())
            .await
            .expect("prompt");
        let source_history = manager.session_history(source.id).await.expect("history");
        let generation = source_history.last().expect("source event").sequence;

        let clone = manager
            .clone_session_at_generation(source.id, None, Some(generation))
            .await
            .expect("matching generation should clone");
        assert_eq!(
            clone
                .session
                .fork
                .as_ref()
                .and_then(|fork| fork.source_cutoff_sequence),
            Some(generation)
        );
        let clone_history = manager
            .session_history(clone.session.id)
            .await
            .expect("clone history");
        let generation_string = generation.to_string();
        assert!(clone_history.iter().any(|event| {
            event.provenance.as_ref().is_some_and(|provenance| {
                provenance.source_event_id.as_deref() == Some(generation_string.as_str())
            })
        }));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn clone_session_copies_full_history_and_records_provenance() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("source".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "prompt".to_string())
            .await
            .expect("prompt should append");
        manager
            .append_assistant_message(session.id, "response".to_string())
            .await
            .expect("response should append");

        let result = manager
            .clone_session(session.id, None)
            .await
            .expect("session should clone");

        assert_ne!(result.session.id, session.id);
        assert_eq!(result.session.name.as_deref(), Some("[clone] source"));
        assert_eq!(result.draft, None);
        assert_eq!(
            result.session.fork.as_ref().map(|fork| fork.kind),
            Some(SessionForkKind::Clone)
        );

        let clone_history = manager
            .session_history(result.session.id)
            .await
            .expect("clone history should load");
        assert!(clone_history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. } if text == "prompt"
        )));
        assert!(clone_history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::AssistantMessage { text } if text == "response"
        )));
        assert!(clone_history.iter().any(|event| {
            matches!(
                &event.kind,
                SessionEventKind::SessionForked {
                    source_session_id,
                    kind: SessionForkKind::Clone,
                    ..
                } if *source_session_id == session.id
            )
        }));
        assert!(
            clone_history
                .iter()
                .all(|event| event.session_id == result.session.id)
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn clone_before_any_boundary_preserves_uncompacted_context() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager");
        let source = manager
            .create_session(Some("source".to_string()), test_working_directory())
            .await
            .expect("source");
        manager
            .append_user_message(source.id, ClientId::new(), "prompt".to_string())
            .await
            .expect("prompt");
        manager
            .append_assistant_message(source.id, "response".to_string())
            .await
            .expect("response");
        let clone = manager.clone_session(source.id, None).await.expect("clone");
        let context = manager
            .model_context_events(clone.session.id)
            .await
            .expect("context");
        assert!(context.iter().any(|event| matches!(&event.kind, SessionEventKind::UserMessage { text, .. } if text == "prompt")));
        assert!(context.iter().any(|event| matches!(&event.kind, SessionEventKind::AssistantMessage { text } if text == "response")));
        assert!(!context.iter().any(|event| matches!(
            event.kind,
            SessionEventKind::ContextCompacted { .. }
                | SessionEventKind::ProviderContextCompacted { .. }
        )));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn clone_after_provider_boundary_preserves_rewritten_canonical_context() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager");
        let source = manager
            .create_session(Some("source".to_string()), test_working_directory())
            .await
            .expect("source");
        let old = manager
            .append_assistant_message(source.id, "old".to_string())
            .await
            .expect("old");
        manager
            .append_event(
                source.id,
                SessionEventKind::ProviderContextCompacted {
                    snapshot: provider_snapshot(),
                    compacted_through_sequence: old.sequence,
                },
            )
            .await
            .expect("boundary");
        manager
            .append_assistant_message(source.id, "tail".to_string())
            .await
            .expect("tail");
        let clone = manager.clone_session(source.id, None).await.expect("clone");
        let context = manager
            .model_context_events(clone.session.id)
            .await
            .expect("context");
        assert_eq!(
            context
                .iter()
                .filter(|event| matches!(
                    event.kind,
                    SessionEventKind::ProviderContextCompacted { .. }
                ))
                .count(),
            1
        );
        assert!(!context.iter().any(|event| matches!(&event.kind, SessionEventKind::AssistantMessage { text } if text == "old")));
        assert!(context.iter().any(|event| matches!(&event.kind, SessionEventKind::AssistantMessage { text } if text == "tail")));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn append_event_with_provenance_persists_source_metadata() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("imported".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        let provenance = SessionEventProvenance {
            source_event_id: Some("pi-event-1".to_string()),
            source_timestamp_ms: Some(1_779_483_416_000),
            source_locator: Some("/tmp/pi-session.jsonl".to_string()),
        };
        manager
            .append_event_with_provenance(
                session.id,
                SessionEventKind::AssistantMessage {
                    text: "imported response".to_string(),
                },
                Some(provenance.clone()),
            )
            .await
            .expect("event should append");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let history = restored
            .session_history(session.id)
            .await
            .expect("history should load");
        let imported = history
            .iter()
            .find(|event| matches!(event.kind, SessionEventKind::AssistantMessage { .. }))
            .expect("imported event should exist");

        assert_eq!(imported.provenance.as_ref(), Some(&provenance));
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn legacy_reasoning_events_survive_session_database_restart() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("legacy reasoning".to_owned()),
                test_working_directory(),
            )
            .await
            .expect("session should create");
        for kind in [
            SessionEventKind::AssistantReasoningDelta {
                text: "legacy delta".to_owned(),
            },
            SessionEventKind::AssistantReasoningMessage {
                text: "legacy complete".to_owned(),
            },
        ] {
            manager
                .append_event(session.id, kind)
                .await
                .expect("legacy reasoning should append");
        }
        drop(manager);

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let history = restored
            .session_history(session.id)
            .await
            .expect("history should load");
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::AssistantReasoningDelta { text } if text == "legacy delta"
        )));
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::AssistantReasoningMessage { text } if text == "legacy complete"
        )));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn structured_reasoning_survives_session_database_restart() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("structured reasoning".to_owned()),
                test_working_directory(),
            )
            .await
            .expect("session should create");
        let activity = bcode_session_models::ReasoningActivity {
            activity_id: "reasoning-1".to_owned(),
            order: 3,
            status: bcode_session_models::ReasoningActivityStatus::Interrupted,
            parts: vec![
                bcode_session_models::ReasoningPart {
                    part_id: "summary-0".to_owned(),
                    kind: bcode_session_models::ReasoningContentKind::Summary,
                    role: bcode_session_models::ReasoningContentRole::Milestone,
                    order: 0,
                    text: "First milestone".to_owned(),
                },
                bcode_session_models::ReasoningPart {
                    part_id: "raw-0".to_owned(),
                    kind: bcode_session_models::ReasoningContentKind::Raw,
                    role: bcode_session_models::ReasoningContentRole::Detail,
                    order: 1,
                    text: "Completed detail".to_owned(),
                },
            ],
            opaque: true,
        };
        manager
            .append_assistant_reasoning_activity(session.id, "turn-1".to_owned(), activity.clone())
            .await
            .expect("reasoning activity should append");
        drop(manager);

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let history = restored
            .session_history(session.id)
            .await
            .expect("history should load");
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::AssistantReasoningActivity { turn_id, activity: stored }
                if turn_id == "turn-1" && stored == &activity
        )));
        let encoded = serde_json::to_string(&history).expect("history should serialize");
        assert!(!encoded.contains("encrypted_content"));
        assert!(!encoded.contains("provider_state"));
        assert!(!encoded.contains("encrypted-sentinel-do-not-expose"));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn attach_uses_db_input_history() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("db input history".to_string()),
                test_working_directory(),
            )
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "hello".to_owned())
            .await
            .expect("message should append");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let attachment = restored
            .attach_session_recent(session.id, ClientId::new(), 16)
            .await
            .expect("attach should use DB projections");

        assert_eq!(attachment.input_history.len(), 1);
        let entry = &attachment.input_history[0];
        assert_eq!(entry.sequence, 1);
        assert!(entry.timestamp_ms > 0);
        assert_eq!(entry.text, "hello");

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn attached_subscribers_survive_database_release_and_summary_refresh() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("subscriber continuity".to_string()),
                test_working_directory(),
            )
            .await
            .expect("session should create");
        let client_id = ClientId::new();
        let mut attachment = manager
            .attach_session_recent(session.id, client_id, 8)
            .await
            .expect("session should attach");

        assert!(
            manager
                .release_session_database_resources(session.id)
                .await
                .expect("database release should succeed")
        );
        let appended = manager
            .append_user_message(session.id, ClientId::new(), "after idle".to_owned())
            .await
            .expect("idle session should reload and append");
        let expected_sequence = appended.last().expect("user event").sequence;
        let durable =
            tokio::time::timeout(std::time::Duration::from_secs(1), attachment.events.recv())
                .await
                .expect("original durable subscriber should remain live")
                .expect("durable broker should remain open");
        assert_eq!(durable.sequence, expected_sequence);
        assert!(matches!(
            durable.kind,
            SessionEventKind::UserMessage { ref text, .. } if text == "after idle"
        ));

        let published = manager
            .publish_live_event(
                session.id,
                SessionLiveEventKind::AssistantTextDelta {
                    turn_id: "turn-after-idle".to_owned(),
                    segment_id: "segment-0".to_owned(),
                    segment_order: 0,
                    text: "live after idle".to_owned(),
                },
            )
            .await;
        assert!(published.is_some());
        let live = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            attachment.live_events.recv(),
        )
        .await
        .expect("original live subscriber should remain live")
        .expect("live broker should remain open");
        assert!(matches!(
            live.kind,
            SessionLiveEventKind::AssistantTextDelta { ref text, .. }
                if text == "live after idle"
        ));

        assert_eq!(
            manager
                .session_summary(session.id)
                .await
                .expect("summary")
                .client_count,
            1
        );
        assert!(
            manager
                .detach_session(session.id, client_id)
                .await
                .expect("original client should detach")
        );
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn release_idle_session_resources_drops_loaded_state_but_retains_lease() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent_with_metrics_and_lease_owner(
            &root,
            MetricsRegistry::default(),
            SessionLeaseOwnerContext {
                build_fingerprint: Some("current-test-build".to_string()),
                ..SessionLeaseOwnerContext::default()
            },
        )
        .expect("manager should initialize");
        let session = manager
            .create_session(Some("idle".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        let client_id = ClientId::new();
        manager
            .attach_session_recent(session.id, client_id, 8)
            .await
            .expect("session should attach");

        assert!(
            manager
                .release_session_database_resources(session.id)
                .await
                .expect("explicit database release should succeed"),
            "attached inactive sessions should release their database handle"
        );
        manager
            .session_summary(session.id)
            .await
            .expect("summary should remain available after database release");

        assert!(
            !manager
                .release_idle_session_resources(session.id)
                .await
                .expect("release should check clients"),
            "attached sessions should not release resources"
        );

        manager
            .detach_session(session.id, client_id)
            .await
            .expect("session should detach");
        assert!(
            !manager
                .release_idle_session_resources(session.id)
                .await
                .expect("already released resources should remain released"),
            "the explicit release already dropped the database handle"
        );

        assert!(
            manager.inner.lock().await.leases.contains_key(&session.id),
            "idle resource release must retain compatibility ownership"
        );

        let incompatible = SessionManager::persistent_with_metrics_and_lease_owner(
            &root,
            MetricsRegistry::default(),
            SessionLeaseOwnerContext {
                storage_writer_epoch: Some(
                    crate::lease::CURRENT_SESSION_STORAGE_WRITER_EPOCH.saturating_add(1),
                ),
                build_fingerprint: Some("incompatible-test-build".to_string()),
                ..SessionLeaseOwnerContext::default()
            },
        )
        .expect("incompatible manager should initialize lazily");
        assert!(matches!(
            incompatible.ensure_session_loaded(session.id).await,
            Err(SessionError::Lease(
                crate::lease::SessionLeaseError::OwnedByOtherDaemon { .. }
            ))
        ));

        manager
            .append_user_message(session.id, ClientId::new(), "after release".to_owned())
            .await
            .expect("released session should reload on next use");
        let history = manager
            .session_history(session.id)
            .await
            .expect("history should load after release");
        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. } if text == "after release"
        )));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn dropping_persistent_manager_releases_loaded_session_lease() {
        let root = unique_temp_dir();
        let session_id = {
            let manager = SessionManager::persistent_with_metrics_and_lease_owner(
                &root,
                MetricsRegistry::default(),
                SessionLeaseOwnerContext {
                    storage_writer_epoch: Some(7),
                    build_fingerprint: Some("first-build".to_string()),
                    ..SessionLeaseOwnerContext::default()
                },
            )
            .expect("first manager");
            let session = manager
                .create_session(Some("lease release".to_string()), test_working_directory())
                .await
                .expect("session should create");
            assert!(manager.inner.lock().await.leases.contains_key(&session.id));
            session.id
        };

        let next = SessionManager::persistent_with_metrics_and_lease_owner(
            &root,
            MetricsRegistry::default(),
            SessionLeaseOwnerContext {
                storage_writer_epoch: Some(8),
                build_fingerprint: Some("next-build".to_string()),
                ..SessionLeaseOwnerContext::default()
            },
        )
        .expect("next manager");
        next.ensure_session_loaded(session_id)
            .await
            .expect("manager drop must release lease");
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn restored_model_context_uses_relevant_canonical_db_events_without_checkpoint() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("model context".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "first".to_owned())
            .await
            .expect("first message should append");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let appended = restored
            .append_user_message(session.id, ClientId::new(), "carry on".to_owned())
            .await
            .expect("carry-on message should append");
        let user_sequence = appended
            .last()
            .expect("user event should be returned")
            .sequence;
        restored
            .append_model_turn_started(session.id, "turn-1".to_owned())
            .await
            .expect("turn start should append");

        let context = restored
            .model_context_events(session.id)
            .await
            .expect("model context should load from canonical DB events");

        assert!(context.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. }
                if event.sequence == user_sequence && text == "carry on"
        )));
        assert!(context.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ModelTurnStarted { turn_id } if turn_id == "turn-1"
        )));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn restored_session_events_range_reads_inclusive_sequences_from_db() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("range".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        for index in 0..5 {
            manager
                .append_user_message(session.id, ClientId::new(), format!("message {index}"))
                .await
                .expect("message should append");
        }

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let events = restored
            .session_events_range(session.id, 2, 4, 8)
            .await
            .expect("events range should load");

        assert_eq!(events.len(), 3);
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert!(matches!(
            &events[0].kind,
            SessionEventKind::UserMessage { text, .. } if text == "message 1"
        ));
        assert!(matches!(
            &events[2].kind,
            SessionEventKind::UserMessage { text, .. } if text == "message 3"
        ));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn restored_projection_windows_page_bidirectionally_without_overlap() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("projection pages".to_string()),
                test_working_directory(),
            )
            .await
            .expect("session should be created");
        for index in 0..6 {
            manager
                .append_user_message(session.id, ClientId::new(), format!("message {index}"))
                .await
                .expect("message should append");
        }
        let request = |anchor, direction| ProjectionWindowRequest {
            projection: SessionProjectionKind::Transcript,
            anchor,
            direction,
            target: ProjectionWindowTarget {
                min_items: Some(2),
                min_estimated_rows: None,
                min_bytes: None,
                width_columns: Some(80),
            },
            limits: ProjectionWindowLimits {
                max_items: 2,
                max_events_scanned: 8,
                max_bytes: 4096,
            },
        };

        let latest = manager
            .session_projection_window(
                session.id,
                request(
                    ProjectionWindowAnchor::Latest,
                    ProjectionWindowDirection::Backward,
                ),
            )
            .await
            .expect("latest window");
        assert_eq!(
            latest.source_range,
            Some(bcode_session_models::ProjectionSourceRange {
                start_sequence: 5,
                end_sequence: 6,
            })
        );
        assert!(latest.has_older);
        assert!(!latest.has_newer);

        let older = manager
            .session_projection_window(
                session.id,
                request(
                    ProjectionWindowAnchor::BeforeSequence(5),
                    ProjectionWindowDirection::Backward,
                ),
            )
            .await
            .expect("older window");
        assert_eq!(
            older.source_range,
            Some(bcode_session_models::ProjectionSourceRange {
                start_sequence: 3,
                end_sequence: 4,
            })
        );
        assert!(older.has_older);
        assert!(older.has_newer);

        let newer = manager
            .session_projection_window(
                session.id,
                request(
                    ProjectionWindowAnchor::AfterSequence(4),
                    ProjectionWindowDirection::Forward,
                ),
            )
            .await
            .expect("newer window");
        assert_eq!(newer.source_range, latest.source_range);
        assert!(newer.has_older);
        assert!(!newer.has_newer);

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn restored_session_events_range_respects_max_events_and_empty_ranges() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("range-limit".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        for index in 0..5 {
            manager
                .append_user_message(session.id, ClientId::new(), format!("message {index}"))
                .await
                .expect("message should append");
        }

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let limited = restored
            .session_events_range(session.id, 1, 5, 2)
            .await
            .expect("events range should load");
        let empty = restored
            .session_events_range(session.id, 5, 1, 8)
            .await
            .expect("empty reversed range should load");

        assert_eq!(limited.len(), 2);
        assert_eq!(
            limited
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(empty.is_empty());

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn attach_session_recent_avoids_full_replay() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("recent".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        for index in 0..205 {
            manager
                .append_user_message(session.id, ClientId::new(), format!("message {index}"))
                .await
                .expect("message should append");
        }

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let attachment = restored
            .attach_session_recent(session.id, ClientId::new(), 1)
            .await
            .expect("recent attach should succeed");

        assert_eq!(attachment.history.len(), 1);
        assert_eq!(attachment.session.name.as_deref(), Some("recent"));
        assert!(matches!(
            &attachment.history[0].kind,
            SessionEventKind::UserMessage { text, .. } if text == "message 204"
        ));
        assert_eq!(attachment.input_history.len(), 205);
        assert_eq!(
            attachment
                .input_history
                .first()
                .map(|entry| entry.text.as_str()),
            Some("message 0")
        );
        assert_eq!(
            attachment
                .input_history
                .last()
                .map(|entry| entry.text.as_str()),
            Some("message 204")
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn catalog_discovers_every_migration_fixture_without_mutation() {
        let root = unique_temp_dir();
        let fixtures = [
            (
                "future-schema-v40.json",
                include_str!("../fixtures/migrations/future-schema-v40.json"),
            ),
            (
                "future-schema-v41.json",
                include_str!("../fixtures/migrations/future-schema-v41.json"),
            ),
            (
                "future-schema-v42.json",
                include_str!("../fixtures/migrations/future-schema-v42.json"),
            ),
            (
                "malformed-json-v39.json",
                include_str!("../fixtures/migrations/malformed-json-v39.json"),
            ),
            (
                "mismatched-session-id-v39.json",
                include_str!("../fixtures/migrations/mismatched-session-id-v39.json"),
            ),
            (
                "plugin-status-note-v39.json",
                include_str!("../fixtures/migrations/plugin-status-note-v39.json"),
            ),
            (
                "unknown-future-event-kind-v39.json",
                include_str!("../fixtures/migrations/unknown-future-event-kind-v39.json"),
            ),
            (
                "sequence-gap-v39.jsonl",
                include_str!("../fixtures/migrations/sequence-gap-v39.jsonl"),
            ),
        ];
        let mut expected = BTreeMap::new();

        for (fixture_name, fixture) in fixtures {
            let session_id = SessionId::new();
            let db = db::SessionDb::open_turso_in_root(session_id, &root)
                .await
                .expect("fixture session DB should initialize");
            for (line_index, payload) in fixture.lines().enumerate() {
                let parsed = serde_json::from_str::<serde_json::Value>(payload).ok();
                let sequence = parsed
                    .as_ref()
                    .and_then(|value| value["sequence"].as_u64())
                    .unwrap_or_else(|| u64::try_from(line_index).expect("line index should fit"));
                let event_type = parsed
                    .as_ref()
                    .and_then(|value| value["kind"].as_object())
                    .and_then(|kind| kind.keys().next())
                    .map_or_else(|| "malformed_fixture".to_owned(), Clone::clone);
                let schema_version = parsed
                    .as_ref()
                    .and_then(|value| value["schema_version"].as_i64())
                    .and_then(|version| i32::try_from(version).ok())
                    .unwrap_or_else(|| i32::from(CURRENT_SESSION_EVENT_SCHEMA_VERSION));
                let created_at_ms = parsed
                    .as_ref()
                    .and_then(|value| value["timestamp_ms"].as_i64())
                    .unwrap_or(0);
                db.database()
                    .insert("events")
                    .value(
                        "event_seq",
                        switchy::database::DatabaseValue::Int64(
                            i64::try_from(sequence).expect("sequence should fit"),
                        ),
                    )
                    .value("event_type", event_type)
                    .value(
                        "schema_version",
                        switchy::database::DatabaseValue::Int32(schema_version),
                    )
                    .value(
                        "created_at_ms",
                        switchy::database::DatabaseValue::Int64(created_at_ms),
                    )
                    .value("payload", payload)
                    .execute(db.database())
                    .await
                    .unwrap_or_else(|error| {
                        panic!("fixture {fixture_name} row should insert: {error}")
                    });
            }
            drop(db);
            std::thread::sleep(Duration::from_millis(20));
            expected.insert(
                session_id,
                (fixture_name, session_database_files(&root, session_id)),
            );
        }

        let catalog = SessionStore::new(&root)
            .load_catalog()
            .expect("fixture catalog should load");
        assert_eq!(catalog.len(), expected.len());
        for (session_id, (fixture_name, before)) in expected {
            let state = catalog
                .get(&session_id)
                .unwrap_or_else(|| panic!("fixture {fixture_name} should be discoverable"));
            assert_eq!(state.load_status, SessionLoadStatusKind::SummaryOnly);
            assert_eq!(
                session_database_files(&root, session_id),
                before,
                "catalog discovery must not mutate fixture {fixture_name}"
            );
        }

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn normal_open_does_not_decode_canonical_events() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(
                Some("decode free open".to_owned()),
                test_working_directory(),
            )
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "canonical payload".to_owned())
            .await
            .expect("message should append");

        let db = db::SessionDb::open_existing_turso_in_root(session.id, &root)
            .await
            .expect("session DB should open");
        db.database()
            .update("events")
            .value("payload", "not valid JSON")
            .where_eq("event_seq", switchy::database::DatabaseValue::Int64(1))
            .execute(db.database())
            .await
            .expect("canonical payload should corrupt");
        drop(db);
        manager
            .inner
            .lock()
            .await
            .sessions
            .remove(&session.id)
            .expect("cached actor should exist");
        manager.inner.lock().await.leases.remove(&session.id);

        let summary = manager
            .session_summary(session.id)
            .await
            .expect("normal open must use derived state without decoding canonical events");
        assert_eq!(summary.name.as_deref(), Some("decode free open"));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn concurrent_same_session_appends_have_contiguous_sequences() {
        let root = unique_temp_dir();
        let manager = std::sync::Arc::new(
            SessionManager::persistent(&root).expect("manager should initialize"),
        );
        let session = manager
            .create_session(Some("concurrent".to_string()), test_working_directory())
            .await
            .expect("session should create");

        let mut tasks = Vec::new();
        for index in 0..16 {
            let manager = std::sync::Arc::clone(&manager);
            tasks.push(tokio::spawn(async move {
                manager
                    .append_event(
                        session.id,
                        SessionEventKind::SystemMessage {
                            text: format!("message {index}"),
                        },
                    )
                    .await
                    .expect("event should append")
            }));
        }

        let mut sequences = Vec::new();
        for task in tasks {
            sequences.push(task.await.expect("task should join").sequence);
        }
        sequences.sort_unstable();
        assert_eq!(sequences, (1..=16).collect::<Vec<_>>());

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn concurrent_same_session_appends_across_managers_have_contiguous_sequences() {
        let root = unique_temp_dir();
        let creator = SessionManager::persistent(&root).expect("manager should initialize");
        let session = creator
            .create_session(Some("cross-manager".to_string()), test_working_directory())
            .await
            .expect("session should create");
        drop(creator);

        let first = std::sync::Arc::new(
            SessionManager::persistent(&root).expect("first manager should restore"),
        );
        let second = std::sync::Arc::new(
            SessionManager::persistent(&root).expect("second manager should restore"),
        );

        let mut tasks = Vec::new();
        for index in 0..16 {
            let manager = if index % 2 == 0 {
                std::sync::Arc::clone(&first)
            } else {
                std::sync::Arc::clone(&second)
            };
            tasks.push(tokio::spawn(async move {
                manager
                    .append_event(
                        session.id,
                        SessionEventKind::SystemMessage {
                            text: format!("message {index}"),
                        },
                    )
                    .await
                    .expect("event should append")
            }));
        }

        let mut sequences = Vec::new();
        for task in tasks {
            sequences.push(task.await.expect("task should join").sequence);
        }
        sequences.sort_unstable();
        assert_eq!(sequences, (1..=16).collect::<Vec<_>>());

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn provider_snapshot_opaque_context_survives_manager_restart() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("opaque restart".to_string()), test_working_directory())
            .await
            .expect("session should create");
        let snapshot = bcode_session_models::ProviderContextSnapshot {
            format_version: 7,
            request_fingerprint: Some("fingerprint".to_string()),
            request_id: Some("request".to_string()),
            provider_plugin_id: "provider".to_string(),
            model_id: "model".to_string(),
            compatibility_key: "surface".to_string(),
            auth_profile: Some("profile".to_string()),
            origin: bcode_session_models::ProviderContextSnapshotOrigin::Explicit,
            messages_json: r#"[{"opaque":"ciphertext"}]"#.to_string(),
            portable_summary: "portable fallback".to_string(),
        };
        manager
            .append_provider_context_compacted(session.id, snapshot.clone(), 0)
            .await
            .expect("snapshot should persist");
        drop(manager);

        let restored = SessionManager::persistent_lazy(&root);
        let context = restored
            .model_context_events(session.id)
            .await
            .expect("context should reload");

        assert!(context.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ProviderContextCompacted { snapshot: actual, .. }
                if actual == &snapshot
        )));
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn catalog_status_subscription_reports_loaded() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        manager
            .create_session(Some("catalog".to_string()), test_working_directory())
            .await
            .expect("session should create");

        let restored = SessionManager::persistent_lazy(&root);
        let mut status = restored.subscribe_catalog_status();
        assert_eq!(*status.borrow(), super::CatalogLoadStatus::NotStarted);
        restored.start_catalog_load();
        loop {
            if matches!(*status.borrow(), super::CatalogLoadStatus::Loaded) {
                break;
            }
            status.changed().await.expect("status should change");
        }
        assert_eq!(
            restored
                .cached_sessions(&test_working_directory())
                .await
                .len(),
            1
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn concurrent_duplicate_turn_admission_is_atomic() {
        let manager = SessionManager::default();
        let session = manager
            .create_session(Some("idempotency".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        let metadata = bcode_session_models::TurnAdmissionMetadata {
            origin: Some(bcode_session_models::TurnOrigin {
                producer: "test.producer".to_string(),
                correlation_id: None,
                display_label: None,
            }),
            idempotency_key: Some("operation-1".to_string()),
            ..bcode_session_models::TurnAdmissionMetadata::default()
        };

        let first = manager.admit_turn(
            session.id,
            ClientId::new(),
            "first".to_string(),
            metadata.clone(),
        );
        let second =
            manager.admit_turn(session.id, ClientId::new(), "second".to_string(), metadata);
        let (first, second) = tokio::join!(first, second);
        let first = first.expect("first concurrent admission");
        let second = second.expect("second concurrent admission");

        let accepted = [&first, &second]
            .into_iter()
            .filter(|admission| {
                matches!(admission, bcode_session_models::TurnAdmission::Accepted(_))
            })
            .count();
        let existing = [&first, &second]
            .into_iter()
            .filter(|admission| {
                matches!(admission, bcode_session_models::TurnAdmission::Existing(_))
            })
            .count();
        assert_eq!((accepted, existing), (1, 1));
        let first_receipt = match first {
            bcode_session_models::TurnAdmission::Accepted(receipt)
            | bcode_session_models::TurnAdmission::Existing(receipt) => receipt,
            other => panic!("unexpected admission: {other:?}"),
        };
        let second_receipt = match second {
            bcode_session_models::TurnAdmission::Accepted(receipt)
            | bcode_session_models::TurnAdmission::Existing(receipt) => receipt,
            other => panic!("unexpected admission: {other:?}"),
        };
        assert_eq!(first_receipt, second_receipt);
    }

    #[tokio::test]
    async fn idempotent_turn_admission_returns_existing_receipt_without_duplicate_event() {
        let manager = SessionManager::default();
        let session = manager
            .create_session(Some("idempotency".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        let metadata = bcode_session_models::TurnAdmissionMetadata {
            origin: Some(bcode_session_models::TurnOrigin {
                producer: "test.producer".to_string(),
                correlation_id: Some("run-1".to_string()),
                display_label: Some("Background pass 1".to_string()),
            }),
            priority: bcode_session_models::TurnPriority::Background,
            idempotency_key: Some("operation-1".to_string()),
            execution: bcode_session_models::TurnExecutionOptions {
                tools: bcode_session_models::TurnToolPolicy::Disabled,
                ..bcode_session_models::TurnExecutionOptions::default()
            },
        };

        let first = manager
            .admit_turn(
                session.id,
                ClientId::new(),
                "prompt".to_string(),
                metadata.clone(),
            )
            .await
            .expect("first admission should succeed");
        let duplicate = manager
            .admit_turn(
                session.id,
                ClientId::new(),
                "different text must not append".to_string(),
                metadata,
            )
            .await
            .expect("duplicate admission should succeed");

        let bcode_session_models::TurnAdmission::Accepted(receipt) = first else {
            panic!("first admission should be accepted");
        };
        assert_eq!(
            duplicate,
            bcode_session_models::TurnAdmission::Existing(receipt)
        );
        let history = manager.session_history(session.id).await.expect("history");
        assert_eq!(
            history
                .iter()
                .filter(|event| matches!(event.kind, SessionEventKind::UserMessage { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn persisted_idempotent_turn_receipt_survives_manager_restart() {
        let root = unique_temp_dir();
        let session_id;
        let expected;
        let metadata = bcode_session_models::TurnAdmissionMetadata {
            origin: Some(bcode_session_models::TurnOrigin {
                producer: "test.producer".to_string(),
                correlation_id: None,
                display_label: None,
            }),
            idempotency_key: Some("operation-1".to_string()),
            ..bcode_session_models::TurnAdmissionMetadata::default()
        };
        {
            let manager = SessionManager::persistent(&root).expect("manager");
            let session = manager
                .create_session(Some("idempotency".to_string()), test_working_directory())
                .await
                .expect("session");
            session_id = session.id;
            expected = manager
                .admit_turn(
                    session_id,
                    ClientId::new(),
                    "prompt".to_string(),
                    metadata.clone(),
                )
                .await
                .expect("admission");
        }

        let restored = SessionManager::persistent(&root).expect("restored manager");
        let duplicate = restored
            .admit_turn(
                session_id,
                ClientId::new(),
                "different".to_string(),
                metadata,
            )
            .await
            .expect("duplicate");
        let bcode_session_models::TurnAdmission::Accepted(receipt) = expected else {
            panic!("first admission should be accepted");
        };
        assert_eq!(
            duplicate,
            bcode_session_models::TurnAdmission::Existing(receipt)
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn generic_turn_origin_is_persisted_on_the_ordinary_user_message_path() {
        let manager = SessionManager::default();
        let session = manager
            .create_session(Some("origin".to_string()), test_working_directory())
            .await
            .expect("session should be created");
        let origin = bcode_session_models::TurnOrigin {
            producer: "test.producer".to_string(),
            correlation_id: Some("operation-1".to_string()),
            display_label: Some("Background pass 1".to_string()),
        };

        let events = manager
            .append_user_message_with_origin(
                session.id,
                ClientId::new(),
                "ordinary prompt".to_string(),
                Some(origin.clone()),
            )
            .await
            .expect("message should append");

        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(SessionEventKind::UserMessage {
                text,
                admission:
                    bcode_session_models::TurnAdmissionMetadata {
                        origin: Some(actual),
                        ..
                    },
                ..
            }) if text == "ordinary prompt" && actual == &origin
        ));
    }

    #[tokio::test]
    async fn unnamed_session_uses_first_prompt_as_title() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(None, test_working_directory())
            .await
            .expect("session should be created");

        let events = manager
            .append_user_message(
                session.id,
                ClientId::new(),
                "# Fix session selection UX\n\nPlease make this nicer".to_string(),
            )
            .await
            .expect("message should append");

        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0].kind,
            SessionEventKind::SessionRenamed { name } if name.as_deref() == Some("Fix session selection UX")
        ));
        let sessions = manager.list_sessions(&test_working_directory()).await;
        assert_eq!(
            sessions[0].name.as_deref(),
            Some("Fix session selection UX")
        );

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let restored_sessions = restored.list_sessions(&test_working_directory()).await;
        assert_eq!(
            restored_sessions[0].name.as_deref(),
            Some("Fix session selection UX")
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn explicit_session_name_is_not_replaced_by_first_prompt() {
        let manager = SessionManager::default();
        let session = manager
            .create_session(Some("Manual title".to_string()), test_working_directory())
            .await
            .expect("session should be created");

        let events = manager
            .append_user_message(session.id, ClientId::new(), "Different title".to_string())
            .await
            .expect("message should append");

        assert_eq!(events.len(), 1);
        let sessions = manager.list_sessions(&test_working_directory()).await;
        assert_eq!(sessions[0].name.as_deref(), Some("Manual title"));
    }

    #[tokio::test]
    async fn rename_session_restores_latest_name() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("Old title".to_string()), test_working_directory())
            .await
            .expect("session should be created");

        manager
            .rename_session(session.id, Some("  New   title  ".to_string()))
            .await
            .expect("session should rename");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let sessions = restored.list_sessions(&test_working_directory()).await;
        assert_eq!(sessions[0].name.as_deref(), Some("New title"));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn catalog_listing_remains_lease_free_with_an_incompatible_live_owner() {
        let root = unique_temp_dir();
        let writer = SessionManager::persistent_with_metrics_and_lease_owner(
            &root,
            MetricsRegistry::default(),
            SessionLeaseOwnerContext {
                storage_writer_epoch: Some(lease::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
                build_fingerprint: Some("current-writer".to_owned()),
                ..SessionLeaseOwnerContext::default()
            },
        )
        .expect("writer manager should initialize");
        let session = writer
            .create_session(Some("catalog-only".to_owned()), test_working_directory())
            .await
            .expect("session should create");
        assert_eq!(
            lease::active_session_owners(&root, session.id)
                .expect("owners should be readable")
                .len(),
            1,
            "the loaded writer should hold exactly one session lease"
        );

        let passive_reader = SessionManager::persistent_with_metrics_and_lease_owner(
            &root,
            MetricsRegistry::default(),
            SessionLeaseOwnerContext {
                storage_writer_epoch: Some(lease::CURRENT_SESSION_STORAGE_WRITER_EPOCH - 1),
                build_fingerprint: Some("incompatible-passive-reader".to_owned()),
                ..SessionLeaseOwnerContext::default()
            },
        )
        .expect("catalog loading must not acquire a session lease");
        assert!(
            passive_reader
                .all_session_summaries()
                .await
                .iter()
                .any(|summary| summary.id == session.id),
            "passive catalog listing should discover the owned session"
        );
        assert_eq!(
            lease::active_session_owners(&root, session.id)
                .expect("owners should remain readable")
                .len(),
            1,
            "passive discovery must not create an owner record"
        );

        drop(passive_reader);
        drop(writer);
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn canonical_database_remains_visible_without_manifest_or_catalog() {
        let root = unique_temp_dir();
        let session_id = {
            let manager = SessionManager::persistent_with_metrics_and_lease_owner(
                &root,
                MetricsRegistry::default(),
                SessionLeaseOwnerContext {
                    build_fingerprint: Some("discovery-build".to_owned()),
                    ..SessionLeaseOwnerContext::default()
                },
            )
            .expect("manager should initialize");
            manager
                .create_session(Some("canonical".to_owned()), test_working_directory())
                .await
                .expect("session")
                .id
        };
        std::fs::remove_file(db::session_dir_path(&root, session_id).join("manifest.json"))
            .expect("remove manifest");
        std::fs::remove_file(db::global_catalog_db_path(&root)).expect("remove catalog");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        assert!(
            restored
                .all_session_summaries()
                .await
                .iter()
                .any(|session| session.id == session_id),
            "canonical session directory must not be hidden by missing caches"
        );
        restored
            .require_write_readiness(session_id)
            .await
            .expect("canonical database should load");
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn unreadable_manifest_and_catalog_cache_do_not_hide_canonical_database() {
        let root = unique_temp_dir();
        let session_id = {
            let manager = SessionManager::persistent_with_metrics_and_lease_owner(
                &root,
                MetricsRegistry::default(),
                SessionLeaseOwnerContext {
                    build_fingerprint: Some("corrupt-cache".to_owned()),
                    ..SessionLeaseOwnerContext::default()
                },
            )
            .expect("manager should initialize");
            manager
                .create_session(Some("canonical".to_owned()), test_working_directory())
                .await
                .expect("session")
                .id
        };
        std::fs::write(
            db::session_dir_path(&root, session_id).join("manifest.json"),
            b"not valid JSON",
        )
        .expect("corrupt manifest");
        let catalog = db::global_catalog_db_path(&root);
        std::fs::write(&catalog, b"not a database").expect("corrupt catalog");

        let restored = SessionManager::persistent_with_metrics_and_lease_owner(
            &root,
            MetricsRegistry::default(),
            SessionLeaseOwnerContext {
                build_fingerprint: Some("corrupt-cache".to_owned()),
                ..SessionLeaseOwnerContext::default()
            },
        )
        .expect("derived catalog damage must not fail discovery");
        let entry = restored
            .all_session_catalog_entries()
            .await
            .into_iter()
            .find(|entry| entry.summary.id == session_id)
            .expect("canonical session should remain visible");
        assert_eq!(entry.load_status, SessionCatalogLoadStatus::SummaryOnly);
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn bounded_manifest_lists_known_legacy_format_without_opening_session_database() {
        let root = unique_temp_dir();
        let session_id = SessionId::new();
        let session_dir = db::session_dir_path(&root, session_id);
        std::fs::create_dir_all(&session_dir).expect("session dir");
        std::fs::write(
            session_dir.join("manifest.json"),
            serde_json::json!({
                "schema_version": 1,
                "summary": {
                    "id": session_id,
                    "name": "legacy session",
                    "explicit_name": "legacy session",
                    "derived_title": null,
                    "title_source": "explicit",
                    "client_count": 0,
                    "created_at_ms": 1,
                    "updated_at_ms": 2,
                    "working_directory": root,
                    "import": null,
                    "fork": null
                }
            })
            .to_string(),
        )
        .expect("old manifest");
        let legacy_db_path = db::session_db_path(&root, session_id);
        std::fs::write(&legacy_db_path, b"not a database").expect("database sentinel");
        let current_session_id = SessionId::new();
        let current_session_dir = db::session_dir_path(&root, current_session_id);
        std::fs::create_dir_all(&current_session_dir).expect("current session dir");
        std::fs::write(
            current_session_dir.join("manifest.json"),
            serde_json::json!({
                "schema_version": SESSION_MANIFEST_SCHEMA_VERSION,
                "session_format": {
                    "family": SESSION_FORMAT_FAMILY,
                    "epoch": CURRENT_SESSION_FORMAT_EPOCH
                },
                "summary": {
                    "id": current_session_id,
                    "name": "current session",
                    "explicit_name": "current session",
                    "derived_title": null,
                    "title_source": "explicit",
                    "client_count": 0,
                    "created_at_ms": 3,
                    "updated_at_ms": 4,
                    "working_directory": root,
                    "import": null,
                    "fork": null
                }
            })
            .to_string(),
        )
        .expect("current manifest");
        let current_db_path = db::session_db_path(&root, current_session_id);
        std::fs::write(&current_db_path, b"also not a database").expect("database sentinel");
        let store = SessionStore::new(&root);
        let summary = store
            .load_session_manifest(session_id)
            .expect("known legacy manifest should load")
            .expect("manifest summary");
        assert_eq!(summary.id, session_id);
        assert_eq!(summary.display_title(), "legacy session");
        let catalog = store.load_catalog().expect("bounded catalog load");
        assert_eq!(catalog.len(), 2);
        assert!(catalog.contains_key(&session_id));
        assert!(catalog.contains_key(&current_session_id));

        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        manager
            .wait_catalog_loaded()
            .await
            .expect("catalog should load");
        let sessions = manager.list_sessions(&root).await;
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|summary| summary.id == session_id));
        assert!(
            sessions
                .iter()
                .any(|summary| summary.id == current_session_id)
        );
        let entries = manager.all_session_catalog_entries().await;
        assert!(
            entries
                .iter()
                .all(|entry| matches!(entry.load_status, SessionCatalogLoadStatus::SummaryOnly))
        );
        assert_eq!(
            std::fs::read(&legacy_db_path).expect("legacy database sentinel"),
            b"not a database",
            "passive listing must not open or mutate the legacy database"
        );
        assert_eq!(
            std::fs::read(&current_db_path).expect("current database sentinel"),
            b"also not a database",
            "passive listing must not open or mutate the current database"
        );
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[test]
    fn bounded_manifest_rejects_unknown_and_inconsistent_metadata() {
        let root = unique_temp_dir();
        let future_session_id = SessionId::new();
        let future_session_dir = db::session_dir_path(&root, future_session_id);
        std::fs::create_dir_all(&future_session_dir).expect("future session dir");
        std::fs::write(
            future_session_dir.join("manifest.json"),
            serde_json::json!({
                "schema_version": SESSION_MANIFEST_SCHEMA_VERSION + 1,
                "summary": { "id": future_session_id }
            })
            .to_string(),
        )
        .expect("future manifest");
        let future_db_path = db::session_db_path(&root, future_session_id);
        std::fs::write(&future_db_path, b"future database sentinel").expect("database sentinel");

        let mismatched_session_id = SessionId::new();
        let mismatched_session_dir = db::session_dir_path(&root, mismatched_session_id);
        std::fs::create_dir_all(&mismatched_session_dir).expect("mismatched session dir");
        std::fs::write(
            mismatched_session_dir.join("manifest.json"),
            serde_json::json!({
                "schema_version": 1,
                "summary": {
                    "id": SessionId::new(),
                    "name": null,
                    "client_count": 0,
                    "created_at_ms": 0,
                    "updated_at_ms": 0,
                    "working_directory": root
                }
            })
            .to_string(),
        )
        .expect("mismatched manifest");
        let mismatched_db_path = db::session_db_path(&root, mismatched_session_id);
        std::fs::write(&mismatched_db_path, b"mismatched database sentinel")
            .expect("database sentinel");

        let store = SessionStore::new(&root);
        assert!(
            store
                .load_session_manifest(future_session_id)
                .expect_err("future manifest should fail closed")
                .to_string()
                .contains("unsupported session manifest schema version")
        );
        assert!(
            store
                .load_session_manifest(mismatched_session_id)
                .expect_err("mismatched manifest should fail closed")
                .to_string()
                .contains("session manifest id mismatch")
        );
        let catalog = store.load_catalog().expect("bounded catalog load");
        assert_eq!(catalog.len(), 2);
        for session_id in [future_session_id, mismatched_session_id] {
            let state = catalog
                .get(&session_id)
                .expect("canonical fallback should remain visible");
            assert_eq!(state.load_status, SessionLoadStatusKind::SummaryOnly);
            assert_eq!(state.summary.id, session_id);
            assert_eq!(state.summary.name, None);
        }
        assert_eq!(
            std::fs::read(&future_db_path).expect("future database sentinel"),
            b"future database sentinel"
        );
        assert_eq!(
            std::fs::read(&mismatched_db_path).expect("mismatched database sentinel"),
            b"mismatched database sentinel"
        );
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn persistent_sessions_write_manifest_and_canonical_catalog() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent_with_metrics_and_lease_owner(
            &root,
            MetricsRegistry::default(),
            SessionLeaseOwnerContext {
                build_fingerprint: Some("test-build".to_string()),
                ..SessionLeaseOwnerContext::default()
            },
        )
        .expect("manager should initialize");
        let session = manager
            .create_session(Some("manifested".to_string()), test_working_directory())
            .await
            .expect("session should create");

        let manifest_path = root.join(session.id.to_string()).join("manifest.json");
        assert!(manifest_path.exists());
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).expect("manifest should read"))
                .expect("manifest should decode");
        assert_eq!(manifest["schema_version"], SESSION_MANIFEST_SCHEMA_VERSION);
        assert_eq!(manifest["session_format"]["family"], SESSION_FORMAT_FAMILY);
        assert_eq!(
            manifest["session_format"]["epoch"],
            CURRENT_SESSION_FORMAT_EPOCH
        );
        assert!(
            db::global_catalog_db_path(&root).exists(),
            "all current writers should use the canonical global catalog"
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn catalog_load_uses_global_catalog_without_opening_session_db() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("global catalog".to_string()), test_working_directory())
            .await
            .expect("session should create");
        drop(manager);
        let session_db = db::session_db_path(&root, session.id);
        let hidden_db = session_db.with_extension("db.hidden");
        std::fs::rename(&session_db, &hidden_db).expect("hide session db");

        let restored = SessionManager::persistent_lazy(&root);
        restored.start_catalog_load();
        let mut status = restored.subscribe_catalog_status();
        loop {
            if matches!(*status.borrow(), super::CatalogLoadStatus::Loaded) {
                break;
            }
            status.changed().await.expect("status should change");
        }
        let sessions = restored.cached_sessions(&test_working_directory()).await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session.id);
        assert_eq!(sessions[0].name.as_deref(), Some("global catalog"));

        std::fs::rename(hidden_db, session_db).expect("restore session db");
        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn list_sessions_orders_by_latest_activity() {
        let manager = SessionManager::default();
        let older = manager
            .create_session(Some("older".to_string()), test_working_directory())
            .await
            .expect("older session should create");
        let newer = manager
            .create_session(Some("newer".to_string()), test_working_directory())
            .await
            .expect("newer session should create");

        let sessions = manager.list_sessions(&test_working_directory()).await;
        assert_eq!(sessions[0].id, newer.id);
        assert_eq!(sessions[1].id, older.id);

        manager
            .append_user_message(older.id, ClientId::new(), "wake older".to_string())
            .await
            .expect("message should append");

        let sessions = manager.list_sessions(&test_working_directory()).await;
        assert_eq!(sessions[0].id, older.id);
        assert_eq!(sessions[1].id, newer.id);
    }

    #[tokio::test]
    async fn restored_sessions_order_by_index_activity() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let older = manager
            .create_session(Some("older".to_string()), test_working_directory())
            .await
            .expect("older session should create");
        let newer = manager
            .create_session(Some("newer".to_string()), test_working_directory())
            .await
            .expect("newer session should create");

        manager
            .append_user_message(older.id, ClientId::new(), "wake older".to_string())
            .await
            .expect("message should append");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let sessions = restored.list_sessions(&test_working_directory()).await;
        assert_eq!(sessions[0].id, older.id);
        assert_eq!(sessions[1].id, newer.id);
        assert!(sessions[0].updated_at_ms >= sessions[0].created_at_ms);

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn delete_session_removes_persisted_history() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("Delete me".to_string()), test_working_directory())
            .await
            .expect("session should be created");

        manager
            .delete_session(session.id)
            .await
            .expect("session should delete");

        assert!(
            manager
                .list_sessions(&test_working_directory())
                .await
                .is_empty()
        );
        let restored = SessionManager::persistent(&root).expect("manager should restore");
        assert!(
            restored
                .list_sessions(&test_working_directory())
                .await
                .is_empty()
        );

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn active_only_presentation_is_rejected_before_durable_append() {
        let manager = SessionManager::default();
        let session = manager
            .create_session(
                Some("active-only presentation".to_owned()),
                test_working_directory(),
            )
            .await
            .expect("session");
        let result = manager
            .append_tool_invocation_result(
                session.id,
                bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call".to_owned(),
                    model_output: "done".to_owned(),
                    is_error: false,
                    presentation: Some(bcode_session_models::ToolPresentationUpdate {
                        invocation_id: "call".to_owned(),
                        producer_id: "producer".to_owned(),
                        generation: 0,
                        revision: 1,
                        identity: bcode_session_models::ToolPresentationIdentity::Primary,
                        retention: bcode_session_models::ToolPresentationRetention::ActiveOnly,
                        schema: "example.active-only".to_owned(),
                        schema_version: 1,
                        artifact: None,
                        payload: serde_json::json!({"must_not_persist": true}),
                    }),
                    result: None,
                },
            )
            .await;
        assert!(
            matches!(result, Err(SessionError::EventSerialization(message)) if message.contains("active-only"))
        );
        assert!(
            !manager
                .session_history(session.id)
                .await
                .expect("history")
                .iter()
                .any(|event| matches!(
                    event.kind,
                    SessionEventKind::ToolInvocationResultRecorded { .. }
                ))
        );
    }

    #[tokio::test]
    async fn transient_contribution_is_rejected_before_durable_append() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("persistent manager");
        let session = manager
            .create_session(
                Some("transient contribution".to_owned()),
                test_working_directory(),
            )
            .await
            .expect("session");
        let result = manager
            .append_event(
                session.id,
                SessionEventKind::ToolContribution {
                    event: bcode_session_models::ToolContributionEvent {
                        invocation_id: "call".to_owned(),
                        contribution_id: "transient".to_owned(),
                        sequence: 1,
                        producer_id: "producer".to_owned(),
                        schema: "example.transient".to_owned(),
                        schema_version: 1,
                        operation: bcode_session_models::ToolContributionOperation::Upsert,
                        persistence: bcode_session_models::ToolContributionPersistence::Transient,
                        artifact: None,
                        payload: serde_json::json!({"must_not_persist": true}),
                    },
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(SessionError::LiveEventPersistenceRejected {
                event_kind: "tool_contribution"
            })
        ));
        assert!(
            !manager
                .session_history(session.id)
                .await
                .expect("history")
                .iter()
                .any(|event| matches!(event.kind, SessionEventKind::ToolContribution { .. }))
        );
        std::fs::remove_dir_all(root).expect("temp dir cleanup");
    }

    #[tokio::test]
    async fn transient_placed_contribution_is_rejected_before_durable_append() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("persistent manager");
        let session = manager
            .create_session(
                Some("transient placed contribution".to_owned()),
                test_working_directory(),
            )
            .await
            .expect("session");
        let result = manager
            .append_event(
                session.id,
                SessionEventKind::ToolContributionPlaced {
                    envelope: bcode_session_models::ToolContributionEnvelope::new(
                        bcode_session_models::ToolContributionPlacement::Progress,
                        bcode_session_models::ToolContributionEvent {
                            invocation_id: "call".to_owned(),
                            contribution_id: "transient".to_owned(),
                            sequence: 1,
                            producer_id: "producer".to_owned(),
                            schema: "example.transient".to_owned(),
                            schema_version: 1,
                            operation: bcode_session_models::ToolContributionOperation::Upsert,
                            persistence:
                                bcode_session_models::ToolContributionPersistence::Transient,
                            artifact: None,
                            payload: serde_json::json!({"must_not_persist": true}),
                        },
                    ),
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(SessionError::LiveEventPersistenceRejected {
                event_kind: "tool_contribution_placed"
            })
        ));
        assert!(
            !manager
                .session_history(session.id)
                .await
                .expect("history")
                .iter()
                .any(|event| matches!(event.kind, SessionEventKind::ToolContributionPlaced { .. }))
        );
        std::fs::remove_dir_all(root).expect("temp dir cleanup");
    }

    #[tokio::test]
    async fn durable_progress_contribution_is_rejected_before_durable_append() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("persistent manager");
        let session = manager
            .create_session(
                Some("durable progress contribution".to_owned()),
                test_working_directory(),
            )
            .await
            .expect("session");
        let result = manager
            .append_event(
                session.id,
                SessionEventKind::ToolContributionPlaced {
                    envelope: bcode_session_models::ToolContributionEnvelope::new(
                        bcode_session_models::ToolContributionPlacement::Progress,
                        bcode_session_models::ToolContributionEvent {
                            invocation_id: "call".to_owned(),
                            contribution_id: "progress".to_owned(),
                            sequence: 1,
                            producer_id: "producer".to_owned(),
                            schema: "example.progress".to_owned(),
                            schema_version: 1,
                            operation: bcode_session_models::ToolContributionOperation::Upsert,
                            persistence: bcode_session_models::ToolContributionPersistence::Durable,
                            artifact: None,
                            payload: serde_json::json!({"must_not_persist": true}),
                        },
                    ),
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(SessionError::LiveEventPersistenceRejected {
                event_kind: "tool_contribution_progress"
            })
        ));
        std::fs::remove_dir_all(root).expect("temp dir cleanup");
    }

    #[tokio::test]
    async fn unknown_durable_contribution_replays_opaquely() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("persistent manager");
        let session = manager
            .create_session(
                Some("opaque contribution".to_owned()),
                test_working_directory(),
            )
            .await
            .expect("session");
        let contribution = bcode_session_models::ToolContributionEvent {
            invocation_id: "call".to_owned(),
            contribution_id: "opaque".to_owned(),
            sequence: 9,
            producer_id: "future.producer".to_owned(),
            schema: "future.unknown/schema".to_owned(),
            schema_version: 4_294_967_000,
            operation: bcode_session_models::ToolContributionOperation::Append,
            persistence: bcode_session_models::ToolContributionPersistence::Durable,
            artifact: None,
            payload: serde_json::json!({"nested": [1, {"future": true}], "number": 1.25}),
        };
        manager
            .append_event(
                session.id,
                SessionEventKind::ToolContribution {
                    event: contribution.clone(),
                },
            )
            .await
            .expect("durable contribution append");
        drop(manager);

        let restored = SessionManager::persistent(&root).expect("restore manager");
        let replayed = restored
            .session_history(session.id)
            .await
            .expect("replayed history")
            .into_iter()
            .find_map(|event| match event.kind {
                SessionEventKind::ToolContribution { event } => Some(event),
                _ => None,
            })
            .expect("durable contribution");
        assert_eq!(replayed, contribution);
        std::fs::remove_dir_all(root).expect("temp dir cleanup");
    }

    #[tokio::test]
    async fn lazy_persistent_manager_defers_catalog_until_requested() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("lazy catalog".to_string()), test_working_directory())
            .await
            .expect("session should create");

        let restored = SessionManager::persistent_lazy(&root);
        assert!(!restored.catalog_loaded());
        assert!(
            restored
                .cached_sessions(&test_working_directory())
                .await
                .is_empty()
        );

        let summary = restored
            .session_summary(session.id)
            .await
            .expect("targeted session load should work");
        assert_eq!(summary.name.as_deref(), Some("lazy catalog"));
        assert!(!restored.catalog_loaded());

        let sessions = restored.list_sessions(&test_working_directory()).await;
        assert!(sessions.len() <= 1);
        restored
            .wait_catalog_loaded()
            .await
            .expect("catalog load should complete");
        let sessions = restored.cached_sessions(&test_working_directory()).await;
        assert_eq!(sessions.len(), 1);
        assert!(restored.catalog_loaded());

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn lazy_catalog_ignores_uncataloged_db_session() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let good = manager
            .create_session(Some("good".to_string()), test_working_directory())
            .await
            .expect("session should create");
        let bad_id = SessionId::new();
        let bad_dir = root.join(bad_id.to_string());
        std::fs::create_dir_all(&bad_dir).expect("bad session dir should create");
        let bad_path = bad_dir.join("session.db");
        std::fs::File::create(&bad_path)
            .expect("bad session DB should create")
            .write_all(&[1_u8])
            .expect("bad session DB should write");

        let restored = SessionManager::persistent_lazy(&root);
        restored
            .wait_catalog_loaded()
            .await
            .expect("catalog load should not inspect uncataloged session DBs");
        let sessions = restored.cached_sessions(&test_working_directory()).await;
        assert!(sessions.iter().any(|session| session.id == good.id));
        assert!(!sessions.iter().any(|session| session.id == bad_id));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[allow(clippy::too_many_lines)]
    fn session_event_kind_tag_cases() -> Vec<(u32, &'static str, SessionEventKind)> {
        let client_id = ClientId::new();
        let skill_id = SkillId::new("compatibility-test");
        vec![
            (
                0,
                "SessionCreated",
                SessionEventKind::SessionCreated {
                    name: Some("created".to_string()),
                    working_directory: test_working_directory(),
                },
            ),
            (
                1,
                "ClientAttached",
                SessionEventKind::ClientAttached { client_id },
            ),
            (
                2,
                "ClientDetached",
                SessionEventKind::ClientDetached { client_id },
            ),
            (
                3,
                "UserMessage",
                SessionEventKind::UserMessage {
                    client_id,
                    text: "user".to_string(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            ),
            (
                4,
                "AssistantDelta",
                SessionEventKind::AssistantDelta {
                    text: "delta".to_string(),
                },
            ),
            (
                5,
                "AssistantMessage",
                SessionEventKind::AssistantMessage {
                    text: "message".to_string(),
                },
            ),
            (
                6,
                "ToolCallRequested",
                SessionEventKind::ToolCallRequested {
                    tool_call_id: "call".to_string(),
                    producer_plugin_id: None,
                    tool_name: "tool".to_string(),
                    arguments_json: "{}".to_string(),
                    working_directory: None,
                },
            ),
            (
                7,
                "PermissionRequested",
                SessionEventKind::PermissionRequested {
                    permission_id: "permission".to_string(),
                    tool_call_id: "call".to_string(),
                    producer_plugin_id: None,
                    tool_name: "tool".to_string(),
                    arguments_json: "{}".to_string(),
                    batch: None,
                    policy_source: None,
                    policy_reason: None,
                },
            ),
            (
                8,
                "PermissionResolved",
                SessionEventKind::PermissionResolved {
                    permission_id: "permission".to_string(),
                    approved: true,
                },
            ),
            (
                9,
                "ModelChanged",
                SessionEventKind::ModelChanged {
                    provider: "provider".to_string(),
                    model: "model".to_string(),
                },
            ),
            (
                10,
                "SystemMessage",
                SessionEventKind::SystemMessage {
                    text: "system".to_string(),
                },
            ),
            (
                11,
                "AgentChanged",
                SessionEventKind::AgentChanged {
                    agent_id: "build".to_string(),
                },
            ),
            (
                12,
                "ModelTurnStarted",
                SessionEventKind::ModelTurnStarted {
                    turn_id: "turn".to_string(),
                },
            ),
            (
                13,
                "ModelTurnFinished",
                SessionEventKind::ModelTurnFinished {
                    turn_id: "turn".to_string(),
                    outcome: bcode_session_models::ModelTurnOutcome::Completed,
                    message: None,
                },
            ),
            (
                14,
                "ModelUsage",
                SessionEventKind::ModelUsage {
                    turn_id: "turn".to_string(),
                    usage: bcode_session_models::SessionTokenUsage {
                        input_tokens: Some(1),
                        output_tokens: Some(2),
                        total_tokens: Some(3),
                        cached_input_tokens: None,
                        cache_write_input_tokens: None,
                        reasoning_tokens: None,
                    },
                },
            ),
            (
                15,
                "ContextCompacted",
                SessionEventKind::ContextCompacted {
                    summary: "summary".to_string(),
                    compacted_through_sequence: 1,
                },
            ),
            (
                16,
                "SessionRenamed",
                SessionEventKind::SessionRenamed {
                    name: Some("renamed".to_string()),
                },
            ),
            (
                17,
                "TraceEvent",
                SessionEventKind::TraceEvent {
                    trace: Box::new(SessionTraceEvent {
                        timestamp_ms: 1,
                        turn_id: None,
                        phase: SessionTracePhase::ModelProviderEvent,
                        payload: SessionTracePayload::ProviderEvent {
                            event_type: "event".to_string(),
                            detail: None,
                        },
                    }),
                },
            ),
            (
                18,
                "SkillInvoked",
                SessionEventKind::SkillInvoked {
                    skill_id: skill_id.clone(),
                    arguments: String::new(),
                    source: None,
                    invoked_at_ms: 1,
                },
            ),
            (
                19,
                "SkillSuggested",
                SessionEventKind::SkillSuggested {
                    skill_id: skill_id.clone(),
                    reason: None,
                    suggested_at_ms: 1,
                },
            ),
            (
                20,
                "SkillActivated",
                SessionEventKind::SkillActivated {
                    skill_id: skill_id.clone(),
                    source: None,
                    mode: SkillActivationMode::Explicit,
                    activated_at_ms: 1,
                },
            ),
            (
                21,
                "SkillDeactivated",
                SessionEventKind::SkillDeactivated {
                    skill_id: skill_id.clone(),
                    deactivated_at_ms: 1,
                },
            ),
            (
                22,
                "SkillContextLoaded",
                SessionEventKind::SkillContextLoaded {
                    skill_id: skill_id.clone(),
                    bytes_loaded: 1,
                    truncated: false,
                    loaded_at_ms: 1,
                    source: None,
                    preview: None,
                },
            ),
            (
                23,
                "SkillInvocationFailed",
                SessionEventKind::SkillInvocationFailed {
                    skill_id,
                    error: "error".to_string(),
                    failed_at_ms: 1,
                },
            ),
            (
                24,
                "AssistantReasoningDelta",
                SessionEventKind::AssistantReasoningDelta {
                    text: "reasoning".to_string(),
                },
            ),
            (
                25,
                "AssistantReasoningMessage",
                SessionEventKind::AssistantReasoningMessage {
                    text: "reasoning".to_string(),
                },
            ),
            (
                26,
                "RuntimeWorkStarted",
                SessionEventKind::RuntimeWorkStarted {
                    work_id: WorkId::new("work"),
                    kind: RuntimeWorkKind::Tool,
                    label: "tool".to_string(),
                    tool_call_id: Some("call".to_string()),
                    plugin_id: Some("plugin".to_string()),
                    service_interface: Some("service".to_string()),
                    operation: Some("invoke".to_string()),
                    parent_work_id: None,
                    started_at_ms: Some(1),
                    cancellable: true,
                },
            ),
            (
                27,
                "RuntimeWorkCancelRequested",
                SessionEventKind::RuntimeWorkCancelRequested {
                    work_id: WorkId::new("work"),
                    requested_at_ms: Some(2),
                    client_id: Some(client_id),
                },
            ),
            (
                28,
                "RuntimeWorkFinished",
                SessionEventKind::RuntimeWorkFinished {
                    work_id: WorkId::new("work"),
                    status: RuntimeWorkStatus::Completed,
                    finished_at_ms: Some(3),
                    message: None,
                },
            ),
            (
                29,
                "RuntimeWorkProgress",
                SessionEventKind::RuntimeWorkProgress {
                    work_id: WorkId::new("work"),
                    message: "progress".to_string(),
                    progress_at_ms: Some(4),
                    completed_units: Some(1),
                    total_units: Some(2),
                },
            ),
            (
                30,
                "ModelTurnCancelRequested",
                SessionEventKind::ModelTurnCancelRequested {
                    turn_id: "turn".to_string(),
                    requested_at_ms: Some(4),
                    client_id: Some(client_id),
                },
            ),
            (
                31,
                "WorkingDirectoryChanged",
                SessionEventKind::WorkingDirectoryChanged {
                    old_working_directory: test_working_directory(),
                    new_working_directory: test_working_directory().join("worktree"),
                },
            ),
            (
                32,
                "SessionImported",
                SessionEventKind::SessionImported {
                    source_id: "pi".to_string(),
                    source_display_name: "Pi".to_string(),
                    external_session_id: "external".to_string(),
                    imported_at_ms: 1,
                },
            ),
            (
                33,
                "SessionForked",
                SessionEventKind::SessionForked {
                    source_session_id: SessionId::new(),
                    source_title: Some("source".to_string()),
                    source_cutoff_sequence: Some(2),
                    source_prompt_sequence: Some(3),
                    forked_at_ms: 1,
                    kind: SessionForkKind::Fork,
                },
            ),
            (
                34,
                "RalphLifecycle",
                SessionEventKind::RalphLifecycle {
                    loop_name: "loop".to_string(),
                    state_dir: test_working_directory(),
                    kind: "started".to_string(),
                    message: "message".to_string(),
                    occurred_at_ms: 1,
                },
            ),
            (
                35,
                "ReasoningChanged",
                SessionEventKind::ReasoningChanged {
                    effort: Some("medium".to_string()),
                    summary: Some("auto".to_string()),
                },
            ),
            (
                38,
                "ProviderContextCompacted",
                SessionEventKind::ProviderContextCompacted {
                    snapshot: bcode_session_models::ProviderContextSnapshot {
                        provider_plugin_id: "provider".to_string(),
                        model_id: "model".to_string(),
                        auth_profile: None,
                        format_version: 1,
                        compatibility_key: "key".to_string(),
                        messages_json: "[]".to_string(),
                        portable_summary: "summary".to_string(),
                        origin:
                            bcode_session_models::ProviderContextSnapshotOrigin::ProviderManaged,
                        request_id: None,
                        request_fingerprint: None,
                    },
                    compacted_through_sequence: 1,
                },
            ),
            (
                39,
                "RequestContextObserved",
                SessionEventKind::RequestContextObserved {
                    observation: bcode_session_models::RequestContextObservation {
                        request: bcode_session_models::ModelRequestIdentity {
                            provider_plugin_id: "provider".to_string(),
                            requested_model_id: None,
                            effective_model_id: "model".to_string(),
                            request_id: "request".to_string(),
                            model_turn_id: "turn".to_string(),
                            round: 0,
                            request_fingerprint: "fingerprint".to_string(),
                            effective_auth_profile: None,
                            context_format_version: None,
                            compatibility_key: None,
                            context_epoch: 0,
                        },
                        context_through_sequence: 1,
                        context_tokens: bcode_session_models::RequestContextTokenCount::Estimated(
                            1,
                        ),
                        local_estimate: bcode_session_models::LocalContextEstimate {
                            tokens: 1,
                            algorithm_version: 1,
                        },
                    },
                },
            ),
            (
                40,
                "PluginStatusNote",
                SessionEventKind::PluginStatusNote {
                    plugin_id: "plugin".to_string(),
                    note_id: "note".to_string(),
                    text: "status".to_string(),
                    metadata: BTreeMap::new(),
                },
            ),
            (
                41,
                "InertHistory",
                SessionEventKind::InertHistory {
                    event_type: "legacy".to_string(),
                    payload: serde_json::Value::Null,
                },
            ),
            (
                42,
                "ToolInvocationLifecycle",
                SessionEventKind::ToolInvocationLifecycle {
                    event: bcode_session_models::ToolInvocationLifecycleEvent {
                        invocation_id: "call".to_string(),
                        sequence: 1,
                        stage: bcode_session_models::ToolInvocationLifecycleStage::Started,
                        message: None,
                        metadata: serde_json::Value::Null,
                    },
                },
            ),
            (
                43,
                "ToolContribution",
                SessionEventKind::ToolContribution {
                    event: bcode_session_models::ToolContributionEvent {
                        invocation_id: "call".to_string(),
                        contribution_id: "surface".to_string(),
                        sequence: 1,
                        producer_id: "producer".to_string(),
                        schema: "example.unknown".to_string(),
                        schema_version: 7,
                        operation: bcode_session_models::ToolContributionOperation::Upsert,
                        persistence: bcode_session_models::ToolContributionPersistence::Durable,
                        artifact: None,
                        payload: serde_json::json!({"opaque": true}),
                    },
                },
            ),
            (
                36,
                "ToolExchangeRequested",
                SessionEventKind::ToolExchangeRequested {
                    request: bcode_session_models::ToolExchangeRequest {
                        invocation_id: "call".to_string(),
                        exchange_id: "question".to_string(),
                        producer_id: "producer".to_string(),
                        schema: "example.question".to_string(),
                        schema_version: 1,
                        payload: serde_json::json!({"opaque": "request"}),
                        response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
                    },
                },
            ),
            (
                37,
                "ToolExchangeResolved",
                SessionEventKind::ToolExchangeResolved {
                    event: bcode_session_models::ToolExchangeResolutionEvent {
                        invocation_id: "call".to_string(),
                        exchange_id: "question".to_string(),
                        resolution: bcode_session_models::ToolExchangeResolution::Responded {
                            payload: serde_json::json!({"opaque": "response"}),
                        },
                    },
                },
            ),
            (
                44,
                "ToolInvocationResultRecorded",
                SessionEventKind::ToolInvocationResultRecorded {
                    record: bcode_session_models::ToolInvocationResultRecord {
                        invocation_id: "call".to_owned(),
                        model_output: "done".to_owned(),
                        is_error: false,
                        presentation: None,
                        result: None,
                    },
                },
            ),
            (
                45,
                "ToolContributionPlaced",
                SessionEventKind::ToolContributionPlaced {
                    envelope: bcode_session_models::ToolContributionEnvelope::new(
                        bcode_session_models::ToolContributionPlacement::Request,
                        bcode_session_models::ToolContributionEvent {
                            invocation_id: "call".to_owned(),
                            contribution_id: "request".to_owned(),
                            sequence: 1,
                            producer_id: "producer".to_owned(),
                            schema: "example.request".to_owned(),
                            schema_version: 1,
                            operation: bcode_session_models::ToolContributionOperation::Upsert,
                            persistence: bcode_session_models::ToolContributionPersistence::Durable,
                            artifact: None,
                            payload: serde_json::json!({"path": "src/lib.rs"}),
                        },
                    ),
                },
            ),
        ]
    }

    fn session_trace_phase_tag_cases() -> Vec<(u32, &'static str, SessionTracePhase)> {
        vec![
            (0, "ModelRequestBuilt", SessionTracePhase::ModelRequestBuilt),
            (
                1,
                "ModelProviderRoundStarted",
                SessionTracePhase::ModelProviderRoundStarted,
            ),
            (
                2,
                "ModelProviderRoundFinished",
                SessionTracePhase::ModelProviderRoundFinished,
            ),
            (
                3,
                "ModelProviderEvent",
                SessionTracePhase::ModelProviderEvent,
            ),
            (
                4,
                "ToolInvocationStarted",
                SessionTracePhase::ToolInvocationStarted,
            ),
            (
                5,
                "ToolPolicyEvaluated",
                SessionTracePhase::ToolPolicyEvaluated,
            ),
            (
                6,
                "ToolPermissionWaitStarted",
                SessionTracePhase::ToolPermissionWaitStarted,
            ),
            (
                7,
                "ToolPermissionWaitFinished",
                SessionTracePhase::ToolPermissionWaitFinished,
            ),
            (
                8,
                "ToolInvocationFinished",
                SessionTracePhase::ToolInvocationFinished,
            ),
            (9, "SkillInvoked", SessionTracePhase::SkillInvoked),
            (10, "SkillSuggested", SessionTracePhase::SkillSuggested),
            (11, "SkillActivated", SessionTracePhase::SkillActivated),
            (12, "SkillDeactivated", SessionTracePhase::SkillDeactivated),
            (
                13,
                "SkillContextLoaded",
                SessionTracePhase::SkillContextLoaded,
            ),
            (
                14,
                "SkillInvocationFailed",
                SessionTracePhase::SkillInvocationFailed,
            ),
            (
                15,
                "ContextCompactionSkipped",
                SessionTracePhase::ContextCompactionSkipped,
            ),
            (
                16,
                "ContextCompactionStarted",
                SessionTracePhase::ContextCompactionStarted,
            ),
            (
                17,
                "ContextCompactionFinished",
                SessionTracePhase::ContextCompactionFinished,
            ),
            (
                18,
                "ToolInvocationOutput",
                SessionTracePhase::ToolInvocationOutput,
            ),
            (
                19,
                "ContextCompactionDiagnostic",
                SessionTracePhase::ContextCompactionDiagnostic,
            ),
        ]
    }

    #[allow(clippy::too_many_lines)]
    fn session_trace_payload_tag_cases() -> Vec<(u32, &'static str, SessionTracePayload)> {
        let mut metadata = BTreeMap::new();
        metadata.insert("conversation_hash".to_string(), "abc123".to_string());
        vec![
            (
                0,
                "ModelRequestBuilt",
                SessionTracePayload::ModelRequestBuilt {
                    provider: "provider".to_string(),
                    model: "model".to_string(),
                    agent_id: "build".to_string(),
                    message_count: 1,
                    tool_count: 2,
                    system_prompt_chars: 3,
                    prompt_cache_mode: "auto".to_string(),
                    conversation_reuse_mode: "auto".to_string(),
                    uses_previous_provider_response: false,
                    metadata,
                    request: None,
                },
            ),
            (
                1,
                "ProviderRound",
                SessionTracePayload::ProviderRound {
                    provider_turn_id: Some("provider-turn".to_string()),
                    provider: "provider".to_string(),
                    round: Some(1),
                    stop_reason: Some("stop".to_string()),
                    duration_ms: Some(42),
                    error: None,
                },
            ),
            (
                2,
                "ProviderEvent",
                SessionTracePayload::ProviderEvent {
                    event_type: "event".to_string(),
                    detail: Some("detail".to_string()),
                },
            ),
            (
                3,
                "ToolInvocationStarted",
                SessionTracePayload::ToolInvocationStarted {
                    tool_call_id: "call".to_string(),
                    plugin_id: "plugin".to_string(),
                    tool_name: "tool".to_string(),
                    side_effect: "read_only".to_string(),
                    requires_permission: false,
                    arguments: None,
                },
            ),
            (
                4,
                "ToolPolicyEvaluated",
                SessionTracePayload::ToolPolicyEvaluated {
                    tool_call_id: "call".to_string(),
                    agent_id: "build".to_string(),
                    decision: "allow".to_string(),
                    reason: None,
                },
            ),
            (
                5,
                "ToolPermissionWait",
                SessionTracePayload::ToolPermissionWait {
                    permission_id: "permission".to_string(),
                    tool_call_id: "call".to_string(),
                    approved: Some(true),
                    duration_ms: Some(7),
                },
            ),
            (
                6,
                "ToolInvocationFinished",
                SessionTracePayload::ToolInvocationFinished {
                    tool_call_id: "call".to_string(),
                    duration_ms: 9,
                    is_error: false,
                    output_bytes: 12,
                    output: None,
                },
            ),
            (
                7,
                "ContextCompaction",
                SessionTracePayload::ContextCompaction {
                    reason: "manual".to_string(),
                    projected_context_chars: 123,
                    compacted: true,
                    message: None,
                },
            ),
            (
                8,
                "ProviderStreamEvent",
                SessionTracePayload::ProviderStreamEvent(ProviderStreamEvent::TurnStarted),
            ),
        ]
    }

    #[tokio::test]
    async fn background_execution_sessions_are_hidden_but_inspectable() {
        let manager = SessionManager::default();
        let parent = manager
            .create_session(Some("parent".to_string()), test_working_directory())
            .await
            .expect("parent");
        let provenance = ExecutionSessionProvenance {
            owner: "workflow".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review-a".to_string(),
            attempt: 1,
            parent_session_id: parent.id,
            workspace_snapshot: Some("snapshot-1".to_string()),
            context_mode: ExecutionSessionContextMode::FreshIsolated,
            parent_generation: None,
        };
        let child = manager
            .create_fresh_execution_session(Some("review".to_string()), provenance.clone(), None)
            .await
            .expect("child");

        assert_eq!(
            child.execution.as_ref().expect("execution").visibility,
            SessionVisibility::Background
        );
        assert_eq!(
            child
                .execution
                .as_ref()
                .map(|execution| &execution.provenance),
            Some(&provenance)
        );
        assert_eq!(child.working_directory, parent.working_directory);
        assert_eq!(
            manager.list_sessions(&parent.working_directory).await,
            vec![parent]
        );
        assert!(
            manager
                .list_sessions_with_background(&child.working_directory, true)
                .await
                .iter()
                .any(|summary| summary.id == child.id)
        );
        assert_eq!(
            manager
                .session_summary(child.id)
                .await
                .expect("direct inspect")
                .id,
            child.id
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn two_fixed_generation_reviewers_have_independent_transcripts() {
        let manager = SessionManager::default();
        let parent = manager
            .create_session(Some("parent".to_string()), test_working_directory())
            .await
            .expect("parent");
        manager
            .append_event(
                parent.id,
                SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "shared snapshot".to_string(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            )
            .await
            .expect("snapshot prompt");
        let generation = manager
            .session_history(parent.id)
            .await
            .expect("history")
            .last()
            .expect("event")
            .sequence;
        let create = |node_id: &str| ExecutionSessionProvenance {
            owner: "workflow".to_string(),
            run_id: "run-1".to_string(),
            node_id: node_id.to_string(),
            attempt: 1,
            parent_session_id: parent.id,
            workspace_snapshot: Some("snapshot-1".to_string()),
            context_mode: ExecutionSessionContextMode::FixedGenerationFork,
            parent_generation: Some(generation),
        };
        let (left, right) = tokio::join!(
            manager.create_fixed_generation_execution_session(
                Some("left".to_string()),
                create("review-left"),
                generation,
                None,
            ),
            manager.create_fixed_generation_execution_session(
                Some("right".to_string()),
                create("review-right"),
                generation,
                None,
            ),
        );
        let left = left.expect("left");
        let right = right.expect("right");
        assert_ne!(left.id, right.id);
        assert_eq!(left.working_directory, right.working_directory);
        assert_eq!(
            left.execution
                .as_ref()
                .and_then(|execution| execution.provenance.workspace_snapshot.as_deref()),
            Some("snapshot-1")
        );
        assert_eq!(
            left.execution
                .as_ref()
                .and_then(|execution| execution.provenance.workspace_snapshot.as_deref()),
            right
                .execution
                .as_ref()
                .and_then(|execution| execution.provenance.workspace_snapshot.as_deref())
        );
        let left_history = manager
            .session_history(left.id)
            .await
            .expect("left history");
        let right_history = manager
            .session_history(right.id)
            .await
            .expect("right history");
        let inherited = |events: &[SessionEvent]| {
            events
                .iter()
                .filter_map(|event| match &event.kind {
                    SessionEventKind::UserMessage { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(inherited(&left_history), ["shared snapshot".to_string()]);
        assert_eq!(inherited(&right_history), ["shared snapshot".to_string()]);

        manager
            .append_event(
                left.id,
                SessionEventKind::SystemMessage {
                    text: "left-only".to_string(),
                },
            )
            .await
            .expect("left note");
        assert!(
            !manager
                .session_history(right.id)
                .await
                .expect("right history")
                .iter()
                .any(|event| matches!(
                    &event.kind,
                    SessionEventKind::SystemMessage { text } if text == "left-only"
                ))
        );
    }

    #[tokio::test]
    async fn undeclared_fresh_directory_is_rejected() {
        let manager = SessionManager::default();
        let parent = manager
            .create_session(Some("parent".to_string()), test_working_directory())
            .await
            .expect("parent");
        let other = unique_temp_dir();
        std::fs::create_dir_all(&other).expect("other directory");
        let error = manager
            .create_fresh_execution_session(
                Some("invalid".to_string()),
                ExecutionSessionProvenance {
                    owner: "workflow".to_string(),
                    run_id: "run-1".to_string(),
                    node_id: "review".to_string(),
                    attempt: 1,
                    parent_session_id: parent.id,
                    workspace_snapshot: Some("snapshot-1".to_string()),
                    context_mode: ExecutionSessionContextMode::FreshIsolated,
                    parent_generation: None,
                },
                Some(other.clone()),
            )
            .await
            .expect_err("undeclared directory rejected");
        assert!(matches!(
            error,
            SessionError::InvalidExecutionSessionProvenance(reason)
                if reason.contains("declared-worktree")
        ));
        std::fs::remove_dir_all(other).expect("cleanup");
    }

    #[tokio::test]
    async fn worktree_execution_uses_owner_validated_directory() {
        let manager = SessionManager::default();
        let parent = manager
            .create_session(Some("parent".to_string()), test_working_directory())
            .await
            .expect("parent");
        let worktree = test_working_directory();
        let provenance = ExecutionSessionProvenance {
            owner: "workflow".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            attempt: 1,
            parent_session_id: parent.id,
            workspace_snapshot: Some("snapshot-1".to_string()),
            context_mode: ExecutionSessionContextMode::FreshIsolated,
            parent_generation: None,
        };
        let child = manager
            .create_fresh_execution_session_in_worktree(
                Some("review".to_string()),
                provenance,
                &worktree,
            )
            .await
            .expect("worktree child");
        assert_eq!(child.working_directory, worktree);
    }

    #[tokio::test]
    async fn execution_session_rejects_missing_snapshot_identity() {
        let manager = SessionManager::default();
        let parent = manager
            .create_session(Some("parent".to_string()), test_working_directory())
            .await
            .expect("parent");
        let error = manager
            .create_fresh_execution_session(
                Some("invalid".to_string()),
                ExecutionSessionProvenance {
                    owner: "workflow".to_string(),
                    run_id: "run-1".to_string(),
                    node_id: "review".to_string(),
                    attempt: 1,
                    parent_session_id: parent.id,
                    workspace_snapshot: None,
                    context_mode: ExecutionSessionContextMode::FreshIsolated,
                    parent_generation: None,
                },
                None,
            )
            .await
            .expect_err("missing snapshot rejected");
        assert!(matches!(
            error,
            SessionError::InvalidExecutionSessionProvenance(reason)
                if reason.contains("workspace_snapshot")
        ));
    }

    #[tokio::test]
    async fn background_execution_provenance_survives_persistent_restore() {
        let root = unique_temp_dir();
        let child_id;
        {
            let manager = SessionManager::persistent(&root).expect("manager");
            let parent = manager
                .create_session(Some("parent".to_string()), test_working_directory())
                .await
                .expect("parent");
            let child = manager
                .create_fresh_execution_session(
                    Some("review".to_string()),
                    ExecutionSessionProvenance {
                        owner: "workflow".to_string(),
                        run_id: "run-1".to_string(),
                        node_id: "review".to_string(),
                        attempt: 1,
                        parent_session_id: parent.id,
                        workspace_snapshot: Some("snapshot-1".to_string()),
                        context_mode: ExecutionSessionContextMode::FreshIsolated,
                        parent_generation: None,
                    },
                    None,
                )
                .await
                .expect("child");
            child_id = child.id;
        }

        let restored = SessionManager::persistent(&root).expect("restored");
        let summary = restored
            .session_summary(child_id)
            .await
            .expect("inspect restored child");
        assert_eq!(
            summary.execution.as_ref().expect("execution").visibility,
            SessionVisibility::Background
        );
        assert!(summary.execution.as_ref().is_some_and(|execution| {
            execution.provenance.owner == "workflow"
                && execution.provenance.run_id == "run-1"
                && execution.provenance.node_id == "review"
        }));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn fixed_generation_execution_session_copies_exact_parent_snapshot() {
        let manager = SessionManager::default();
        let parent = manager
            .create_session(Some("parent".to_string()), test_working_directory())
            .await
            .expect("parent");
        manager
            .append_event(
                parent.id,
                SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "snapshot".to_string(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            )
            .await
            .expect("prompt");
        let generation = manager
            .session_history(parent.id)
            .await
            .expect("history")
            .last()
            .expect("event")
            .sequence;
        let provenance = ExecutionSessionProvenance {
            owner: "workflow".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review-a".to_string(),
            attempt: 1,
            parent_session_id: parent.id,
            workspace_snapshot: Some("snapshot-1".to_string()),
            context_mode: ExecutionSessionContextMode::FixedGenerationFork,
            parent_generation: Some(generation),
        };
        let child = manager
            .create_fixed_generation_execution_session(
                Some("review".to_string()),
                provenance.clone(),
                generation,
                None,
            )
            .await
            .expect("fixed clone");

        assert_eq!(
            child
                .execution
                .as_ref()
                .map(|execution| &execution.provenance),
            Some(&provenance)
        );
        assert_eq!(child.working_directory, parent.working_directory);
        let child_history = manager
            .session_history(child.id)
            .await
            .expect("child history");
        assert!(child_history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. } if text == "snapshot"
        )));

        manager
            .append_event(
                parent.id,
                SessionEventKind::SystemMessage {
                    text: "later".to_string(),
                },
            )
            .await
            .expect("later event");
        let stale = manager
            .create_fixed_generation_execution_session(
                Some("stale".to_string()),
                ExecutionSessionProvenance {
                    node_id: "review-b".to_string(),
                    ..child.execution.expect("execution").provenance
                },
                generation,
                None,
            )
            .await
            .expect_err("stale generation rejected");
        assert!(matches!(stale, SessionError::CloneGenerationChanged { .. }));
    }

    #[tokio::test]
    async fn shared_execution_admission_serializes_one_parent() {
        let manager = SessionManager::default();
        let parent = manager
            .create_session(Some("parent".to_string()), test_working_directory())
            .await
            .expect("parent");
        let provenance = ExecutionSessionProvenance {
            owner: "workflow".to_string(),
            run_id: "run-1".to_string(),
            node_id: "sequential".to_string(),
            attempt: 1,
            parent_session_id: parent.id,
            workspace_snapshot: Some("snapshot-1".to_string()),
            context_mode: ExecutionSessionContextMode::SharedSequential,
            parent_generation: None,
        };
        let first = manager
            .admit_shared_execution_session(parent.id, &provenance)
            .await
            .expect("first permit");
        assert_eq!(first.session_id(), parent.id);
        let waiting = {
            let manager = manager.clone();
            let provenance = provenance.clone();
            tokio::spawn(async move {
                manager
                    .admit_shared_execution_session(parent.id, &provenance)
                    .await
                    .expect("second permit")
            })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        drop(first);
        assert_eq!(waiting.await.expect("join").session_id(), parent.id);
    }

    #[test]
    fn shared_execution_requires_declared_parent_and_no_child() {
        let parent = SessionId::new();
        let provenance = ExecutionSessionProvenance {
            owner: "workflow".to_string(),
            run_id: "run-1".to_string(),
            node_id: "sequential".to_string(),
            attempt: 1,
            parent_session_id: parent,
            workspace_snapshot: Some("snapshot-1".to_string()),
            context_mode: ExecutionSessionContextMode::SharedSequential,
            parent_generation: None,
        };
        assert_eq!(
            shared_execution_session(parent, &provenance).expect("shared parent"),
            parent
        );
        assert!(shared_execution_session(SessionId::new(), &provenance).is_err());
    }

    fn encoded_variant_tag(value: &impl Serialize) -> u32 {
        let bytes = bmux_codec::to_positional_vec(value).expect("value should encode");
        let (tag, _) = bmux_codec::varint::decode_u32(&bytes).expect("variant tag should decode");
        tag
    }

    fn test_working_directory() -> std::path::PathBuf {
        "/tmp/bcode-session-test-working-directory".into()
    }

    fn unique_temp_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        let counter = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "bcode-session-test-{}-{nanos}-{counter}",
            std::process::id()
        ))
    }
}
