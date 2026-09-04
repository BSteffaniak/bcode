//! Session actor, handle, and snapshot plumbing.

use super::{
    Arc, ClientId, Instant, PathBuf, ProjectionWindow, ProjectionWindowRequest, SessionAttachment,
    SessionError, SessionEvent, SessionEventKind, SessionEventProvenance, SessionInputHistoryEntry,
    SessionLiveEvent, SessionLiveEventKind, SessionLoadStatusKind, SessionState,
    SessionStoreExecutor, SessionSummary, elapsed_ms, input_history_from_events,
    model_context_events_from_history, state::LiveTextStreamKey, title_from_first_prompt,
    usize_to_u64,
};
use crate::db::{MaterializedProjection, SessionDb, SessionDbError};
use crate::lease::SessionLeaseGuard;
use bcode_metrics::MetricsContext;
use bcode_session_models::{
    ProjectionWindowAnchor, SessionHistoryAroundQuery, SessionHistoryPage, SessionHistoryQuery,
    SessionHistoryWindow, SessionInspectionPage, SessionInspectionQuery,
};
use std::collections::BTreeMap;
use std::sync::RwLock;
use tokio::sync::{broadcast, mpsc, oneshot};

const SESSION_DATABASE_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How far `updated_at_ms` may drift before an activity-only change rewrites `manifest.json`.
const MANIFEST_ACTIVITY_REFRESH_INTERVAL_MS: u64 = 5_000;

/// The subset of a session summary that `manifest.json` exists to cache.
///
/// `display` covers every field a picker or catalog fallback renders; `updated_at_ms` is tracked
/// separately so pure activity ticks can be coalesced instead of rewriting the file per event.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ManifestSummaryIdentity {
    display: SessionSummary,
    updated_at_ms: u64,
}

impl ManifestSummaryIdentity {
    fn from_summary(summary: &SessionSummary) -> Self {
        let mut display = summary.clone();
        display.client_count = 0;
        display.updated_at_ms = 0;
        Self {
            display,
            updated_at_ms: summary.updated_at_ms,
        }
    }
}
const MAX_ACTIVE_TEXT_STREAM_KEYS: usize = 32;
const MAX_ACTIVE_TEXT_STREAM_TOMBSTONES: usize = 64;
const MAX_ACTIVE_TEXT_STREAM_BYTES_PER_KEY: usize = 256 * 1024;
const MAX_ACTIVE_TEXT_STREAM_BYTES_PER_SESSION: usize = 1024 * 1024;

fn bounded_text_suffix(text: &str, max_bytes: usize) -> (&str, usize) {
    if text.len() <= max_bytes {
        return (text, 0);
    }
    let mut start = text.len().saturating_sub(max_bytes);
    while !text.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    (&text[start..], start)
}

const fn append_rejection_metric(error: &SessionDbError) -> &'static str {
    match error {
        SessionDbError::WriterIncompatible { .. } => {
            "session.actor.append_event.rejected.writer_incompatible_total"
        }
        SessionDbError::ModelContextProjectionStale { .. }
        | SessionDbError::ProjectionStale { .. } => {
            "session.actor.append_event.rejected.projection_stale_total"
        }
        SessionDbError::ModelContextProjectionVersion { .. }
        | SessionDbError::ProjectionIncompatible { .. } => {
            "session.actor.append_event.rejected.projection_incompatible_total"
        }
        SessionDbError::InvalidCanonicalAppendSequence { .. }
        | SessionDbError::InvalidCanonicalSequence { .. } => {
            "session.actor.append_event.rejected.canonical_sequence_total"
        }
        SessionDbError::TransientContribution { .. } => {
            "session.actor.append_event.rejected.transient_contribution_total"
        }
        SessionDbError::Connection(_)
        | SessionDbError::Database(_)
        | SessionDbError::Migration(_)
        | SessionDbError::Io(_)
        | SessionDbError::Lease(_)
        | SessionDbError::Serialize(_)
        | SessionDbError::PersistedEvent(_)
        | SessionDbError::InvalidCompactionMarker { .. }
        | SessionDbError::InvalidRow { .. }
        | SessionDbError::MigrationHistoryIncompatible { .. }
        | SessionDbError::HistoryReadLimitExceeded { .. } => {
            "session.actor.append_event.rejected.storage_error_total"
        }
    }
}

fn record_append_rejection_metrics(
    metrics: &bcode_metrics::MetricsRegistry,
    result: &Result<(), SessionDbError>,
) {
    if let Err(error) = result {
        metrics.increment_counter("session.actor.append_event.rejected_total");
        metrics.increment_counter(append_rejection_metric(error));
    }
}

#[derive(Debug, Clone)]
pub struct SessionHandle {
    commands: mpsc::Sender<SessionCommand>,
    snapshot: Arc<RwLock<SessionSnapshot>>,
}

/// Categories of server work that can keep a persistent session owned after clients detach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionOwnershipKind {
    /// A prompt, skill, or compaction command accepted for later execution.
    QueuedCommand,
    /// Runtime work that can access or mutate canonical session state.
    RuntimeWork,
    /// A plugin invocation that can still publish canonical session results.
    PluginInvocation,
}

impl SessionOwnershipKind {
    const fn label(self) -> &'static str {
        match self {
            Self::QueuedCommand => "queued_command",
            Self::RuntimeWork => "runtime_work",
            Self::PluginInvocation => "plugin_invocation",
        }
    }
}

/// Snapshot of ownership blockers currently registered with one session actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOwnershipSnapshot {
    /// Number of clients attached to the actor.
    pub attached_clients: usize,
    /// Long-lived ownership guards grouped by category.
    pub guards: BTreeMap<SessionOwnershipKind, u64>,
    /// Whether a session database handle could not be proven closed during release.
    ///
    /// This is never a reason to withhold a release attempt; it records that an otherwise
    /// quiescent release could not complete because the backend connection stayed open.
    pub database_handle_retained: bool,
}

/// Outcome of releasing one actor's cached session database handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DatabaseRelease {
    /// The handle was closed, proven terminal, and dropped.
    Released,
    /// No cached handle was held.
    AlreadyReleased,
    /// The connection could not be proven closed, so the handle is still held.
    Retained,
}

/// Result of an atomic quiescent ownership-release attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOwnershipRelease {
    /// Runtime ownership and cached database resources were released.
    Released,
    /// The actor did not own the persistent session.
    AlreadyUnowned,
    /// Clients or typed guards still require runtime ownership.
    Blocked(SessionOwnershipSnapshot),
}

impl SessionOwnershipSnapshot {
    /// Return whether no client or long-lived activity currently requires ownership.
    #[must_use]
    pub fn is_quiescent(&self) -> bool {
        self.attached_clients == 0 && self.guards.values().all(|count| *count == 0)
    }
}

/// Cloneable hold that keeps one persistent session's compatibility lease alive.
///
/// Dropping the final clone notifies the actor, which serially reevaluates quiescent release.
#[derive(Debug, Clone)]
pub struct SessionOwnershipGuard {
    _inner: Arc<SessionOwnershipGuardInner>,
}

#[derive(Debug)]
struct SessionOwnershipGuardInner {
    releases: mpsc::UnboundedSender<OwnershipRelease>,
    kind: SessionOwnershipKind,
    lease: Option<Arc<SessionLeaseGuard>>,
}

#[derive(Debug)]
struct OwnershipRelease {
    kind: SessionOwnershipKind,
    lease: Option<Arc<SessionLeaseGuard>>,
}

impl Drop for SessionOwnershipGuardInner {
    fn drop(&mut self) {
        let _ = self.releases.send(OwnershipRelease {
            kind: self.kind,
            lease: self.lease.take(),
        });
    }
}

impl SessionHandle {
    #[must_use]
    pub fn new(
        state: SessionState,
        store: Option<SessionStoreExecutor>,
        lease: Option<SessionLeaseGuard>,
    ) -> Self {
        Self::new_with_db(state, store, lease, None)
    }

    /// Create a handle whose actor starts with an already-open session database.
    ///
    /// Loading a persistent session validates storage compatibility, readiness, and projected
    /// state through one database handle; passing that handle along lets the actor's first write
    /// reuse it instead of opening the file again. Turso opens take an exclusive file lock, so
    /// every avoided reopen is also one fewer window in which a concurrent daemon can collide.
    #[must_use]
    pub(crate) fn new_with_db(
        state: SessionState,
        store: Option<SessionStoreExecutor>,
        lease: Option<SessionLeaseGuard>,
        db: Option<SessionDb>,
    ) -> Self {
        let snapshot = Arc::new(RwLock::new(SessionSnapshot::from_state(
            &state,
            lease.is_some(),
        )));
        let (commands, receiver) = mpsc::channel(256);
        let (ownership_releases, ownership_release_receiver) = mpsc::unbounded_channel();
        let actor = SessionActor {
            state,
            store,
            lease: lease.map(Arc::new),
            ownership_guards: BTreeMap::new(),
            db,
            last_manifest_summary: None,
            commands: receiver,
            ownership_releases,
            ownership_release_receiver,
            snapshot: Arc::clone(&snapshot),
        };
        tokio::spawn(actor.run());
        Self { commands, snapshot }
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        self.snapshot
            .read()
            .expect("session snapshot lock poisoned")
            .clone()
    }

