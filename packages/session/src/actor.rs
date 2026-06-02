//! Session actor, handle, and snapshot plumbing.

use super::*;
use crate::db::SessionDb;
use std::sync::RwLock;
use tokio::sync::{mpsc, oneshot};

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
        activity_timestamp_ms: u64,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        self.send(|reply| SessionCommand::AppendUserMessage {
            client_id,
            text,
            activity_timestamp_ms,
            reply,
        })
        .await?
    }

    pub async fn attach(
        &self,
        client_id: ClientId,
        mode: AttachMode,
        activity_timestamp_ms: u64,
    ) -> Result<SessionAttachment, SessionError> {
        let (reply, receiver) = oneshot::channel();
        let queued_at = Instant::now();
        self.commands
            .send(SessionCommand::Attach {
                client_id,
                mode,
                activity_timestamp_ms,
                queued_at,
                reply,
            })
            .await
            .map_err(|_| SessionError::NotFound(self.snapshot().summary.id))?;
        receiver
            .await
            .map_err(|_| SessionError::NotFound(self.snapshot().summary.id))?
    }

    pub async fn read_only_attach(
        &self,
        client_id: ClientId,
        mode: AttachMode,
    ) -> Result<SessionAttachment, SessionError> {
        self.send(|reply| SessionCommand::ReadOnlyAttach {
            client_id,
            mode,
            reply,
        })
        .await?
    }

    pub async fn detach(
        &self,
        client_id: ClientId,
        activity_timestamp_ms: u64,
    ) -> Result<Option<SessionEvent>, SessionError> {
        self.send(|reply| SessionCommand::Detach {
            client_id,
            activity_timestamp_ms,
            reply,
        })
        .await?
    }

    pub async fn summary(&self) -> Result<SessionSummary, SessionError> {
        self.send(SessionCommand::Summary).await
    }

    pub async fn working_directory(&self) -> Result<PathBuf, SessionError> {
        self.send(SessionCommand::WorkingDirectory).await
    }

    pub async fn requires_migration_for_write(&self) -> Result<bool, SessionError> {
        self.send(|reply| SessionCommand::RequiresMigrationForWrite { reply })
            .await?
    }

    pub async fn access_status(&self) -> Result<SessionAccessStatus, SessionError> {
        self.send(SessionCommand::AccessStatus).await
    }

    pub async fn history(&self) -> Result<Vec<SessionEvent>, SessionError> {
        self.send(SessionCommand::History).await?
    }

    pub async fn projection_window_from_index(
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

    pub async fn model_context_events(&self) -> Result<Vec<SessionEvent>, SessionError> {
        self.send(SessionCommand::ModelContextEvents).await?
    }

    pub async fn active_tool_runs(&self) -> Result<Vec<crate::db::ToolRun>, SessionError> {
        self.send(SessionCommand::ActiveToolRuns).await?
    }

    pub async fn current_model_selection(&self) -> Result<Option<(String, String)>, SessionError> {
        self.send(SessionCommand::CurrentModelSelection).await
    }

    pub async fn current_agent_selection(&self) -> Result<Option<String>, SessionError> {
        self.send(SessionCommand::CurrentAgentSelection).await
    }

    pub async fn set_current_agent(&self, agent_id: String) -> Result<(), SessionError> {
        self.send(|reply| SessionCommand::SetCurrentAgent { agent_id, reply })
            .await?
    }

    pub async fn publish_transient_event(
        &self,
        kind: SessionEventKind,
    ) -> Result<Option<SessionEvent>, SessionError> {
        self.send(|reply| SessionCommand::PublishTransient { kind, reply })
            .await
    }

    pub async fn client_ids(&self) -> Result<BTreeSet<ClientId>, SessionError> {
        self.send(SessionCommand::ClientIds).await
    }

    pub async fn replace_state(&self, state: SessionState) -> Result<(), SessionError> {
        self.send(|reply| SessionCommand::ReplaceState {
            state: Box::new(state),
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
        activity_timestamp_ms: u64,
        reply: oneshot::Sender<Result<Vec<SessionEvent>, SessionError>>,
    },
    Attach {
        client_id: ClientId,
        mode: AttachMode,
        activity_timestamp_ms: u64,
        queued_at: Instant,
        reply: oneshot::Sender<Result<SessionAttachment, SessionError>>,
    },
    ReadOnlyAttach {
        client_id: ClientId,
        mode: AttachMode,
        reply: oneshot::Sender<Result<SessionAttachment, SessionError>>,
    },
    Detach {
        client_id: ClientId,
        activity_timestamp_ms: u64,
        reply: oneshot::Sender<Result<Option<SessionEvent>, SessionError>>,
    },
    Summary(oneshot::Sender<SessionSummary>),
    WorkingDirectory(oneshot::Sender<PathBuf>),
    RequiresMigrationForWrite {
        reply: oneshot::Sender<Result<bool, SessionError>>,
    },
    AccessStatus(oneshot::Sender<SessionAccessStatus>),
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
    ModelContextEvents(oneshot::Sender<Result<Vec<SessionEvent>, SessionError>>),
    ActiveToolRuns(oneshot::Sender<Result<Vec<crate::db::ToolRun>, SessionError>>),
    CurrentModelSelection(oneshot::Sender<Option<(String, String)>>),
    CurrentAgentSelection(oneshot::Sender<Option<String>>),
    SetCurrentAgent {
        agent_id: String,
        reply: oneshot::Sender<Result<(), SessionError>>,
    },
    PublishTransient {
        kind: SessionEventKind,
        reply: oneshot::Sender<Option<SessionEvent>>,
    },
    ClientIds(oneshot::Sender<BTreeSet<ClientId>>),
    ReplaceState {
        state: Box<SessionState>,
        reply: oneshot::Sender<()>,
    },
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
        while let Some(command) = self.commands.recv().await {
            if self.handle_command(command).await {
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
                activity_timestamp_ms,
                reply,
            } => {
                let _ = reply.send(
                    self.append_user_message(client_id, text, activity_timestamp_ms)
                        .await,
                );
            }
            SessionCommand::Attach {
                client_id,
                mode,
                activity_timestamp_ms,
                queued_at,
                reply,
            } => {
                let _ = reply.send(
                    self.attach(client_id, mode, activity_timestamp_ms, queued_at)
                        .await,
                );
            }
            command => return self.handle_read_command(command).await,
        }
        false
    }

    async fn handle_read_command(&mut self, command: SessionCommand) -> bool {
        match command {
            SessionCommand::AppendEvent { .. }
            | SessionCommand::AppendUserMessage { .. }
            | SessionCommand::Attach { .. } => {
                unreachable!("write commands are handled before read commands")
            }
            SessionCommand::ReadOnlyAttach {
                client_id,
                mode,
                reply,
            } => {
                let _ = reply.send(self.read_only_attach(client_id, mode).await);
            }
            SessionCommand::Detach {
                client_id,
                activity_timestamp_ms,
                reply,
            } => {
                let _ = reply.send(self.detach(client_id, activity_timestamp_ms).await);
            }
            SessionCommand::Summary(reply) => {
                let _ = reply.send(self.state.summary());
            }
            SessionCommand::WorkingDirectory(reply) => {
                let _ = reply.send(self.state.working_directory.clone());
            }
            SessionCommand::RequiresMigrationForWrite { reply } => {
                let _ = reply.send(
                    self.state
                        .requires_migration_for_write(self.state.summary.id),
                );
            }
            SessionCommand::AccessStatus(reply) => {
                let _ = reply.send(self.state.access_status);
            }
            SessionCommand::History(reply) => {
                let _ = reply.send(self.history().await);
            }
            SessionCommand::ProjectionWindowFromIndex { request, reply } => {
                let _ = reply.send(self.projection_window_from_index(request).await);
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
            SessionCommand::ModelContextEvents(reply) => {
                let _ = reply.send(self.model_context_events().await);
            }
            SessionCommand::ActiveToolRuns(reply) => {
                let _ = reply.send(self.active_tool_runs().await);
            }
            SessionCommand::CurrentModelSelection(reply) => {
                let _ = reply.send(
                    self.state
                        .current_provider
                        .clone()
                        .zip(self.state.current_model.clone()),
                );
            }
            SessionCommand::CurrentAgentSelection(reply) => {
                let _ = reply.send(self.state.current_agent.clone());
            }
            SessionCommand::SetCurrentAgent { agent_id, reply } => {
                let _ = reply.send(self.set_current_agent(agent_id));
            }
            SessionCommand::PublishTransient { kind, reply } => {
                let _ = reply.send(self.publish_transient_event(kind));
            }
            SessionCommand::ClientIds(reply) => {
                let _ = reply.send(self.state.clients.clone());
            }
            SessionCommand::ReplaceState { state, reply } => {
                self.state = *state;
                self.refresh_snapshot();
                let _ = reply.send(());
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

    async fn session_db_for_write(&mut self) -> Result<SessionDb, SessionError> {
        if let Some(db) = &self.db {
            return Ok(db.clone());
        }
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::NotFound(self.state.summary.id))?;
        let db = SessionDb::open_turso_in_root(self.state.summary.id, &store.root_path()).await?;
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
        let db = SessionDb::open_turso_in_root(self.state.summary.id, &store.root_path()).await?;
        self.db = Some(db.clone());
        Ok(Some(db))
    }

    async fn append_event(
        &mut self,
        kind: SessionEventKind,
        provenance: Option<SessionEventProvenance>,
        activity_timestamp_ms: u64,
    ) -> Result<SessionEvent, SessionError> {
        let mut event = self.state.build_next_event(kind)?;
        event.provenance = provenance;
        if self.store.is_some() {
            let db = self.session_db_for_write().await?;
            let db_append_started_at = Instant::now();
            db.append_event_with_activity_timestamp(&event, Some(activity_timestamp_ms))
                .await?;
            if let Some(store) = &self.store {
                store.metrics().record_histogram(
                    "session.actor.append_event.db_append_duration_ms",
                    elapsed_ms(db_append_started_at),
                );
            }
        }
        self.state
            .apply_persisted_event(event.clone(), activity_timestamp_ms);
        if let Some(store) = &self.store {
            let catalog =
                crate::db::GlobalSessionDb::open_turso_in_root(&store.root_path()).await?;
            catalog
                .upsert_session(
                    &self.state.summary(),
                    &crate::db::session_db_path(&store.root_path(), self.state.summary.id),
                )
                .await?;
        }
        self.state.index_status = SessionIndexStatusKind::Current;
        self.refresh_snapshot();
        Ok(event)
    }

    async fn append_user_message(
        &mut self,
        client_id: ClientId,
        text: String,
        activity_timestamp_ms: u64,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        self.state.ensure_writable()?;
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
        events.push(
            self.append_event(
                SessionEventKind::UserMessage { client_id, text },
                None,
                activity_timestamp_ms,
            )
            .await?,
        );
        Ok(events)
    }

    #[allow(clippy::too_many_lines)]
    async fn attach(
        &mut self,
        client_id: ClientId,
        mode: AttachMode,
        activity_timestamp_ms: u64,
        queued_at: Instant,
    ) -> Result<SessionAttachment, SessionError> {
        let total_started_at = Instant::now();
        let metrics = self.store.as_ref().map(SessionStoreExecutor::metrics);
        if let Some(metrics) = &metrics {
            metrics.increment_counter("session.actor.attach.total");
            metrics.record_histogram(
                "session.actor.attach.queue_wait_duration_ms",
                elapsed_ms(queued_at),
            );
        }
        let writable_started_at = Instant::now();
        self.state.ensure_writable()?;
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
                } else {
                    let store = self
                        .store
                        .as_ref()
                        .ok_or(SessionError::NotFound(self.state.summary.id))?;
                    store
                        .read_session_history_page(
                            self.state.summary.id,
                            SessionHistoryQuery {
                                cursor: None,
                                limit,
                                direction: SessionHistoryDirection::Backward,
                            },
                        )
                        .await?
                        .events
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
        let append_started_at = Instant::now();
        let attached_event = self
            .append_event(
                SessionEventKind::ClientAttached { client_id },
                None,
                activity_timestamp_ms,
            )
            .await?;
        let session = self.state.summary();
        if let Some(metrics) = &metrics {
            metrics.record_histogram(
                "session.actor.attach.append_client_attached_duration_ms",
                elapsed_ms(append_started_at),
            );
            metrics.record_histogram(
                "session.actor.attach.total_duration_ms",
                elapsed_ms(total_started_at),
            );
        }
        Ok(SessionAttachment {
            session,
            history,
            input_history,
            attached_event,
            events,
        })
    }

    async fn read_only_attach(
        &mut self,
        client_id: ClientId,
        mode: AttachMode,
    ) -> Result<SessionAttachment, SessionError> {
        let history = match mode {
            AttachMode::Full => self.history().await?,
            AttachMode::Recent { limit } => {
                let store = self
                    .store
                    .as_ref()
                    .ok_or(SessionError::NotFound(self.state.summary.id))?;
                store
                    .read_session_history_page(
                        self.state.summary.id,
                        SessionHistoryQuery {
                            cursor: None,
                            limit,
                            direction: SessionHistoryDirection::Backward,
                        },
                    )
                    .await?
                    .events
            }
            AttachMode::ProjectionWindow { history } => history,
        };
        let input_history = self.input_history().await?;
        self.state.clients.insert(client_id);
        self.state.summary.client_count = self.state.clients.len();
        self.refresh_snapshot();
        let events = self.state.sender.subscribe();
        let attached_event = SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: self.state.next_sequence,
            session_id: self.state.summary.id,
            provenance: None,
            kind: SessionEventKind::ClientAttached { client_id },
        };
        Ok(SessionAttachment {
            session: self.state.summary(),
            history,
            input_history,
            attached_event,
            events,
        })
    }

    async fn detach(
        &mut self,
        client_id: ClientId,
        activity_timestamp_ms: u64,
    ) -> Result<Option<SessionEvent>, SessionError> {
        if !self.state.access_status.writable() {
            if self.state.clients.remove(&client_id) {
                self.state.summary.client_count = self.state.clients.len();
                self.refresh_snapshot();
            }
            return Ok(None);
        }
        if self.state.clients.remove(&client_id) {
            self.state.summary.client_count = self.state.clients.len();
            return Ok(Some(
                self.append_event(
                    SessionEventKind::ClientDetached { client_id },
                    None,
                    activity_timestamp_ms,
                )
                .await?,
            ));
        }
        Ok(None)
    }

    async fn history(&mut self) -> Result<Vec<SessionEvent>, SessionError> {
        if let Some(db) = self.existing_session_db().await? {
            return Ok(db.all_events().await?);
        }
        if let Some(events) = &self.state.events {
            return Ok(events.clone());
        }
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::NotFound(self.state.summary.id))?;
        Ok(store.read_session_events(self.state.summary.id).await?)
    }

    async fn projection_window_from_index(
        &mut self,
        request: ProjectionWindowRequest,
    ) -> Result<ProjectionWindow, SessionError> {
        if let Some(db) = self.existing_session_db().await? {
            let expected_last_sequence = self.state.next_sequence.saturating_sub(1);
            let checkpoint = db.projection_checkpoint("transcript").await?;
            if checkpoint.is_none_or(|checkpoint| checkpoint < expected_last_sequence) {
                return Err(SessionError::ProjectionStale {
                    session_id: self.state.summary.id,
                    projection: "transcript",
                    checkpoint,
                    expected: expected_last_sequence,
                });
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

        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::UnsupportedProjectionWindow)?;
        let index = store.ensure_fresh_index(self.state.summary.id).await?;
        let transcript_index = store
            .ensure_transcript_index(self.state.summary.id)
            .await
            .map_err(|_error| SessionError::UnsupportedProjectionWindow)?;
        if transcript_index.spans.is_empty() {
            return Err(SessionError::UnsupportedProjectionWindow);
        }
        crate::projection::projection_window_from_index_entries(
            &transcript_index.spans,
            Some(0),
            index.next_sequence.checked_sub(1),
            &request,
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
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::NotFound(self.state.summary.id))?;
        Ok(store
            .read_session_events_range(
                self.state.summary.id,
                start_sequence,
                end_sequence,
                max_events,
            )
            .await?)
    }

    async fn recent_history_from_db(
        &mut self,
        limit: usize,
    ) -> Result<Option<Vec<SessionEvent>>, SessionError> {
        let Some(db) = self.existing_session_db().await? else {
            return Ok(None);
        };
        let expected_last_sequence = self.state.next_sequence.saturating_sub(1);
        match db.projection_checkpoint("transcript").await? {
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
            let checkpoint = db.projection_checkpoint("input_history").await?;
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
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::NotFound(self.state.summary.id))?;
        Ok(store
            .read_session_input_history(self.state.summary.id)
            .await?)
    }

    async fn active_tool_runs(&mut self) -> Result<Vec<crate::db::ToolRun>, SessionError> {
        let Some(db) = self.existing_session_db().await? else {
            return Ok(Vec::new());
        };
        let expected_last_sequence = self.state.next_sequence.saturating_sub(1);
        let checkpoint = db.projection_checkpoint("tool_runs").await?;
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

    async fn model_context_events(&mut self) -> Result<Vec<SessionEvent>, SessionError> {
        if let Some(db) = self.existing_session_db().await? {
            let expected_last_sequence = self.state.next_sequence.saturating_sub(1);
            let checkpoint = db.projection_checkpoint("model_context").await?;
            if checkpoint.is_some_and(|checkpoint| checkpoint >= expected_last_sequence) {
                return Ok(db.model_context_events().await?);
            }
            return Err(SessionError::ProjectionStale {
                session_id: self.state.summary.id,
                projection: "model_context",
                checkpoint,
                expected: expected_last_sequence,
            });
        }
        if let Some(events) = &self.state.events {
            return Ok(model_context_events_from_history(events));
        }
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::NotFound(self.state.summary.id))?;
        let (events, refreshed_state) = store
            .read_model_context_events(self.state.summary.id)
            .await?;
        if let Some(mut state) = refreshed_state {
            state.clients.clone_from(&self.state.clients);
            state.summary.client_count = self.state.summary.client_count;
            state.sender = self.state.sender.clone();
            state.access_status = self.state.access_status;
            self.state = state;
            self.refresh_snapshot();
        }
        Ok(events)
    }

    fn set_current_agent(&mut self, agent_id: String) -> Result<(), SessionError> {
        self.state.ensure_writable()?;
        self.state.current_agent = Some(agent_id);
        self.refresh_snapshot();
        Ok(())
    }

    fn publish_transient_event(&self, kind: SessionEventKind) -> Option<SessionEvent> {
        if self.state.sender.receiver_count() == 0 {
            return None;
        }
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: self.state.next_sequence,
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
    pub access_status: SessionAccessStatus,
    pub index_status: SessionIndexStatusKind,
}

impl SessionSnapshot {
    fn from_state(state: &SessionState) -> Self {
        Self {
            summary: state.summary(),
            working_directory: state.working_directory.clone(),
            access_status: state.access_status,
            index_status: state.index_status,
        }
    }
}
