//! Session actor, handle, and snapshot plumbing.

use super::{
    Arc, CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, Instant, PathBuf, ProjectionWindow,
    ProjectionWindowRequest, SessionAttachment, SessionError, SessionEvent, SessionEventKind,
    SessionEventProvenance, SessionInputHistoryEntry, SessionLiveEvent, SessionLiveEventKind,
    SessionLoadStatusKind, SessionState, SessionStoreExecutor, SessionSummary, current_unix_millis,
    elapsed_ms, input_history_from_events, model_context_events_from_history,
    title_from_first_prompt, usize_to_u64,
};
use crate::db::{MaterializedProjection, SessionDb, SessionDbError};
use bcode_metrics::MetricsContext;
use bcode_session_models::ProjectionWindowAnchor;
use std::sync::RwLock;
use tokio::sync::{broadcast, mpsc, oneshot};

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
        | SessionDbError::MigrationHistoryIncompatible { .. } => {
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

impl SessionHandle {
    #[must_use]
    pub fn new(state: SessionState, store: Option<SessionStoreExecutor>) -> Self {
        let snapshot = Arc::new(RwLock::new(SessionSnapshot::from_state(&state)));
        let (commands, receiver) = mpsc::channel(256);
        let actor = SessionActor {
            state,
            store,
            db: None,
            commands: receiver,
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

    pub async fn publish_transient_event(
        &self,
        kind: SessionEventKind,
    ) -> Result<Option<SessionEvent>, SessionError> {
        self.send(|reply| SessionCommand::PublishTransient { kind, reply })
            .await
    }

    pub async fn replace_state(&self, state: SessionState) -> Result<(), SessionError> {
        self.send(|reply| SessionCommand::ReplaceState {
            state: Box::new(state),
            reply,
        })
        .await
    }

    pub async fn release_idle_resources(&self) -> Result<bool, SessionError> {
        self.send(SessionCommand::ReleaseIdleResources).await
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
    History(oneshot::Sender<Result<Vec<SessionEvent>, SessionError>>),
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
    CurrentAgentSelection(oneshot::Sender<Option<String>>),
    SetCurrentAgent {
        agent_id: String,
        reply: oneshot::Sender<Result<(), SessionError>>,
    },
    PublishLive {
        event: SessionLiveEventKind,
        reply: oneshot::Sender<Option<SessionLiveEvent>>,
    },
    PublishTransient {
        kind: SessionEventKind,
        reply: oneshot::Sender<Option<SessionEvent>>,
    },
    ReplaceState {
        state: Box<SessionState>,
        reply: oneshot::Sender<()>,
    },
    ReleaseIdleResources(oneshot::Sender<bool>),
    Shutdown(oneshot::Sender<()>),
}

struct SessionActor {
    state: SessionState,
    store: Option<SessionStoreExecutor>,
    db: Option<SessionDb>,
    commands: mpsc::Receiver<SessionCommand>,
    snapshot: Arc<RwLock<SessionSnapshot>>,
}

impl SessionActor {
    async fn run(mut self) {
        let context = MetricsContext::new().with_session_id(&self.state.summary.id);
        while let Some(command) = self.commands.recv().await {
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
                let _ = reply.send(self.attach(client_id, mode, queued_at).await);
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
            | SessionCommand::AppendUserMessage { .. }
            | SessionCommand::Attach { .. }
            | SessionCommand::SetComposerDraft { .. } => {
                unreachable!("write commands are handled before read commands")
            }
            SessionCommand::SubscribeEvents(reply) => {
                let _ = reply.send(Ok(self.subscribe_events()));
            }
            SessionCommand::Detach { client_id, reply } => {
                let _ = reply.send(Ok(self.detach(client_id)));
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
            SessionCommand::History(reply) => {
                let _ = reply.send(self.history().await);
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
            SessionCommand::PublishTransient { kind, reply } => {
                let _ = reply.send(self.publish_transient_event(kind));
            }
            SessionCommand::ReplaceState { state, reply } => {
                self.state = *state;
                self.refresh_snapshot();
                let _ = reply.send(());
            }
            SessionCommand::ReleaseIdleResources(reply) => {
                let _ = reply.send(self.release_idle_resources());
            }
            SessionCommand::Shutdown(reply) => {
                let _ = reply.send(());
                return true;
            }
        }
        false
    }

    fn refresh_snapshot(&self) {
        *self
            .snapshot
            .write()
            .expect("session snapshot lock poisoned") = SessionSnapshot::from_state(&self.state);
    }

    fn release_idle_resources(&mut self) -> bool {
        if !self.state.clients.is_empty() {
            return false;
        }
        self.db = None;
        self.state.events = None;
        self.state.load_status = SessionLoadStatusKind::SummaryOnly;
        self.refresh_snapshot();
        true
    }

    async fn set_composer_draft(
        &mut self,
        text: &str,
        updated_at_ms: u64,
    ) -> Result<(), SessionError> {
        let Some(db) = self.existing_session_db().await? else {
            return Ok(());
        };
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::NotFound(self.state.summary.id))?;
        let _write_guard =
            crate::lease::acquire_session_write_lock(&store.root_path(), self.state.summary.id)?;
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
        let db = self.session_db_for_write().await?;
        db.validate_write_readiness().await?;
        Ok(())
    }

    async fn session_db_for_write(&mut self) -> Result<SessionDb, SessionError> {
        if let Some(db) = &self.db {
            return Ok(db.clone());
        }
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
        self.db = Some(db.clone());
        Ok(db)
    }

    async fn existing_session_db(&mut self) -> Result<Option<SessionDb>, SessionError> {
        if self.db.is_some() {
            return Ok(self.db.clone());
        }
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
        self.db = Some(db.clone());
        Ok(Some(db))
    }

    async fn refresh_state_from_db_for_write(
        &mut self,
        db: &SessionDb,
    ) -> Result<(), SessionError> {
        let Some(db_state) = db.session_state().await? else {
            return Ok(());
        };
        let expected_last_sequence = db
            .last_event_sequence()
            .await?
            .unwrap_or(db_state.last_event_seq);
        if db_state.last_event_seq < expected_last_sequence {
            return Err(SessionError::ProjectionStale {
                session_id: self.state.summary.id,
                projection: "session_state",
                checkpoint: Some(db_state.last_event_seq),
                expected: expected_last_sequence,
            });
        }
        if expected_last_sequence.saturating_add(1) == self.state.next_sequence {
            return Ok(());
        }
        let activity_bounds = db.activity_bounds().await?;
        let created_at_ms = activity_bounds
            .map(|(created_at_ms, _)| created_at_ms)
            .or(db_state.updated_at_ms)
            .unwrap_or(self.state.summary.created_at_ms);
        let updated_at_ms = db_state
            .updated_at_ms
            .or_else(|| activity_bounds.map(|(_, updated_at_ms)| updated_at_ms))
            .unwrap_or(self.state.summary.updated_at_ms);
        let clients = self.state.clients.clone();
        let sender = self.state.sender.clone();
        let live_events = self.state.live_events.clone();
        self.state = SessionState::from_db_state(db_state, created_at_ms, updated_at_ms);
        self.state.clients = clients;
        self.state.summary.client_count = self.state.clients.len();
        self.state.sender = sender;
        self.state.live_events = live_events;
        Ok(())
    }

    async fn append_event(
        &mut self,
        kind: SessionEventKind,
        provenance: Option<SessionEventProvenance>,
        activity_timestamp_ms: u64,
    ) -> Result<SessionEvent, SessionError> {
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
            let _write_guard = crate::lease::acquire_session_write_lock(
                &store.root_path(),
                self.state.summary.id,
            )?;
            let db = self.session_db_for_write().await?;
            self.refresh_state_from_db_for_write(&db).await?;
            let mut event = self.state.build_next_event(kind, event_timestamp_ms);
            event.provenance = provenance;
            let db_append_started_at = Instant::now();
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
        self.update_manifest_and_catalog_after_append().await;
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

    async fn update_manifest_and_catalog_after_append(&self) {
        let Some(store) = &self.store else {
            return;
        };
        if let Err(error) = store.write_session_manifest(self.state.summary()).await {
            store
                .metrics()
                .increment_counter("session.manifest.write_error_total");
            eprintln!("failed to write session manifest: {error}");
        }
        let catalog = match store
            .lease_owner()
            .build_fingerprint
            .as_deref()
            .map(crate::safe_catalog_namespace)
        {
            Some(namespace) => {
                crate::db::GlobalSessionDb::open_turso_in_root_namespace_observed(
                    &store.root_path(),
                    &namespace,
                    store.metrics(),
                )
                .await
            }
            None => {
                crate::db::GlobalSessionDb::open_turso_in_root_observed(
                    &store.root_path(),
                    store.metrics(),
                )
                .await
            }
        };
        match catalog {
            Ok(catalog) => {
                if let Err(error) = catalog
                    .upsert_session(
                        &self.state.summary(),
                        &crate::db::session_db_path(&store.root_path(), self.state.summary.id),
                    )
                    .await
                {
                    store
                        .metrics()
                        .increment_counter("session.catalog.upsert_error_total");
                    eprintln!("failed to update session catalog: {error}");
                }
            }
            Err(error) => {
                store
                    .metrics()
                    .increment_counter("session.catalog.open_error_total");
                eprintln!("failed to open session catalog for update: {error}");
            }
        }
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

    fn detach(&mut self, client_id: ClientId) -> bool {
        if self.state.clients.remove(&client_id) {
            self.state.summary.client_count = self.state.clients.len();
            self.refresh_snapshot();
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

    async fn projection_window(
        &mut self,
        request: ProjectionWindowRequest,
    ) -> Result<ProjectionWindow, SessionError> {
        if let Some(db) = self.existing_session_db().await? {
            let expected_last_sequence = self.state.next_sequence.saturating_sub(1);
            let checkpoint = db
                .materialized_projection_checkpoint(MaterializedProjection::Transcript)
                .await?;
            if checkpoint.is_none_or(|checkpoint| checkpoint < expected_last_sequence) {
                return Err(SessionError::ProjectionStale {
                    session_id: self.state.summary.id,
                    projection: "transcript",
                    checkpoint,
                    expected: expected_last_sequence,
                });
            }
            if !matches!(request.anchor, ProjectionWindowAnchor::Latest) {
                return self.projection_window_from_bounded_events(&request).await;
            }
            let transcript_items = db
                .transcript_items_for_latest_window(
                    request.target.min_items.unwrap_or(1),
                    request.limits.max_items,
                    request.limits.max_bytes,
                )
                .await?;
            return crate::projection::projection_window_from_db_transcript_items(
                &transcript_items,
                db.first_event_sequence().await?,
                db.last_event_sequence().await?,
                &request,
            )
            .ok_or(SessionError::UnsupportedProjectionWindow);
        }

        if self.store.is_some() {
            return Err(SessionError::NotFound(self.state.summary.id));
        }
        Err(SessionError::UnsupportedProjectionWindow)
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
        let expected_last_sequence = self.state.next_sequence.saturating_sub(1);
        match db
            .materialized_projection_checkpoint(MaterializedProjection::Transcript)
            .await?
        {
            Some(checkpoint) if checkpoint >= expected_last_sequence => {}
            checkpoint => {
                return Err(SessionError::ProjectionStale {
                    session_id: self.state.summary.id,
                    projection: "transcript",
                    checkpoint,
                    expected: expected_last_sequence,
                });
            }
        }

        let transcript_items = db.latest_transcript_items(limit).await?;
        if transcript_items.is_empty() {
            return Ok(Some(Vec::new()));
        }

        let start_sequence = transcript_items
            .iter()
            .map(|item| item.event_seq_start)
            .min()
            .unwrap_or(0);
        let end_sequence = transcript_items
            .iter()
            .map(|item| item.event_seq_end)
            .max()
            .unwrap_or(start_sequence);
        let max_events =
            usize::try_from(end_sequence.saturating_sub(start_sequence) + 1).unwrap_or(usize::MAX);

        Ok(Some(
            db.events_range(start_sequence, end_sequence, max_events)
                .await?,
        ))
    }

    async fn input_history(&mut self) -> Result<Vec<SessionInputHistoryEntry>, SessionError> {
        if let Some(db) = self.existing_session_db().await? {
            let expected_last_sequence = self.state.next_sequence.saturating_sub(1);
            let checkpoint = db
                .materialized_projection_checkpoint(MaterializedProjection::InputHistory)
                .await?;
            if checkpoint.is_some_and(|checkpoint| checkpoint >= expected_last_sequence) {
                return Ok(db.input_history().await?);
            }
            return Err(SessionError::ProjectionStale {
                session_id: self.state.summary.id,
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

    async fn active_tool_runs(&mut self) -> Result<Vec<crate::db::ToolRun>, SessionError> {
        let Some(db) = self.existing_session_db().await? else {
            return Ok(Vec::new());
        };
        let expected_last_sequence = self.state.next_sequence.saturating_sub(1);
        let checkpoint = db
            .materialized_projection_checkpoint(MaterializedProjection::ToolRuns)
            .await?;
        if checkpoint.is_some_and(|checkpoint| checkpoint >= expected_last_sequence) {
            return Ok(db.active_tool_runs().await?);
        }
        Err(SessionError::ProjectionStale {
            session_id: self.state.summary.id,
            projection: "tool_runs",
            checkpoint,
            expected: expected_last_sequence,
        })
    }

    async fn active_runtime_work(
        &mut self,
    ) -> Result<Vec<crate::db::RuntimeWorkProjection>, SessionError> {
        let Some(db) = self.existing_session_db().await? else {
            return Ok(Vec::new());
        };
        let expected_last_sequence = self.state.next_sequence.saturating_sub(1);
        let checkpoint = db
            .materialized_projection_checkpoint(MaterializedProjection::RuntimeWork)
            .await?;
        if checkpoint.is_some_and(|checkpoint| checkpoint >= expected_last_sequence) {
            return Ok(db.active_runtime_work().await?);
        }
        Err(SessionError::ProjectionStale {
            session_id: self.state.summary.id,
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

    fn publish_live_event(&self, kind: SessionLiveEventKind) -> Option<SessionLiveEvent> {
        let event = SessionLiveEvent {
            session_id: self.state.summary.id,
            kind,
        };
        self.state.live_events.publish(event)
    }

    fn publish_transient_event(&self, kind: SessionEventKind) -> Option<SessionEvent> {
        if self.state.sender.receiver_count() == 0 {
            return None;
        }
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: self.state.next_sequence,
            timestamp_ms: current_unix_millis(),
            session_id: self.state.summary.id,
            provenance: None,
            kind,
        };
        let _ = self.state.sender.send(event.clone());
        Some(event)
    }
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
}

impl SessionSnapshot {
    fn from_state(state: &SessionState) -> Self {
        Self {
            summary: state.summary(),
            working_directory: state.working_directory.clone(),
            load_status: state.load_status,
        }
    }
}