    async fn send<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<T>) -> SessionCommand,
    ) -> Result<T, SessionError> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(build(reply))
            .await
            .map_err(|_| SessionError::NotFound(self.snapshot().summary.id))?;
        receiver
            .await
            .map_err(|_| SessionError::NotFound(self.snapshot().summary.id))
    }

    pub async fn set_composer_draft(
        &self,
        text: String,
        updated_at_ms: u64,
    ) -> Result<(), SessionError> {
        self.send(|reply| SessionCommand::SetComposerDraft {
            text,
            updated_at_ms,
            reply,
        })
        .await?
    }

    pub async fn composer_draft(&self) -> Result<Option<String>, SessionError> {
        self.send(SessionCommand::ComposerDraft).await?
    }

    /// Validate that the next append can begin on the actor-owned database connection.
    ///
    /// # Errors
    ///
    /// Returns a session database error when the writer contract or required projections are not
    /// ready for the next append.
    pub async fn validate_write_readiness(&self) -> Result<(), SessionError> {
        self.send(SessionCommand::ValidateWriteReadiness).await?
    }

    /// Reload persisted state through the actor's cached database and validate append readiness.
    ///
    /// # Errors
    ///
    /// Returns a session database error when the session is unowned, the writer contract or
    /// required projections are not ready, or the session-state projection is stale.
    pub(crate) async fn reload_state_and_validate_write_readiness(
        &self,
    ) -> Result<(), SessionError> {
        self.send(SessionCommand::ReloadStateAndValidateWriteReadiness)
            .await?
    }

    /// Compute bounded session health through the actor's cached database handle.
    ///
    /// # Errors
    ///
    /// Returns an error when the session database does not exist or cannot be opened; the caller
    /// then falls back to its own probe.
    pub(crate) async fn health(&self, root: PathBuf) -> Result<super::SessionHealth, SessionError> {
        self.send(|reply| SessionCommand::Health { root, reply })
            .await?
    }

    pub async fn append_event(
        &self,
        kind: SessionEventKind,
        activity_timestamp_ms: u64,
    ) -> Result<SessionEvent, SessionError> {
        self.send(|reply| SessionCommand::AppendEvent {
            kind,
            provenance: None,
            activity_timestamp_ms,
            reply,
        })
        .await?
    }

    pub async fn append_event_with_provenance(
        &self,
        kind: SessionEventKind,
        provenance: Option<SessionEventProvenance>,
        activity_timestamp_ms: u64,
    ) -> Result<SessionEvent, SessionError> {
        self.send(|reply| SessionCommand::AppendEvent {
            kind,
            provenance,
            activity_timestamp_ms,
            reply,
        })
        .await?
    }

    pub async fn append_tool_invocation_result(
        &self,
        record: bcode_session_models::ToolInvocationResultRecord,
        activity_timestamp_ms: u64,
    ) -> Result<SessionEvent, SessionError> {
        self.send(|reply| SessionCommand::AppendToolInvocationResult {
            record,
            activity_timestamp_ms,
            reply,
        })
        .await?
    }

    pub async fn append_user_message(
        &self,
        client_id: ClientId,
        text: String,
        admission: bcode_session_models::TurnAdmissionMetadata,
        activity_timestamp_ms: u64,
    ) -> Result<TurnAdmissionResult, SessionError> {
        self.send(|reply| SessionCommand::AppendUserMessage {
            client_id,
            text,
            admission,
            activity_timestamp_ms,
            reply,
        })
        .await?
    }

    pub async fn attach(
        &self,
        client_id: ClientId,
        mode: AttachMode,
    ) -> Result<SessionAttachment, SessionError> {
        let (reply, receiver) = oneshot::channel();
        let queued_at = Instant::now();
        self.commands
            .send(SessionCommand::Attach {
                client_id,
                mode,
                queued_at,
                reply,
            })
            .await
            .map_err(|_| SessionError::NotFound(self.snapshot().summary.id))?;
        receiver
            .await
            .map_err(|_| SessionError::NotFound(self.snapshot().summary.id))?
    }

    #[cfg(test)]
    pub async fn cancel_attach_before_reply(
        &self,
        client_id: ClientId,
        mode: AttachMode,
    ) -> Result<(), SessionError> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(SessionCommand::Attach {
                client_id,
                mode,
                queued_at: Instant::now(),
                reply,
            })
            .await
            .map_err(|_| SessionError::NotFound(self.snapshot().summary.id))?;
        drop(receiver);
        let _summary = self.summary().await?;
        Ok(())
    }

    pub async fn subscribe_events(&self) -> Result<SessionEventReceivers, SessionError> {
        self.send(SessionCommand::SubscribeEvents).await?
    }

    pub async fn detach(&self, client_id: ClientId) -> Result<bool, SessionError> {
        self.send(|reply| SessionCommand::Detach { client_id, reply })
            .await?
    }

    pub async fn summary(&self) -> Result<SessionSummary, SessionError> {
        self.send(SessionCommand::Summary).await
    }

    pub async fn working_directory(&self) -> Result<PathBuf, SessionError> {
        self.send(SessionCommand::WorkingDirectory).await
    }

    /// Return the complete durable event history.
    ///
    /// This method performs a full canonical event read and is reserved for explicit
    /// export/debug/history commands. Normal runtime flows must use bounded history pages,
    /// projection windows, or typed read models instead.
    pub async fn history(&self) -> Result<Vec<SessionEvent>, SessionError> {
        self.send(SessionCommand::History).await?
    }

    pub async fn history_page(
        &self,
        query: SessionHistoryQuery,
    ) -> Result<SessionHistoryPage, SessionError> {
        self.send(|reply| SessionCommand::HistoryPage { query, reply })
            .await?
    }

    pub async fn history_around(
        &self,
        query: SessionHistoryAroundQuery,
    ) -> Result<SessionHistoryWindow, SessionError> {
        self.send(|reply| SessionCommand::HistoryAround { query, reply })
            .await?
    }

    pub async fn inspection_page(
        &self,
        query: SessionInspectionQuery,
    ) -> Result<SessionInspectionPage, SessionError> {
        self.send(|reply| SessionCommand::InspectionPage { query, reply })
            .await?
    }

    pub async fn projection_window(
        &self,
        request: ProjectionWindowRequest,
    ) -> Result<ProjectionWindow, SessionError> {
        self.send(|reply| SessionCommand::ProjectionWindowFromIndex { request, reply })
            .await?
    }

    pub async fn events_range(
        &self,
        start_sequence: u64,
        end_sequence: u64,
        max_events: usize,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        self.send(|reply| SessionCommand::EventsRange {
            start_sequence,
            end_sequence,
            max_events,
            reply,
        })
        .await?
    }

    pub async fn input_history(&self) -> Result<Vec<SessionInputHistoryEntry>, SessionError> {
        self.send(SessionCommand::InputHistory).await?
    }

    pub async fn input_history_page(
        &self,
        before_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<SessionInputHistoryEntry>, SessionError> {
        self.send(|reply| SessionCommand::InputHistoryPage {
            before_sequence,
            limit,
            reply,
        })
        .await?
    }

    pub async fn current_context_epoch(&self) -> Result<u64, SessionError> {
        self.send(SessionCommand::CurrentContextEpoch).await?
    }

    pub async fn current_context_occupancy(
        &self,
    ) -> Result<Option<bcode_session_models::RequestContextOccupancy>, SessionError> {
        self.send(SessionCommand::CurrentRequestContextOccupancy)
            .await?
    }

    pub async fn model_context_events(&self) -> Result<Vec<SessionEvent>, SessionError> {
        self.send(SessionCommand::ModelContextEvents).await?
    }

    pub async fn active_tool_runs(&self) -> Result<Vec<crate::db::ToolRun>, SessionError> {
        self.send(SessionCommand::ActiveToolRuns).await?
    }

    pub async fn active_runtime_work(
        &self,
    ) -> Result<Vec<crate::db::RuntimeWorkProjection>, SessionError> {
        self.send(SessionCommand::ActiveRuntimeWork).await?
    }

    pub async fn current_runtime_selection(
        &self,
    ) -> Result<crate::SessionRuntimeSelection, SessionError> {
        self.send(SessionCommand::CurrentRuntimeSelection).await
    }

    pub async fn current_model_selection(
        &self,
    ) -> Result<(Option<String>, Option<String>), SessionError> {
        self.send(SessionCommand::CurrentModelSelection).await
    }

    pub async fn current_reasoning_selection(
        &self,
    ) -> Result<(Option<String>, Option<String>), SessionError> {
        self.send(SessionCommand::CurrentReasoningSelection).await
    }

    /// Return the reasoning selection remembered for one provider/model pair, when present.
    ///
    /// # Errors
    ///
    /// Returns an error when the session actor is unavailable.
    pub async fn remembered_reasoning_for_model(
        &self,
        scope: bcode_session_models::ModelScopeKey,
    ) -> Result<Option<crate::state::RememberedReasoning>, SessionError> {
        self.send(|reply| SessionCommand::RememberedReasoningForModel { scope, reply })
            .await
    }

    pub async fn current_agent_selection(&self) -> Result<Option<String>, SessionError> {
        self.send(SessionCommand::CurrentAgentSelection).await
    }

    pub async fn set_current_agent(&self, agent_id: String) -> Result<(), SessionError> {
        self.send(|reply| SessionCommand::SetCurrentAgent { agent_id, reply })
            .await?
    }

    pub async fn publish_live_event(
        &self,
        event: SessionLiveEventKind,
    ) -> Result<Option<SessionLiveEvent>, SessionError> {
        self.send(|reply| SessionCommand::PublishLive { event, reply })
            .await
    }

    pub async fn release_idle_resources(&self) -> Result<bool, SessionError> {
        self.send(SessionCommand::ReleaseIdleResources).await
    }

    pub async fn release_database_resources(&self) -> Result<bool, SessionError> {
        self.send(SessionCommand::ReleaseDatabaseResources).await
    }

    pub async fn acquire_ownership(
        &self,
        kind: SessionOwnershipKind,
    ) -> Result<SessionOwnershipGuard, SessionError> {
        self.send(|reply| SessionCommand::AcquireOwnership { kind, reply })
            .await?
    }

    pub async fn ownership_snapshot(&self) -> Result<SessionOwnershipSnapshot, SessionError> {
        self.send(SessionCommand::OwnershipSnapshot).await
    }

    pub async fn release_ownership_if_quiescent(
        &self,
    ) -> Result<SessionOwnershipRelease, SessionError> {
        self.send(SessionCommand::ReleaseOwnershipIfQuiescent).await
    }

    pub async fn adopt_lease(&self, lease: SessionLeaseGuard) -> Result<(), SessionError> {
        self.send(|reply| SessionCommand::AdoptLease {
            lease: Arc::new(lease),
            reply,
        })
        .await
    }

    pub fn client_count(&self) -> usize {
        self.snapshot().summary.client_count
    }

    pub async fn shutdown(&self) -> Result<(), SessionError> {
        self.send(SessionCommand::Shutdown).await
    }
}

#[derive(Debug, Clone)]
pub enum AttachMode {
    Full,
    Recent { limit: usize },
    ProjectionWindow { history: Vec<SessionEvent> },
}

type SessionEventReceivers = (
    SessionSummary,
    broadcast::Receiver<SessionEvent>,
    broadcast::Receiver<SessionLiveEvent>,
);

#[derive(Debug)]
pub struct TurnAdmissionResult {
    pub admission: bcode_session_models::TurnAdmission,
    pub events: Vec<SessionEvent>,
}

