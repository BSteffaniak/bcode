//! Session actor, handle, and snapshot plumbing.

use super::*;
use crate::store_executor::PersistedSessionMetadata;
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

    pub async fn history_page(
        &self,
        query: SessionHistoryQuery,
    ) -> Result<SessionHistoryPage, SessionError> {
        self.send(|reply| SessionCommand::HistoryPage { query, reply })
            .await?
    }

    pub async fn projection_window(
        &self,
        request: ProjectionWindowRequest,
    ) -> Result<ProjectionWindow, SessionError> {
        self.send(|reply| SessionCommand::ProjectionWindow { request, reply })
            .await?
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

    pub async fn current_model_selection(&self) -> Result<Option<(String, String)>, SessionError> {
        self.send(SessionCommand::CurrentModelSelection).await
    }

    pub async fn current_agent_selection(&self) -> Result<Option<String>, SessionError> {
        self.send(SessionCommand::CurrentAgentSelection).await
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
    HistoryPage {
        query: SessionHistoryQuery,
        reply: oneshot::Sender<Result<SessionHistoryPage, SessionError>>,
    },
    ProjectionWindow {
        request: ProjectionWindowRequest,
        reply: oneshot::Sender<Result<ProjectionWindow, SessionError>>,
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
    ModelContextEvents(oneshot::Sender<Result<Vec<SessionEvent>, SessionError>>),
    CurrentModelSelection(oneshot::Sender<Option<(String, String)>>),
    CurrentAgentSelection(oneshot::Sender<Option<String>>),
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
            SessionCommand::HistoryPage { query, reply } => {
                let _ = reply.send(self.history_page(query).await);
            }
            SessionCommand::ProjectionWindow { request, reply } => {
                let _ = reply.send(self.projection_window(request).await);
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

    async fn append_event(
        &mut self,
        kind: SessionEventKind,
        provenance: Option<SessionEventProvenance>,
        activity_timestamp_ms: u64,
    ) -> Result<SessionEvent, SessionError> {
        let mut event = self.state.build_next_event(kind)?;
        event.provenance = provenance;
        if let Some(store) = &self.store {
            let append_started_at = Instant::now();
            store.append_event_frame(event.clone()).await?;
            store.metrics().record_histogram(
                "session.actor.append_event.append_frame_duration_ms",
                elapsed_ms(append_started_at),
            );
        }
        self.state
            .apply_persisted_event(event.clone(), activity_timestamp_ms);
        self.state.index_status = SessionIndexStatusKind::Current;
        if let Some(store) = &self.store {
            self.state.index_status = SessionIndexStatusKind::Stale;
            let metadata = PersistedSessionMetadata::from_state(&self.state);
            let write_index_started_at = Instant::now();
            match store
                .write_metadata_index(metadata, Some(event.clone()))
                .await
            {
                Ok(()) => {
                    store.metrics().record_histogram(
                        "session.actor.append_event.write_metadata_index_duration_ms",
                        elapsed_ms(write_index_started_at),
                    );
                    self.state.index_status = SessionIndexStatusKind::Current;
                }
                Err(error) => {
                    store.metrics().record_histogram(
                        "session.actor.append_event.write_metadata_index_duration_ms",
                        elapsed_ms(write_index_started_at),
                    );
                    eprintln!(
                        "failed to update session index for {}: {error}",
                        self.state.summary.id
                    );
                }
            }
        }
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
                self.history_page(SessionHistoryQuery {
                    cursor: None,
                    limit,
                    direction: SessionHistoryDirection::Backward,
                })
                .await?
                .events
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
                self.history_page(SessionHistoryQuery {
                    cursor: None,
                    limit,
                    direction: SessionHistoryDirection::Backward,
                })
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

    async fn history(&self) -> Result<Vec<SessionEvent>, SessionError> {
        if let Some(events) = &self.state.events {
            return Ok(events.clone());
        }
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::NotFound(self.state.summary.id))?;
        Ok(store.read_session_events(self.state.summary.id).await?)
    }

    async fn history_page(
        &mut self,
        query: SessionHistoryQuery,
    ) -> Result<SessionHistoryPage, SessionError> {
        if let Some(events) = &self.state.events {
            return Ok(history_page_from_events(
                self.state.summary.id,
                events.clone(),
                query,
            ));
        }
        let should_mark_current = self.state.index_status == SessionIndexStatusKind::Stale;
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::NotFound(self.state.summary.id))?;
        let page = store
            .read_session_history_page(self.state.summary.id, query)
            .await?;
        if should_mark_current {
            self.state.index_status = SessionIndexStatusKind::Current;
            self.refresh_snapshot();
        }
        Ok(page)
    }

    async fn projection_window(
        &self,
        request: ProjectionWindowRequest,
    ) -> Result<ProjectionWindow, SessionError> {
        let started_at = Instant::now();
        let events = self.history().await?;
        let window = crate::projection::projection_window_from_events(&events, &request)
            .ok_or(SessionError::UnsupportedProjectionWindow)?;
        if let Some(metrics) = self.store.as_ref().map(SessionStoreExecutor::metrics) {
            metrics.record_histogram(
                "session.actor.projection_window.duration_ms",
                elapsed_ms(started_at),
            );
            metrics.record_histogram(
                "session.actor.projection_window.scanned_event_count",
                usize_to_u64(window.scanned_events),
            );
            metrics.record_histogram(
                "session.actor.projection_window.selected_item_count",
                usize_to_u64(window.transcript_items.len()),
            );
            metrics.record_histogram(
                "session.actor.projection_window.selected_event_count",
                window
                    .source_range
                    .map_or(0, |range| range.end_sequence - range.start_sequence + 1),
            );
            metrics.record_histogram(
                "session.actor.projection_window.estimated_row_count",
                usize_to_u64(
                    window
                        .transcript_items
                        .iter()
                        .map(|item| item.estimated_rows.unwrap_or(1))
                        .sum(),
                ),
            );
        }
        Ok(window)
    }

    async fn projection_window_from_index(
        &self,
        request: ProjectionWindowRequest,
    ) -> Result<ProjectionWindow, SessionError> {
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
        &self,
        start_sequence: u64,
        end_sequence: u64,
        max_events: usize,
    ) -> Result<Vec<SessionEvent>, SessionError> {
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

    async fn input_history(&self) -> Result<Vec<SessionInputHistoryEntry>, SessionError> {
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

    async fn model_context_events(&mut self) -> Result<Vec<SessionEvent>, SessionError> {
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
    access_status: SessionAccessStatus,
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