enum SessionCommand {
    AppendEvent {
        kind: SessionEventKind,
        provenance: Option<SessionEventProvenance>,
        activity_timestamp_ms: u64,
        reply: oneshot::Sender<Result<SessionEvent, SessionError>>,
    },
    AppendToolInvocationResult {
        record: bcode_session_models::ToolInvocationResultRecord,
        activity_timestamp_ms: u64,
        reply: oneshot::Sender<Result<SessionEvent, SessionError>>,
    },
    AppendUserMessage {
        client_id: ClientId,
        text: String,
        admission: bcode_session_models::TurnAdmissionMetadata,
        activity_timestamp_ms: u64,
        reply: oneshot::Sender<Result<TurnAdmissionResult, SessionError>>,
    },
    Attach {
        client_id: ClientId,
        mode: AttachMode,
        queued_at: Instant,
        reply: oneshot::Sender<Result<SessionAttachment, SessionError>>,
    },
    SubscribeEvents(oneshot::Sender<Result<SessionEventReceivers, SessionError>>),
    Detach {
        client_id: ClientId,
        reply: oneshot::Sender<Result<bool, SessionError>>,
    },
    Summary(oneshot::Sender<SessionSummary>),
    WorkingDirectory(oneshot::Sender<PathBuf>),
    SetComposerDraft {
        text: String,
        updated_at_ms: u64,
        reply: oneshot::Sender<Result<(), SessionError>>,
    },
    ComposerDraft(oneshot::Sender<Result<Option<String>, SessionError>>),
    ValidateWriteReadiness(oneshot::Sender<Result<(), SessionError>>),
    ReloadStateAndValidateWriteReadiness(oneshot::Sender<Result<(), SessionError>>),
    Health {
        root: PathBuf,
        reply: oneshot::Sender<Result<super::SessionHealth, SessionError>>,
    },
    History(oneshot::Sender<Result<Vec<SessionEvent>, SessionError>>),
    HistoryPage {
        query: SessionHistoryQuery,
        reply: oneshot::Sender<Result<SessionHistoryPage, SessionError>>,
    },
    HistoryAround {
        query: SessionHistoryAroundQuery,
        reply: oneshot::Sender<Result<SessionHistoryWindow, SessionError>>,
    },
    InspectionPage {
        query: SessionInspectionQuery,
        reply: oneshot::Sender<Result<SessionInspectionPage, SessionError>>,
    },
    ProjectionWindowFromIndex {
        request: ProjectionWindowRequest,
        reply: oneshot::Sender<Result<ProjectionWindow, SessionError>>,
    },
    EventsRange {
        start_sequence: u64,
        end_sequence: u64,
        max_events: usize,
        reply: oneshot::Sender<Result<Vec<SessionEvent>, SessionError>>,
    },
    InputHistory(oneshot::Sender<Result<Vec<SessionInputHistoryEntry>, SessionError>>),
    InputHistoryPage {
        before_sequence: Option<u64>,
        limit: usize,
        reply: oneshot::Sender<Result<Vec<SessionInputHistoryEntry>, SessionError>>,
    },
    CurrentContextEpoch(oneshot::Sender<Result<u64, SessionError>>),
    CurrentRequestContextOccupancy(
        oneshot::Sender<
            Result<Option<bcode_session_models::RequestContextOccupancy>, SessionError>,
        >,
    ),
    ModelContextEvents(oneshot::Sender<Result<Vec<SessionEvent>, SessionError>>),
    ActiveToolRuns(oneshot::Sender<Result<Vec<crate::db::ToolRun>, SessionError>>),
    ActiveRuntimeWork(oneshot::Sender<Result<Vec<crate::db::RuntimeWorkProjection>, SessionError>>),
    CurrentRuntimeSelection(oneshot::Sender<crate::SessionRuntimeSelection>),
    CurrentModelSelection(oneshot::Sender<(Option<String>, Option<String>)>),
    CurrentReasoningSelection(oneshot::Sender<(Option<String>, Option<String>)>),
    RememberedReasoningForModel {
        scope: bcode_session_models::ModelScopeKey,
        reply: oneshot::Sender<Option<crate::state::RememberedReasoning>>,
    },
    CurrentAgentSelection(oneshot::Sender<Option<String>>),
    SetCurrentAgent {
        agent_id: String,
        reply: oneshot::Sender<Result<(), SessionError>>,
    },
    PublishLive {
        event: SessionLiveEventKind,
        reply: oneshot::Sender<Option<SessionLiveEvent>>,
    },
    ReleaseIdleResources(oneshot::Sender<bool>),
    ReleaseDatabaseResources(oneshot::Sender<bool>),
    AcquireOwnership {
        kind: SessionOwnershipKind,
        reply: oneshot::Sender<Result<SessionOwnershipGuard, SessionError>>,
    },
    OwnershipSnapshot(oneshot::Sender<SessionOwnershipSnapshot>),
    ReleaseOwnershipIfQuiescent(oneshot::Sender<SessionOwnershipRelease>),
    AdoptLease {
        lease: Arc<SessionLeaseGuard>,
        reply: oneshot::Sender<()>,
    },
    Shutdown(oneshot::Sender<()>),
}

struct SessionActor {
    state: SessionState,
    store: Option<SessionStoreExecutor>,
    lease: Option<Arc<SessionLeaseGuard>>,
    ownership_guards: BTreeMap<SessionOwnershipKind, u64>,
    db: Option<SessionDb>,
    /// Summary identity of the most recent successfully written `manifest.json`.
    ///
    /// The manifest is a disposable display cache; tracking what it currently contains lets the
    /// append path skip rewriting it when a durable event did not change any manifest field.
    last_manifest_summary: Option<ManifestSummaryIdentity>,
    commands: mpsc::Receiver<SessionCommand>,
    ownership_releases: mpsc::UnboundedSender<OwnershipRelease>,
    ownership_release_receiver: mpsc::UnboundedReceiver<OwnershipRelease>,
    snapshot: Arc<RwLock<SessionSnapshot>>,
}

impl SessionActor {
    async fn run(mut self) {
        let context = MetricsContext::new().with_session_id(&self.state.summary.id);
        loop {
            let command = if self.db.is_some() {
                tokio::select! {
                    release = self.ownership_release_receiver.recv() => {
                        if let Some(release) = release {
                            self.release_ownership(release.kind, release.lease).await;
                        }
                        continue;
                    }
                    result = tokio::time::timeout(
                        SESSION_DATABASE_IDLE_TIMEOUT,
                        self.commands.recv(),
                    ) => {
                        if let Ok(command) = result {
                            command
                        } else {
                            if self.release_database_resources().await
                                == DatabaseRelease::Released
                            {
                                tracing::debug!(
                                    session_id = %self.state.summary.id,
                                    "released idle session database handle"
                                );
                            }
                            continue;
                        }
                    }
                }
            } else {
                tokio::select! {
                    release = self.ownership_release_receiver.recv() => {
                        if let Some(release) = release {
                            self.release_ownership(release.kind, release.lease).await;
                        }
                        continue;
                    }
                    command = self.commands.recv() => command,
                }
            };
            let Some(command) = command else {
                break;
            };
            let should_shutdown =
                bcode_metrics::scope_metrics_context(context.clone(), self.handle_command(command))
                    .await;
            if should_shutdown {
                break;
            }
        }
    }

    async fn handle_command(&mut self, command: SessionCommand) -> bool {
        match command {
            SessionCommand::AppendEvent {
                kind,
                provenance,
                activity_timestamp_ms,
                reply,
            } => {
                let _ = reply.send(
                    self.append_event(kind, provenance, activity_timestamp_ms)
                        .await,
                );
            }
            SessionCommand::AppendToolInvocationResult {
                record,
                activity_timestamp_ms,
                reply,
            } => {
                let _ = reply.send(
                    self.append_tool_invocation_result(record, activity_timestamp_ms)
                        .await,
                );
            }
            SessionCommand::AppendUserMessage {
                client_id,
                text,
                admission,
                activity_timestamp_ms,
                reply,
            } => {
                let _ = reply.send(
                    self.append_user_message(client_id, text, admission, activity_timestamp_ms)
                        .await,
                );
            }
            SessionCommand::Attach {
                client_id,
                mode,
                queued_at,
                reply,
            } => {
                let result = self.attach(client_id, mode, queued_at).await;
                if result.is_err() && self.state.clients.is_empty() && !self.has_ownership_guards()
                {
                    let _ = self.release_idle_resources().await;
                }
                if let Err(undelivered) = reply.send(result)
                    && undelivered.is_ok()
                {
                    let _ = self.detach(client_id).await;
                }
            }
            SessionCommand::SetComposerDraft {
                text,
                updated_at_ms,
                reply,
            } => {
                let _ = reply.send(self.set_composer_draft(&text, updated_at_ms).await);
            }
            command => return self.handle_read_command(command).await,
        }
        false
    }

    #[allow(clippy::too_many_lines)]
    async fn handle_read_command(&mut self, command: SessionCommand) -> bool {
        match command {
            SessionCommand::AppendEvent { .. }
            | SessionCommand::AppendToolInvocationResult { .. }
            | SessionCommand::AppendUserMessage { .. }
            | SessionCommand::Attach { .. }
            | SessionCommand::SetComposerDraft { .. } => {
                unreachable!("write commands are handled before read commands")
            }
            SessionCommand::SubscribeEvents(reply) => {
                let _ = reply.send(Ok(self.subscribe_events()));
            }
            SessionCommand::Detach { client_id, reply } => {
                let _ = reply.send(Ok(self.detach(client_id).await));
            }
            SessionCommand::Summary(reply) => {
                let _ = reply.send(self.state.summary());
            }
            SessionCommand::WorkingDirectory(reply) => {
                let _ = reply.send(self.state.working_directory.clone());
            }
            SessionCommand::ComposerDraft(reply) => {
                let _ = reply.send(self.composer_draft().await);
            }
            SessionCommand::ValidateWriteReadiness(reply) => {
                let _ = reply.send(self.validate_write_readiness().await);
            }
            SessionCommand::ReloadStateAndValidateWriteReadiness(reply) => {
                let _ = reply.send(self.reload_state_and_validate_write_readiness().await);
            }
            SessionCommand::Health { root, reply } => {
                let _ = reply.send(self.health(&root).await);
            }
            SessionCommand::History(reply) => {
                let _ = reply.send(self.history().await);
            }
            SessionCommand::HistoryPage { query, reply } => {
                let _ = reply.send(self.history_page(query).await);
            }
            SessionCommand::HistoryAround { query, reply } => {
                let _ = reply.send(self.history_around(query).await);
            }
            SessionCommand::InspectionPage { query, reply } => {
                let _ = reply.send(self.inspection_page(query).await);
            }
            SessionCommand::ProjectionWindowFromIndex { request, reply } => {
                let _ = reply.send(self.projection_window(request).await);
            }
            SessionCommand::EventsRange {
                start_sequence,
                end_sequence,
                max_events,
                reply,
            } => {
                let _ = reply.send(
                    self.events_range(start_sequence, end_sequence, max_events)
                        .await,
                );
            }
            SessionCommand::InputHistory(reply) => {
                let _ = reply.send(self.input_history().await);
            }
            SessionCommand::InputHistoryPage {
                before_sequence,
                limit,
                reply,
            } => {
                let _ = reply.send(self.input_history_page(before_sequence, limit).await);
            }
            SessionCommand::CurrentContextEpoch(reply) => {
                let result = if let Ok(Some(db)) = self.existing_session_db().await {
                    db.current_context_epoch().await.map_err(SessionError::from)
                } else {
                    Ok(self.state.context_epoch)
                };
                let _ = reply.send(result);
            }
            SessionCommand::CurrentRequestContextOccupancy(reply) => {
                let _ = reply.send(self.current_context_occupancy().await);
            }
            SessionCommand::ModelContextEvents(reply) => {
                let _ = reply.send(self.model_context_events().await);
            }
            SessionCommand::ActiveToolRuns(reply) => {
                let _ = reply.send(self.active_tool_runs().await);
            }
            SessionCommand::ActiveRuntimeWork(reply) => {
                let _ = reply.send(self.active_runtime_work().await);
            }
            SessionCommand::CurrentRuntimeSelection(reply) => {
                let _ = reply.send(crate::SessionRuntimeSelection {
                    agent_id: self.state.current_agent.clone(),
                    provider_plugin_id: self.state.current_provider.clone(),
                    model_id: self.state.current_model.clone(),
                    model_selection_source: self.state.model_selection_source,
                    reasoning_effort: self.state.reasoning_effort.clone(),
                    reasoning_summary: self.state.reasoning_summary.clone(),
                });
            }
            SessionCommand::CurrentModelSelection(reply) => {
                let _ = reply.send((
                    self.state.current_provider.clone(),
                    self.state.current_model.clone(),
                ));
            }
            SessionCommand::CurrentReasoningSelection(reply) => {
                let _ = reply.send((
                    self.state.reasoning_effort.clone(),
                    self.state.reasoning_summary.clone(),
                ));
            }
            SessionCommand::RememberedReasoningForModel { scope, reply } => {
                let _ = reply.send(self.state.reasoning_by_model.get(&scope).cloned());
            }
            SessionCommand::CurrentAgentSelection(reply) => {
                let _ = reply.send(self.state.current_agent.clone());
            }
            SessionCommand::SetCurrentAgent { agent_id, reply } => {
                self.set_current_agent(agent_id);
                let _ = reply.send(Ok(()));
            }
            SessionCommand::PublishLive { event, reply } => {
                let _ = reply.send(self.publish_live_event(event));
            }
            SessionCommand::ReleaseIdleResources(reply) => {
                let _ = reply.send(self.release_idle_resources().await);
            }
            SessionCommand::ReleaseDatabaseResources(reply) => {
                let released = self.release_database_resources().await == DatabaseRelease::Released;
                let _ = reply.send(released);
            }
            SessionCommand::AcquireOwnership { kind, reply } => {
                let _ = reply.send(self.acquire_ownership(kind));
            }
            SessionCommand::OwnershipSnapshot(reply) => {
                let _ = reply.send(self.ownership_snapshot());
            }
            SessionCommand::ReleaseOwnershipIfQuiescent(reply) => {
                let _ = reply.send(self.release_ownership_if_quiescent().await);
            }
            SessionCommand::AdoptLease { lease, reply } => {
                self.lease = Some(lease);
                self.refresh_snapshot();
                if self.state.clients.is_empty() && !self.has_ownership_guards() {
                    let _ = self.release_idle_resources().await;
                }
                let _ = reply.send(());
            }
            SessionCommand::Shutdown(reply) => {
                let _ = reply.send(());
                return true;
            }
        }
        false
    }

    fn replace_persisted_state(&mut self, mut state: SessionState) {
        // Broadcast brokers and attached clients belong to the actor's lifetime, not to the
        // persisted snapshot. Replacing them would strand existing session forwarders on closed
        // receivers after an idle database reload.
        state.clients.clone_from(&self.state.clients);
        state.summary.client_count = state.clients.len();
        state.sender = self.state.sender.clone();
        state.live_events = self.state.live_events.clone();
        self.state = state;
        self.refresh_snapshot();
    }

    fn refresh_snapshot(&self) {
        *self
            .snapshot
            .write()
            .expect("session snapshot lock poisoned") =
            SessionSnapshot::from_state(&self.state, self.lease.is_some());
    }

    async fn release_idle_resources(&mut self) -> bool {
        matches!(
            self.release_ownership_if_quiescent().await,
            SessionOwnershipRelease::Released
        )
    }

    async fn release_ownership_if_quiescent(&mut self) -> SessionOwnershipRelease {
        let snapshot = self.ownership_snapshot();
        let outcome = if snapshot.is_quiescent() {
            match self.release_database_resources().await {
                // The connection could not be proven terminal. Report a blocker instead of
                // claiming release, so a retained handle is loud rather than invisible.
                DatabaseRelease::Retained => {
                    let mut blocked = snapshot.clone();
                    blocked.database_handle_retained = true;
                    SessionOwnershipRelease::Blocked(blocked)
                }
                released => {
                    let released_database = released == DatabaseRelease::Released;
                    let released_lease = self.lease.take().is_some();
                    if released_lease {
                        self.refresh_snapshot();
                    }
                    if released_database || released_lease {
                        SessionOwnershipRelease::Released
                    } else {
                        SessionOwnershipRelease::AlreadyUnowned
                    }
                }
            }
        } else {
            SessionOwnershipRelease::Blocked(snapshot)
        };
        self.record_ownership_release(&outcome);
        outcome
    }

    fn record_ownership_release(&self, outcome: &SessionOwnershipRelease) {
        let Some(metrics) = self.store.as_ref().map(SessionStoreExecutor::metrics) else {
            return;
        };
        let mut labels = bcode_metrics::MetricLabels::new();
        let outcome_label = match outcome {
            SessionOwnershipRelease::Released => "released",
            SessionOwnershipRelease::AlreadyUnowned => "already_unowned",
            SessionOwnershipRelease::Blocked(_) => "blocked",
        };
        labels.insert("outcome".to_owned(), outcome_label.to_owned());
        metrics.add_counter_with_labels("session.ownership.release_total", 1, labels);
        if let SessionOwnershipRelease::Blocked(snapshot) = outcome {
            if snapshot.attached_clients > 0 {
                let mut labels = bcode_metrics::MetricLabels::new();
                labels.insert("blocker".to_owned(), "attached_client".to_owned());
                metrics.add_counter_with_labels(
                    "session.ownership.release_blocked_total",
                    1,
                    labels,
                );
            }
            for kind in snapshot.guards.keys() {
                let mut labels = bcode_metrics::MetricLabels::new();
                labels.insert("blocker".to_owned(), kind.label().to_owned());
                metrics.add_counter_with_labels(
                    "session.ownership.release_blocked_total",
                    1,
                    labels,
                );
            }
            if snapshot.database_handle_retained {
                let mut labels = bcode_metrics::MetricLabels::new();
                labels.insert("blocker".to_owned(), "database_handle_retained".to_owned());
                metrics.add_counter_with_labels(
                    "session.ownership.release_blocked_total",
                    1,
                    labels,
                );
            }
        }
    }

    /// Close and drop the cached session database handle.
    ///
    /// Dropping the handle alone is not sufficient. `SessionDb` wraps a shared connection, so a
    /// surviving clone would keep the backend connection — and its process-level file locks —
    /// alive after the lease is gone, producing a lock with no owner record. Release therefore
    /// closes the connection through the backend lifecycle and then proves it is terminal.
    async fn release_database_resources(&mut self) -> DatabaseRelease {
        let Some(db) = self.db.take() else {
            return DatabaseRelease::AlreadyReleased;
        };
        // The database is going idle, so make the display cache converge on the final summary
        // even when recent appends deferred an activity-only manifest rewrite.
        self.flush_deferred_manifest().await;
        if let Err(error) = db.close().await {
            tracing::warn!(
                session_id = %self.state.summary.id,
                "failed to close session database on release: {error}"
            );
        }
        // A close error may still have released the connection, and a successful close must still
        // be confirmed, so terminality is decided by probing rather than by the close result.
        match db.is_closed().await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    session_id = %self.state.summary.id,
                    "session database connection remained open after close; retaining ownership"
                );
                self.db = Some(db);
                return DatabaseRelease::Retained;
            }
            Err(error) => {
                tracing::warn!(
                    session_id = %self.state.summary.id,
                    "could not verify session database closed; retaining ownership: {error}"
                );
                self.db = Some(db);
                return DatabaseRelease::Retained;
            }
        }
        drop(db);
        self.state.events = None;
        self.state.load_status = SessionLoadStatusKind::SummaryOnly;
        self.refresh_snapshot();
        DatabaseRelease::Released
    }

    fn ensure_ownership(&mut self) -> Result<(), SessionError> {
        if self.store.is_none() || self.lease.is_some() {
            return Ok(());
        }
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::NotFound(self.state.summary.id))?;
        let reacquiring = matches!(self.state.load_status, SessionLoadStatusKind::SummaryOnly);
        self.lease = Some(Arc::new(crate::lease::acquire_session_lease(
            &store.root_path(),
            self.state.summary.id,
            store.lease_owner(),
        )?));
        if reacquiring {
            store
                .metrics()
                .increment_counter("session.ownership.reacquired_total");
        }
        self.refresh_snapshot();
        Ok(())
    }

    fn acquire_ownership(
        &mut self,
        kind: SessionOwnershipKind,
    ) -> Result<SessionOwnershipGuard, SessionError> {
        self.ensure_ownership()?;
        let count = self.ownership_guards.entry(kind).or_default();
        *count = count.saturating_add(1);
        if let Some(metrics) = self.store.as_ref().map(SessionStoreExecutor::metrics) {
            let mut labels = bcode_metrics::MetricLabels::new();
            labels.insert("kind".to_owned(), kind.label().to_owned());
            metrics.add_counter_with_labels("session.ownership.guard_acquired_total", 1, labels);
        }
        Ok(SessionOwnershipGuard {
            _inner: Arc::new(SessionOwnershipGuardInner {
                releases: self.ownership_releases.clone(),
                kind,
                lease: self.lease.clone(),
            }),
        })
    }

    async fn release_ownership(
        &mut self,
        kind: SessionOwnershipKind,
        lease: Option<Arc<SessionLeaseGuard>>,
    ) {
        let Some(count) = self.ownership_guards.get_mut(&kind) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.ownership_guards.remove(&kind);
        }
        drop(lease);
        if self.state.clients.is_empty() && !self.has_ownership_guards() {
            let _ = self.release_idle_resources().await;
        }
    }

    fn ownership_snapshot(&self) -> SessionOwnershipSnapshot {
        SessionOwnershipSnapshot {
            attached_clients: self.state.clients.len(),
            guards: self.ownership_guards.clone(),
            database_handle_retained: false,
        }
    }

    fn has_ownership_guards(&self) -> bool {
        self.ownership_guards.values().any(|count| *count > 0)
    }

    async fn set_composer_draft(
        &mut self,
        text: &str,
        updated_at_ms: u64,
    ) -> Result<(), SessionError> {
        self.ensure_ownership()?;
        let session_id = self.state.summary.id;
        let root_path = self
            .store
            .as_ref()
            .ok_or(SessionError::NotFound(session_id))?
            .root_path();
        let Some(db) = self.existing_session_db().await? else {
            return Ok(());
        };
        let _write_guard = crate::lease::acquire_session_write_lock(&root_path, session_id)?;
        db.set_session_composer_draft(text, updated_at_ms).await?;
        Ok(())
    }

    async fn composer_draft(&mut self) -> Result<Option<String>, SessionError> {
        let Some(db) = self.existing_session_db().await? else {
            return Ok(None);
        };
        Ok(db.session_composer_draft().await?)
    }

    async fn validate_write_readiness(&mut self) -> Result<(), SessionError> {
        self.ensure_ownership()?;
        let db = self.session_db_for_write().await?;
        db.validate_write_readiness().await?;
        Ok(())
    }

    /// Reload persisted state through the cached write handle and validate append readiness.
    ///
    /// This is the actor-side counterpart of a manager-level summary refresh: it reuses the
    /// actor's open database (opening it once if needed) instead of the caller opening a second
    /// connection to the same file, which would take a second exclusive lock cycle.
    async fn reload_state_and_validate_write_readiness(&mut self) -> Result<(), SessionError> {
        self.ensure_ownership()?;
        self.ensure_session_db_for_write().await?;
        {
            let db = self
                .db
                .as_ref()
                .ok_or(SessionError::DbUnavailable(self.state.summary.id))?;
            db.validate_write_readiness().await?;
        }
        self.reload_state_from_db().await?;
        self.state.load_status = SessionLoadStatusKind::Current;
        self.refresh_snapshot();
        Ok(())
    }

    /// Bounded health through the cached database, opening it once if the file exists.
    ///
    /// Unlike write paths this does not require ownership: health is a read-only probe, and the
    /// actor holding the handle is exactly what makes a second manager-side open redundant.
    async fn health(
        &mut self,
        root: &std::path::Path,
    ) -> Result<super::SessionHealth, SessionError> {
        let session_id = self.state.summary.id;
        let db = self
            .existing_session_db()
            .await?
            .ok_or(SessionError::NotFound(session_id))?;
        Ok(super::session_health_from_db(db, root, session_id).await)
    }

    /// Replace in-memory state from the durable session-state projection unconditionally.
    async fn reload_state_from_db(&mut self) -> Result<(), SessionError> {
        let session_id = self.state.summary.id;
        let summary_created_at_ms = self.state.summary.created_at_ms;
        let summary_updated_at_ms = self.state.summary.updated_at_ms;
        let Some(db) = self.db.as_ref() else {
            return Err(SessionError::DbUnavailable(session_id));
        };
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
            .unwrap_or(summary_created_at_ms);
        let updated_at_ms = db_state
            .updated_at_ms
            .or_else(|| activity_bounds.map(|(_, updated_at_ms)| updated_at_ms))
            .unwrap_or(summary_updated_at_ms);
        let state = SessionState::from_db_state(db_state, created_at_ms, updated_at_ms);
        self.replace_persisted_state(state);
        Ok(())
    }

    /// Return the cached write-capable session database, opening or initializing it if needed.
    ///
    /// This intentionally returns a borrow rather than a clone. `SessionDb` wraps a shared
    /// connection, so handing out clones would let a backend connection — and its process-level
    /// file locks — outlive ownership release. Borrowing keeps the actor the sole owner.
    async fn session_db_for_write(&mut self) -> Result<&SessionDb, SessionError> {
        if self.db.is_none() {
            let store = self
                .store
                .as_ref()
                .ok_or(SessionError::NotFound(self.state.summary.id))?;
            let db_path = crate::db::session_db_path(&store.root_path(), self.state.summary.id);
            let db = if db_path.exists() {
                SessionDb::open_runtime_turso_in_root_observed(
                    self.state.summary.id,
                    &store.root_path(),
                    store.metrics(),
                )
                .await?
            } else {
                SessionDb::initialize_turso_in_root_observed(
                    self.state.summary.id,
                    &store.root_path(),
                    store.metrics(),
                )
                .await?
            };
            self.db = Some(db);
        }
        self.db
            .as_ref()
            .ok_or(SessionError::DbUnavailable(self.state.summary.id))
    }

    /// Return the cached session database when canonical storage already exists.
    ///
    /// Returns a borrow rather than a clone for the same ownership reason as
    /// [`Self::session_db_for_write`].
    async fn existing_session_db(&mut self) -> Result<Option<&SessionDb>, SessionError> {
        if self.db.is_none() {
            let Some(store) = &self.store else {
                return Ok(None);
            };
            if !crate::db::session_db_path(&store.root_path(), self.state.summary.id).exists() {
                return Ok(None);
            }
            let db = SessionDb::open_existing_turso_in_root_observed(
                self.state.summary.id,
                &store.root_path(),
                store.metrics(),
            )
            .await?;
            self.db = Some(db);
        }
        Ok(self.db.as_ref())
    }

    /// Ensure the write-capable database is open without holding a borrow across the call.
    async fn ensure_session_db_for_write(&mut self) -> Result<(), SessionError> {
        self.session_db_for_write().await.map(|_| ())
    }

    /// Reconcile in-memory state with durable projections before a write.
    ///
    /// This reads every value it needs into owned locals so the database borrow ends before the
    /// actor mutates itself, keeping the actor the sole owner of the handle.
    async fn refresh_state_from_db_for_write(&mut self) -> Result<(), SessionError> {
        let session_id = self.state.summary.id;
        let next_sequence = self.state.next_sequence;
        let summary_created_at_ms = self.state.summary.created_at_ms;
        let summary_updated_at_ms = self.state.summary.updated_at_ms;
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        let Some(db_state) = db.session_state().await? else {
            return Ok(());
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
        if expected_last_sequence.saturating_add(1) == next_sequence {
            return Ok(());
        }
        let activity_bounds = db.activity_bounds().await?;
        let created_at_ms = activity_bounds
            .map(|(created_at_ms, _)| created_at_ms)
            .or(db_state.updated_at_ms)
            .unwrap_or(summary_created_at_ms);
        let updated_at_ms = db_state
            .updated_at_ms
            .or_else(|| activity_bounds.map(|(_, updated_at_ms)| updated_at_ms))
            .unwrap_or(summary_updated_at_ms);
        let state = SessionState::from_db_state(db_state, created_at_ms, updated_at_ms);
        self.replace_persisted_state(state);
        Ok(())
    }

    async fn append_tool_invocation_result(
        &mut self,
        record: bcode_session_models::ToolInvocationResultRecord,
        activity_timestamp_ms: u64,
    ) -> Result<SessionEvent, SessionError> {
        if let Some(db) = self.existing_session_db().await?
            && let Some(tool_run) = db.tool_run(&record.invocation_id).await?
            && let Some(event_sequence) = tool_run.event_seq_end
            && let Some(event) = db
                .events_range(event_sequence, event_sequence, 1)
                .await?
                .pop()
            && matches!(
                &event.kind,
                SessionEventKind::ToolInvocationResultRecorded { record: existing }
                    if existing.invocation_id == record.invocation_id
            )
        {
            return Ok(event);
        }
        if let Some(event) = self.state.events.as_ref().and_then(|events| {
            events.iter().rev().find(|event| {
                matches!(
                    &event.kind,
                    SessionEventKind::ToolInvocationResultRecorded { record: existing }
                        if existing.invocation_id == record.invocation_id
                )
            })
        }) {
            return Ok(event.clone());
        }
        self.append_event(
            SessionEventKind::ToolInvocationResultRecorded { record },
            None,
            activity_timestamp_ms,
        )
        .await
    }

    async fn append_event(
        &mut self,
        kind: SessionEventKind,
        provenance: Option<SessionEventProvenance>,
        activity_timestamp_ms: u64,
    ) -> Result<SessionEvent, SessionError> {
        self.ensure_ownership()?;
        let total_started_at = Instant::now();
        let metrics = self.store.as_ref().map(SessionStoreExecutor::metrics);
        crate::ensure_durable_session_event_kind(&kind, metrics.as_ref())?;
        if let Some(metrics) = &metrics {
            metrics.increment_counter("session.actor.append_event.total");
        }
        let event_timestamp_ms = provenance
            .as_ref()
            .and_then(|provenance| provenance.source_timestamp_ms)
            .unwrap_or(activity_timestamp_ms);
        let event = if let Some(store) = self.store.clone() {
            let lock_started_at = Instant::now();
            let _write_guard = crate::lease::acquire_session_write_lock(
                &store.root_path(),
                self.state.summary.id,
            )?;
            if let Some(metrics) = &metrics {
                metrics.record_histogram(
                    "session.actor.append_event.write_lock_duration_ms",
                    elapsed_ms(lock_started_at),
                );
            }
            let readiness_started_at = Instant::now();
            self.ensure_session_db_for_write().await?;
            self.refresh_state_from_db_for_write().await?;
            if let Some(metrics) = &metrics {
                metrics.record_histogram(
                    "session.actor.append_event.readiness_duration_ms",
                    elapsed_ms(readiness_started_at),
                );
            }
            let mut event = self.state.build_next_event(kind, event_timestamp_ms);
            event.provenance = provenance;
            let db_append_started_at = Instant::now();
            let db = self
                .db
                .as_ref()
                .ok_or(SessionError::DbUnavailable(self.state.summary.id))?;
            let append_result = db
                .append_event_with_activity_timestamp(&event, Some(event_timestamp_ms))
                .await;
            if let Some(metrics) = &metrics {
                record_append_rejection_metrics(metrics, &append_result);
            }
            append_result?;
            if let Some(metrics) = &metrics {
                metrics.record_histogram(
                    "session.actor.append_event.db_append_duration_ms",
                    elapsed_ms(db_append_started_at),
                );
                crate::record_session_event_domain_metrics(metrics, &event);
            }
            event
        } else {
            let mut event = self.state.build_next_event(kind, event_timestamp_ms);
            event.provenance = provenance;
            event
        };
        self.state
            .apply_persisted_event(event.clone(), activity_timestamp_ms);
        self.retire_live_text_checkpoint_for_durable_event(&event.kind);
        let manifest_started_at = Instant::now();
        self.update_manifest_and_schedule_catalog().await;
        if let Some(metrics) = &metrics {
            metrics.record_histogram(
                "session.actor.append_event.manifest_duration_ms",
                elapsed_ms(manifest_started_at),
            );
        }
        self.state.load_status = SessionLoadStatusKind::Current;
        self.refresh_snapshot();
        if let Some(metrics) = &metrics {
            metrics.record_histogram(
                "session.actor.append_event.duration_ms",
                elapsed_ms(total_started_at),
            );
        }
        Ok(event)
    }

    /// Write the manifest now if any deferred activity-only refresh is pending.
    async fn flush_deferred_manifest(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        let summary = self.state.summary();
        let identity = ManifestSummaryIdentity::from_summary(&summary);
        if self.last_manifest_summary.as_ref() == Some(&identity) {
            return;
        }
        if store.write_session_manifest(summary).await.is_ok() {
            store
                .metrics()
                .increment_counter("session.manifest.write_total");
            self.last_manifest_summary = Some(identity);
        }
    }

    /// Refresh the disposable `manifest.json` display cache and enqueue the catalog update.
    ///
    /// `manifest.json` mirrors the session summary for discovery when the catalog is unavailable.
    /// Most appends (assistant deltas, tool lifecycle, usage) change only `updated_at_ms`, and
    /// rewriting the file for each one is a blocking write+rename on the append hot path. The
    /// manifest is therefore rewritten when any display field other than activity time changes,
    /// when the activity time has drifted by more than [`MANIFEST_ACTIVITY_REFRESH_INTERVAL_MS`],
    /// or on ownership release (see [`Self::flush_deferred_manifest`]), so it converges shortly
    /// after activity stops without costing a write per event. The catalog coordinator coalesces
    /// its own updates independently.
    async fn update_manifest_and_schedule_catalog(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        let summary = self.state.summary();
        let identity = ManifestSummaryIdentity::from_summary(&summary);
        let stale = match &self.last_manifest_summary {
            None => true,
            Some(last) => {
                last.display != identity.display
                    || summary.updated_at_ms.saturating_sub(last.updated_at_ms)
                        >= MANIFEST_ACTIVITY_REFRESH_INTERVAL_MS
            }
        };
        if stale {
            if let Err(error) = store.write_session_manifest(summary.clone()).await {
                store
                    .metrics()
                    .increment_counter("session.manifest.write_error_total");
                eprintln!("failed to write session manifest: {error}");
            } else {
                store
                    .metrics()
                    .increment_counter("session.manifest.write_total");
                self.last_manifest_summary = Some(identity);
            }
        } else {
            store
                .metrics()
                .increment_counter("session.manifest.write_skipped_total");
        }
        store.schedule_catalog_summary(summary).await;
    }

    async fn append_user_message(
        &mut self,
        client_id: ClientId,
        text: String,
        admission: bcode_session_models::TurnAdmissionMetadata,
        activity_timestamp_ms: u64,
    ) -> Result<TurnAdmissionResult, SessionError> {
        admission.validate()?;
        if let Some((producer, idempotency_key)) = admission.idempotency_identity() {
            let identity = (producer.to_owned(), idempotency_key.to_owned());
            if let Some(receipt) = self.state.turn_receipts.get(&identity) {
                return Ok(TurnAdmissionResult {
                    admission: bcode_session_models::TurnAdmission::Existing(receipt.clone()),
                    events: Vec::new(),
                });
            }
            if let Some(db) = self.existing_session_db().await?
                && let Some(receipt) = db.turn_receipt(producer, idempotency_key).await?
            {
                self.state.turn_receipts.insert(identity, receipt.clone());
                return Ok(TurnAdmissionResult {
                    admission: bcode_session_models::TurnAdmission::Existing(receipt),
                    events: Vec::new(),
                });
            }
        }
        let mut events = Vec::new();
        if self.state.summary.name.is_none() && !self.state.has_user_message {
            let title = title_from_first_prompt(&text);
            events.push(
                self.append_event(
                    SessionEventKind::SessionRenamed { name: Some(title) },
                    None,
                    activity_timestamp_ms,
                )
                .await?,
            );
        }
        let event = self
            .append_event(
                SessionEventKind::UserMessage {
                    client_id,
                    text,
                    admission: admission.clone(),
                },
                None,
                activity_timestamp_ms,
            )
            .await?;
        let receipt = bcode_session_models::TurnReceipt::from_accepted_event(
            event.session_id,
            event.sequence,
        );
        if let Some((producer, idempotency_key)) = admission.idempotency_identity() {
            self.state.turn_receipts.insert(
                (producer.to_owned(), idempotency_key.to_owned()),
                receipt.clone(),
            );
        }
        events.push(event);
        Ok(TurnAdmissionResult {
            admission: bcode_session_models::TurnAdmission::Accepted(receipt),
            events,
        })
    }

    const fn attach_mode_label(mode: &AttachMode) -> &'static str {
        match mode {
            AttachMode::Full => "full",
            AttachMode::Recent { .. } => "recent",
            AttachMode::ProjectionWindow { .. } => "projection_window",
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn attach(
        &mut self,
        client_id: ClientId,
        mode: AttachMode,
        queued_at: Instant,
    ) -> Result<SessionAttachment, SessionError> {
        self.ensure_ownership()?;
        let total_started_at = Instant::now();
        let metrics = self.store.as_ref().map(SessionStoreExecutor::metrics);
        if let Some(metrics) = &metrics {
            let mut labels = bcode_metrics::MetricLabels::new();
            labels.insert("mode".to_owned(), Self::attach_mode_label(&mode).to_owned());
            metrics.add_counter_with_labels("session.actor.attach.total", 1, labels.clone());
            metrics.record_histogram_with_labels(
                "session.actor.attach.queue_wait_duration_ms",
                elapsed_ms(queued_at),
                labels,
            );
        }
        let writable_started_at = Instant::now();
        if let Some(metrics) = &metrics {
            metrics.record_histogram(
                "session.actor.attach.ensure_writable_duration_ms",
                elapsed_ms(writable_started_at),
            );
        }
        let history_started_at = Instant::now();
        let history = match mode {
            AttachMode::Full => self.history().await?,
            AttachMode::Recent { limit } => {
                if let Some(metrics) = &metrics {
                    metrics
                        .record_histogram("session.actor.attach.recent_limit", usize_to_u64(limit));
                }
                if let Some(history) = self.recent_history_from_db(limit).await? {
                    history
                } else if self.store.is_some() {
                    return Err(SessionError::NotFound(self.state.summary.id));
                } else {
                    self.history()
                        .await?
                        .into_iter()
                        .rev()
                        .take(limit)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect()
                }
            }
            AttachMode::ProjectionWindow { history } => history,
        };
        if let Some(metrics) = &metrics {
            metrics.record_histogram(
                "session.actor.attach.history_duration_ms",
                elapsed_ms(history_started_at),
            );
            metrics.record_histogram(
                "session.actor.attach.history_event_count",
                usize_to_u64(history.len()),
            );
        }
        let input_history_started_at = Instant::now();
        let input_history = self.input_history().await?;
        let usage_summary = self.session_usage_summary().await?;
        if let Some(metrics) = &metrics {
            metrics.record_histogram(
                "session.actor.attach.input_history_duration_ms",
                elapsed_ms(input_history_started_at),
            );
            metrics.record_histogram(
                "session.actor.attach.input_history_entry_count",
                usize_to_u64(input_history.len()),
            );
        }
        let subscribe_started_at = Instant::now();
        self.state.clients.insert(client_id);
        self.state.summary.client_count = self.state.clients.len();
        let events = self.state.sender.subscribe();
        if let Some(metrics) = &metrics {
            metrics.record_histogram(
                "session.actor.attach.subscribe_duration_ms",
                elapsed_ms(subscribe_started_at),
            );
        }
        let session = self.state.summary();
        if let Some(metrics) = &metrics {
            metrics.record_histogram(
                "session.actor.attach.total_duration_ms",
                elapsed_ms(total_started_at),
            );
        }
        Ok(SessionAttachment {
            session,
            history,
            input_history,
            usage_summary,
            live_checkpoints: self.state.live_text_checkpoints.values().cloned().collect(),
            events,
            live_events: self.state.live_events.subscribe(),
        })
    }

    fn subscribe_events(&self) -> SessionEventReceivers {
        (
            self.state.summary(),
            self.state.sender.subscribe(),
            self.state.live_events.subscribe(),
        )
    }

    async fn detach(&mut self, client_id: ClientId) -> bool {
        if self.state.clients.remove(&client_id) {
            self.state.summary.client_count = self.state.clients.len();
            self.refresh_snapshot();
            if self.state.clients.is_empty() && !self.has_ownership_guards() {
                let _ = self.release_idle_resources().await;
            }
            return true;
        }
        false
    }

    async fn history(&mut self) -> Result<Vec<SessionEvent>, SessionError> {
        if let Some(db) = self.existing_session_db().await? {
            return Ok(db.all_events().await?);
        }
        if let Some(events) = &self.state.events {
            return Ok(events.clone());
        }
        if self.store.is_some() {
            return Err(SessionError::NotFound(self.state.summary.id));
        }
        Err(SessionError::NotFound(self.state.summary.id))
    }

    async fn history_page(
        &mut self,
        query: SessionHistoryQuery,
    ) -> Result<SessionHistoryPage, SessionError> {
        if let Some(db) = self.existing_session_db().await? {
            return Ok(db.history_page(query).await?);
        }
        if let Some(events) = &self.state.events {
            return history_page_from_events(self.state.summary.id, events, query);
        }
        Err(SessionError::NotFound(self.state.summary.id))
    }

    async fn history_around(
        &mut self,
        query: SessionHistoryAroundQuery,
    ) -> Result<SessionHistoryWindow, SessionError> {
        if let Some(db) = self.existing_session_db().await? {
            return Ok(db.history_around(query).await?);
        }
        if let Some(events) = &self.state.events {
            return history_around_from_events(self.state.summary.id, events, query);
        }
        Err(SessionError::NotFound(self.state.summary.id))
    }

    async fn inspection_page(
        &mut self,
        query: SessionInspectionQuery,
    ) -> Result<SessionInspectionPage, SessionError> {
        if let Some(db) = self.existing_session_db().await? {
            return Ok(db.inspection_page(query).await?);
        }
        Err(SessionError::DbUnavailable(self.state.summary.id))
    }

    async fn projection_window(
        &mut self,
        request: ProjectionWindowRequest,
    ) -> Result<ProjectionWindow, SessionError> {
        if matches!(request.anchor, ProjectionWindowAnchor::Latest) {
            return Err(SessionError::UnsupportedProjectionWindow);
        }
        if self.store.is_some() || self.state.events.is_some() {
            return self.projection_window_from_bounded_events(&request).await;
        }
        Err(SessionError::NotFound(self.state.summary.id))
    }

    async fn projection_window_from_bounded_events(
        &mut self,
        request: &ProjectionWindowRequest,
    ) -> Result<ProjectionWindow, SessionError> {
        let max_events = request.limits.max_events_scanned.max(1);
        let max_events_u64 = u64::try_from(max_events).unwrap_or(u64::MAX);
        let (start_sequence, end_sequence) = match request.anchor {
            ProjectionWindowAnchor::BeforeSequence(sequence) => (
                sequence.saturating_sub(max_events_u64),
                sequence.saturating_sub(1),
            ),
            ProjectionWindowAnchor::AfterSequence(sequence) => (
                sequence.saturating_add(1),
                sequence.saturating_add(max_events_u64),
            ),
            ProjectionWindowAnchor::AroundSequence(sequence) => {
                let half_scan = max_events_u64 / 2;
                (
                    sequence.saturating_sub(half_scan),
                    sequence.saturating_add(half_scan),
                )
            }
            ProjectionWindowAnchor::Latest => {
                return Err(SessionError::UnsupportedProjectionWindow);
            }
        };
        let events = self
            .events_range(start_sequence, end_sequence, max_events)
            .await?;
        let (first_event_sequence, last_event_sequence) =
            if let Some(db) = self.existing_session_db().await? {
                (
                    db.first_event_sequence().await?,
                    db.last_event_sequence().await?,
                )
            } else if let Some(all_events) = &self.state.events {
                (
                    all_events.first().map(|event| event.sequence),
                    all_events.last().map(|event| event.sequence),
                )
            } else {
                (None, None)
            };
        crate::projection::projection_window_from_events_with_source_bounds(
            &events,
            first_event_sequence,
            last_event_sequence,
            request,
        )
        .ok_or(SessionError::UnsupportedProjectionWindow)
    }

    async fn events_range(
        &mut self,
        start_sequence: u64,
        end_sequence: u64,
        max_events: usize,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        if let Some(db) = self.existing_session_db().await? {
            return Ok(db
                .events_range(start_sequence, end_sequence, max_events)
                .await?);
        }
        if let Some(events) = &self.state.events {
            return Ok(select_event_range_from_events(
                events,
                start_sequence,
                end_sequence,
                max_events,
            ));
        }
        if self.store.is_some() {
            return Err(SessionError::NotFound(self.state.summary.id));
        }
        Err(SessionError::NotFound(self.state.summary.id))
    }

    async fn recent_history_from_db(
        &mut self,
        limit: usize,
    ) -> Result<Option<Vec<SessionEvent>>, SessionError> {
        let Some(db) = self.existing_session_db().await? else {
            return Ok(None);
        };
        let latest = db.last_event_sequence().await?.unwrap_or_default();
        let max_events = 2_048usize;
        let start = latest.saturating_sub(
            u64::try_from(max_events)
                .unwrap_or(u64::MAX)
                .saturating_sub(1),
        );
        let events = db.events_range(start, latest, max_events).await?;
        let request = ProjectionWindowRequest {
            projection: bcode_session_models::SessionProjectionKind::Transcript,
            anchor: ProjectionWindowAnchor::Latest,
            direction: bcode_session_models::ProjectionWindowDirection::Backward,
            target: bcode_session_models::ProjectionWindowTarget {
                min_items: Some(limit.max(1)),
                min_estimated_rows: None,
                min_bytes: None,
                width_columns: None,
            },
            limits: bcode_session_models::ProjectionWindowLimits {
                max_items: limit.max(1),
                max_bytes: 512 * 1024,
                max_events_scanned: 2_048,
            },
        };
        let window = crate::projection::projection_window_from_events_with_source_bounds(
            &events,
            db.first_event_sequence().await?,
            db.last_event_sequence().await?,
            &request,
        )
        .ok_or(SessionError::UnsupportedProjectionWindow)?;
        let Some(range) = window.source_range else {
            return Ok(Some(Vec::new()));
        };
        Ok(Some(
            db.events_range(
                range.start_sequence,
                range.end_sequence,
                request.limits.max_events_scanned,
            )
            .await?,
        ))
    }

    async fn session_usage_summary(
        &mut self,
    ) -> Result<bcode_session_models::SessionUsageSummary, SessionError> {
        let expected_last_sequence = self.state.next_sequence.saturating_sub(1);
        if let Some(db) = self.existing_session_db().await? {
            return db.session_usage_summary().await.map_err(SessionError::from);
        }
        if let Some(events) = &self.state.events {
            let mut latest = BTreeMap::new();
            for event in events {
                if let SessionEventKind::ModelUsage { usage, .. } = &event.kind {
                    let key = usage
                        .request_id
                        .clone()
                        .unwrap_or_else(|| format!("event:{}", event.sequence));
                    latest.insert(key, (event.sequence, usage.clone()));
                }
            }
            return Ok(session_usage_summary_from_observations(
                latest.into_values(),
            ));
        }
        Err(SessionError::ProjectionStale {
            session_id: self.state.summary.id,
            projection: "session_usage",
            checkpoint: None,
            expected: expected_last_sequence,
        })
    }

    async fn input_history(&mut self) -> Result<Vec<SessionInputHistoryEntry>, SessionError> {
        let session_id = self.state.summary.id;
        let expected_last_sequence = self.state.next_sequence.saturating_sub(1);
        if let Some(db) = self.existing_session_db().await? {
            let checkpoint = db
                .materialized_projection_checkpoint(MaterializedProjection::InputHistory)
                .await?;
            if checkpoint.is_some_and(|checkpoint| checkpoint >= expected_last_sequence) {
                return Ok(db.input_history().await?);
            }
            return Err(SessionError::ProjectionStale {
                session_id,
                projection: "input_history",
                checkpoint,
                expected: expected_last_sequence,
            });
        }
        if let Some(events) = &self.state.events {
            return Ok(input_history_from_events(events));
        }
        if self.store.is_some() {
            return Err(SessionError::NotFound(self.state.summary.id));
        }
        Err(SessionError::NotFound(self.state.summary.id))
    }

    async fn input_history_page(
        &mut self,
        before_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<SessionInputHistoryEntry>, SessionError> {
        let session_id = self.state.summary.id;
        let expected_last_sequence = self.state.next_sequence.saturating_sub(1);
        if let Some(db) = self.existing_session_db().await? {
            let checkpoint = db
                .materialized_projection_checkpoint(MaterializedProjection::InputHistory)
                .await?;
            if checkpoint.is_some_and(|checkpoint| checkpoint >= expected_last_sequence) {
                return Ok(db.input_history_page(before_sequence, limit).await?);
            }
            return Err(SessionError::ProjectionStale {
                session_id,
                projection: "input_history",
                checkpoint,
                expected: expected_last_sequence,
            });
        }
        if let Some(events) = &self.state.events {
            return Ok(input_history_from_events(events)
                .into_iter()
                .rev()
                .filter(|entry| before_sequence.is_none_or(|before| entry.sequence < before))
                .take(limit)
                .collect());
        }
        Err(SessionError::NotFound(self.state.summary.id))
    }

    async fn active_tool_runs(&mut self) -> Result<Vec<crate::db::ToolRun>, SessionError> {
        let session_id = self.state.summary.id;
        let expected_last_sequence = self.state.next_sequence.saturating_sub(1);
        let Some(db) = self.existing_session_db().await? else {
            return Ok(Vec::new());
        };
        let checkpoint = db
            .materialized_projection_checkpoint(MaterializedProjection::ToolRuns)
            .await?;
        if checkpoint.is_some_and(|checkpoint| checkpoint >= expected_last_sequence) {
            return Ok(db.active_tool_runs().await?);
        }
        Err(SessionError::ProjectionStale {
            session_id,
            projection: "tool_runs",
            checkpoint,
            expected: expected_last_sequence,
        })
    }

    async fn active_runtime_work(
        &mut self,
    ) -> Result<Vec<crate::db::RuntimeWorkProjection>, SessionError> {
        let session_id = self.state.summary.id;
        let expected_last_sequence = self.state.next_sequence.saturating_sub(1);
        let Some(db) = self.existing_session_db().await? else {
            return Ok(Vec::new());
        };
        let checkpoint = db
            .materialized_projection_checkpoint(MaterializedProjection::RuntimeWork)
            .await?;
        if checkpoint.is_some_and(|checkpoint| checkpoint >= expected_last_sequence) {
            return Ok(db.active_runtime_work().await?);
        }
        Err(SessionError::ProjectionStale {
            session_id,
            projection: "runtime_work",
            checkpoint,
            expected: expected_last_sequence,
        })
    }

    async fn current_context_occupancy(
        &mut self,
    ) -> Result<Option<bcode_session_models::RequestContextOccupancy>, SessionError> {
        if let Some(db) = self.existing_session_db().await? {
            return Ok(db.current_context_occupancy().await?);
        }
        if self.state.events.is_some() {
            return Ok(self.state.context_occupancy.clone());
        }
        Err(SessionError::NotFound(self.state.summary.id))
    }

    async fn model_context_events(&mut self) -> Result<Vec<SessionEvent>, SessionError> {
        let started_at = Instant::now();
        let metrics = self.store.as_ref().map(SessionStoreExecutor::metrics);
        if let Some(db) = self.existing_session_db().await? {
            let events = db.model_context_events().await?;
            if let Some(metrics) = &metrics {
                metrics.record_histogram(
                    "session.actor.model_context_events.duration_ms",
                    elapsed_ms(started_at),
                );
                metrics.record_histogram(
                    "session.actor.model_context_events.event_count",
                    usize_to_u64(events.len()),
                );
            }
            return Ok(events);
        }
        if let Some(events) = &self.state.events {
            let events = model_context_events_from_history(events);
            if let Some(metrics) = &metrics {
                metrics.record_histogram(
                    "session.actor.model_context_events.duration_ms",
                    elapsed_ms(started_at),
                );
                metrics.record_histogram(
                    "session.actor.model_context_events.event_count",
                    usize_to_u64(events.len()),
                );
            }
            return Ok(events);
        }
        if self.store.is_some() {
            return Err(SessionError::NotFound(self.state.summary.id));
        }
        Err(SessionError::NotFound(self.state.summary.id))
    }

    fn set_current_agent(&mut self, agent_id: String) {
        self.state.current_agent = Some(agent_id);
        self.refresh_snapshot();
    }

    fn publish_live_event(&mut self, kind: SessionLiveEventKind) -> Option<SessionLiveEvent> {
        self.update_live_text_checkpoint(&kind);
        let event = SessionLiveEvent {
            session_id: self.state.summary.id,
            kind,
        };
        self.state.live_events.publish(event)
    }

    fn retire_live_text_checkpoint_for_durable_event(&mut self, kind: &SessionEventKind) {
        match kind {
            SessionEventKind::AssistantResponseSegment {
                turn_id,
                segment_id,
                ..
            } => {
                let key = LiveTextStreamKey::Assistant {
                    turn_id: turn_id.clone(),
                    segment_id: segment_id.clone(),
                };
                self.retire_live_text_checkpoint_with_tombstone(key);
            }
            SessionEventKind::AssistantReasoningActivity { turn_id, activity } => {
                for part in &activity.parts {
                    let key = LiveTextStreamKey::Reasoning {
                        turn_id: turn_id.clone(),
                        activity_id: activity.activity_id.clone(),
                        part_id: part.part_id.clone(),
                    };
                    self.retire_live_text_checkpoint_with_tombstone(key);
                }
            }
            SessionEventKind::ModelTurnFinished { turn_id, .. } => {
                self.state
                    .live_text_checkpoints
                    .retain(|key, _| key.turn_id() != turn_id);
                self.state
                    .live_text_checkpoint_order
                    .retain(|key| key.turn_id() != turn_id);
                self.state
                    .live_text_tombstones
                    .retain(|key, _| key.turn_id() != turn_id);
                self.state
                    .live_text_tombstone_order
                    .retain(|key| key.turn_id() != turn_id);
            }
            _ => {}
        }
    }

    fn retire_live_text_checkpoint_with_tombstone(&mut self, key: LiveTextStreamKey) {
        let (generation, revision) = self
            .state
            .live_text_checkpoints
            .get(&key)
            .and_then(live_text_event_update)
            .map_or((0, 0), |update| (update.generation, update.revision));
        self.retire_live_text_checkpoint(&key);
        self.insert_live_text_tombstone(key, generation, revision);
    }

    fn retire_live_text_checkpoint(&mut self, key: &LiveTextStreamKey) {
        self.state.live_text_checkpoints.remove(key);
        self.state
            .live_text_checkpoint_order
            .retain(|item| item != key);
    }

    fn insert_live_text_tombstone(
        &mut self,
        key: LiveTextStreamKey,
        generation: u64,
        revision: u64,
    ) {
        if !self.state.live_text_tombstones.contains_key(&key) {
            self.state.live_text_tombstone_order.push(key.clone());
        }
        self.state
            .live_text_tombstones
            .insert(key, (generation, revision));
        while self.state.live_text_tombstones.len() > MAX_ACTIVE_TEXT_STREAM_TOMBSTONES {
            let Some(oldest) = self.state.live_text_tombstone_order.first().cloned() else {
                break;
            };
            self.state.live_text_tombstone_order.remove(0);
            self.state.live_text_tombstones.remove(&oldest);
        }
    }

    #[allow(clippy::too_many_lines)] // One actor-owned reducer keeps bounds and lifecycle transitions atomic.
    fn update_live_text_checkpoint(&mut self, kind: &SessionLiveEventKind) {
        use bcode_session_models::{TextStreamOperation, TextStreamUpdate};

        let (key, update) = match kind {
            SessionLiveEventKind::AssistantTextStreamUpdated {
                turn_id,
                segment_id,
                update,
                ..
            } => (
                LiveTextStreamKey::Assistant {
                    turn_id: turn_id.clone(),
                    segment_id: segment_id.clone(),
                },
                update,
            ),
            SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                turn_id,
                activity_id,
                part_id,
                update,
                ..
            } => (
                LiveTextStreamKey::Reasoning {
                    turn_id: turn_id.clone(),
                    activity_id: activity_id.clone(),
                    part_id: part_id.clone(),
                },
                update,
            ),
            _ => return,
        };

        if let Some((terminal_generation, _)) = self.state.live_text_tombstones.get(&key).copied() {
            if update.generation <= terminal_generation {
                return;
            }
            self.state.live_text_tombstones.remove(&key);
            self.state
                .live_text_tombstone_order
                .retain(|item| item != &key);
        }
        if let Some(current_generation) = self
            .state
            .live_text_checkpoints
            .get(&key)
            .and_then(live_text_event_update)
            .map(|update| update.generation)
        {
            if update.generation < current_generation {
                return;
            }
            if update.generation > current_generation {
                self.retire_live_text_checkpoint(&key);
            }
        }
        if matches!(update.operation, TextStreamOperation::Terminal { .. }) {
            self.retire_live_text_checkpoint(&key);
            self.insert_live_text_tombstone(key, update.generation, update.revision);
            return;
        }
        let (mut text, mut start_offset, mut total_bytes, mut truncated) = self
            .state
            .live_text_checkpoints
            .get(&key)
            .and_then(live_text_event_update)
            .and_then(|update| match &update.operation {
                TextStreamOperation::Checkpoint {
                    start_offset,
                    text,
                    total_bytes,
                    truncated,
                } => Some((text.clone(), *start_offset, *total_bytes, *truncated)),
                _ => None,
            })
            .unwrap_or_default();
        match &update.operation {
            TextStreamOperation::Append {
                expected_offset,
                text: appended,
            } if *expected_offset == total_bytes => {
                text.push_str(appended);
                total_bytes = total_bytes.saturating_add(appended.len());
            }
            TextStreamOperation::Checkpoint {
                start_offset: checkpoint_start,
                text: checkpoint_text,
                total_bytes: checkpoint_total,
                truncated: checkpoint_truncated,
            } => {
                text.clone_from(checkpoint_text);
                start_offset = *checkpoint_start;
                total_bytes = *checkpoint_total;
                truncated = *checkpoint_truncated;
            }
            _ => return,
        }
        let (retained, omitted) = bounded_text_suffix(&text, MAX_ACTIVE_TEXT_STREAM_BYTES_PER_KEY);
        start_offset = start_offset.saturating_add(omitted);
        truncated |= omitted != 0;
        let checkpoint_update = TextStreamUpdate {
            generation: update.generation,
            first_revision: update.revision,
            revision: update.revision,
            operation: TextStreamOperation::Checkpoint {
                start_offset,
                text: retained.to_owned(),
                total_bytes,
                truncated,
            },
        };
        let checkpoint_kind = match kind {
            SessionLiveEventKind::AssistantTextStreamUpdated {
                output_position,
                turn_id,
                segment_id,
                segment_order,
                ..
            } => SessionLiveEventKind::AssistantTextStreamUpdated {
                output_position: *output_position,
                turn_id: turn_id.clone(),
                segment_id: segment_id.clone(),
                segment_order: *segment_order,
                update: checkpoint_update,
            },
            SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                output_position,
                turn_id,
                activity_id,
                activity_order,
                part_id,
                kind,
                role,
                part_order,
                ..
            } => SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                output_position: *output_position,
                turn_id: turn_id.clone(),
                activity_id: activity_id.clone(),
                activity_order: *activity_order,
                part_id: part_id.clone(),
                kind: *kind,
                role: *role,
                part_order: *part_order,
                update: checkpoint_update,
            },
            _ => unreachable!("stream kind was matched above"),
        };
        let checkpoint = SessionLiveEvent {
            session_id: self.state.summary.id,
            kind: checkpoint_kind,
        };
        if !self.state.live_text_checkpoints.contains_key(&key) {
            self.state.live_text_checkpoint_order.push(key.clone());
        }
        self.state.live_text_checkpoints.insert(key, checkpoint);
        self.enforce_live_text_checkpoint_bounds();
    }

    fn enforce_live_text_checkpoint_bounds(&mut self) {
        while self.state.live_text_checkpoints.len() > MAX_ACTIVE_TEXT_STREAM_KEYS
            || live_text_checkpoint_bytes(&self.state.live_text_checkpoints)
                > MAX_ACTIVE_TEXT_STREAM_BYTES_PER_SESSION
        {
            let Some(oldest) = self.state.live_text_checkpoint_order.first().cloned() else {
                break;
            };
            self.state.live_text_checkpoint_order.remove(0);
            self.state.live_text_checkpoints.remove(&oldest);
        }
    }
}

const fn live_text_event_update(
    event: &SessionLiveEvent,
) -> Option<&bcode_session_models::TextStreamUpdate> {
    match &event.kind {
        SessionLiveEventKind::AssistantTextStreamUpdated { update, .. }
        | SessionLiveEventKind::AssistantReasoningTextStreamUpdated { update, .. } => Some(update),
        _ => None,
    }
}

fn live_text_checkpoint_bytes(
    checkpoints: &std::collections::BTreeMap<LiveTextStreamKey, SessionLiveEvent>,
) -> usize {
    checkpoints
        .values()
        .filter_map(live_text_event_update)
        .filter_map(|update| match &update.operation {
            bcode_session_models::TextStreamOperation::Checkpoint { text, .. } => Some(text.len()),
            _ => None,
        })
        .sum()
}

fn history_page_from_events(
    session_id: bcode_session_models::SessionId,
    events: &[SessionEvent],
    query: SessionHistoryQuery,
) -> Result<SessionHistoryPage, SessionError> {
    let limit = query.limit.max(1);
    if limit > bcode_session_models::MAX_SESSION_HISTORY_READ_EVENTS {
        return Err(crate::db::SessionDbError::HistoryReadLimitExceeded {
            requested: limit,
            maximum: bcode_session_models::MAX_SESSION_HISTORY_READ_EVENTS,
        }
        .into());
    }
    let mut matching = events
        .iter()
        .filter(|event| match query.direction {
            bcode_session_models::SessionHistoryDirection::Forward => query
                .cursor
                .is_none_or(|cursor| event.sequence >= cursor.sequence),
            bcode_session_models::SessionHistoryDirection::Backward => query
                .cursor
                .is_none_or(|cursor| event.sequence <= cursor.sequence),
        })
        .collect::<Vec<_>>();
    if matches!(
        query.direction,
        bcode_session_models::SessionHistoryDirection::Backward
    ) {
        matching.reverse();
    }
    let has_more = matching.len() > limit;
    let next_cursor = matching
        .get(limit)
        .map(|event| bcode_session_models::SessionHistoryCursor {
            sequence: event.sequence,
        });
    let mut page_events = matching
        .into_iter()
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    if matches!(
        query.direction,
        bcode_session_models::SessionHistoryDirection::Backward
    ) {
        page_events.reverse();
    }
    Ok(SessionHistoryPage {
        session_id,
        events: page_events,
        compatibility_issues: Vec::new(),
        next_cursor,
        has_more,
    })
}

fn history_around_from_events(
    session_id: bcode_session_models::SessionId,
    events: &[SessionEvent],
    query: SessionHistoryAroundQuery,
) -> Result<SessionHistoryWindow, SessionError> {
    let limit = query.event_limit();
    if limit > bcode_session_models::MAX_SESSION_HISTORY_READ_EVENTS {
        return Err(crate::db::SessionDbError::HistoryReadLimitExceeded {
            requested: limit,
            maximum: bcode_session_models::MAX_SESSION_HISTORY_READ_EVENTS,
        }
        .into());
    }
    let start = query
        .sequence
        .saturating_sub(u64::try_from(query.before).unwrap_or(u64::MAX));
    let end = query
        .sequence
        .saturating_add(u64::try_from(query.after).unwrap_or(u64::MAX));
    let window_events = select_event_range_from_events(events, start, end, limit);
    Ok(SessionHistoryWindow {
        session_id,
        requested_sequence: query.sequence,
        anchor_present: window_events
            .iter()
            .any(|event| event.sequence == query.sequence),
        events: window_events,
        first_available_sequence: events.first().map(|event| event.sequence),
        last_available_sequence: events.last().map(|event| event.sequence),
        compatibility_issues: Vec::new(),
    })
}

fn select_event_range_from_events(
    events: &[SessionEvent],
    start_sequence: u64,
    end_sequence: u64,
    max_events: usize,
) -> Vec<SessionEvent> {
    if start_sequence > end_sequence || max_events == 0 {
        return Vec::new();
    }
    events
        .iter()
        .filter(|event| event.sequence >= start_sequence && event.sequence <= end_sequence)
        .take(max_events)
        .cloned()
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub summary: SessionSummary,
    pub working_directory: PathBuf,
    pub load_status: SessionLoadStatusKind,
    pub owned: bool,
}

fn session_usage_summary_from_observations(
    observations: impl Iterator<Item = (u64, bcode_session_models::SessionTokenUsage)>,
) -> bcode_session_models::SessionUsageSummary {
    let mut summary = bcode_session_models::SessionUsageSummary::default();
    let mut latest_sequence = None;
    for (sequence, usage) in observations {
        summary.observed_usage_count = summary.observed_usage_count.saturating_add(1);
        if let Some(tokens) = usage.metered_total_tokens() {
            summary.cumulative_metered_tokens = summary
                .cumulative_metered_tokens
                .saturating_add(u64::from(tokens));
        }
        match &usage.cost {
            Some(bcode_session_models::SessionCostEstimate::Estimated {
                currency,
                total_micros,
                ..
            }) => {
                let total = summary.totals_micros.entry(currency.clone()).or_default();
                *total = total.saturating_add(*total_micros);
                summary.estimated_usage_count = summary.estimated_usage_count.saturating_add(1);
            }
            Some(bcode_session_models::SessionCostEstimate::Unavailable { .. }) => {
                summary.unavailable_usage_count = summary.unavailable_usage_count.saturating_add(1);
            }
            None => {}
        }
        if latest_sequence.is_none_or(|latest| sequence > latest) {
            latest_sequence = Some(sequence);
            summary.latest_usage = Some(usage);
        }
    }
    summary
}

impl SessionSnapshot {
    fn from_state(state: &SessionState, owned: bool) -> Self {
        Self {
            summary: state.summary(),
            working_directory: state.working_directory.clone(),
            load_status: state.load_status,
            owned,
        }
    }
}
