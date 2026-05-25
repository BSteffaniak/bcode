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
        self.send(|reply| SessionCommand::Attach {
            client_id,
            mode,
            activity_timestamp_ms,
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
        self.send(|reply| SessionCommand::ReplaceState { state, reply })
            .await
    }

    pub fn client_count(&self) -> usize {
        self.snapshot().summary.client_count
    }

    pub async fn shutdown(&self) -> Result<(), SessionError> {
        self.send(SessionCommand::Shutdown).await
    }
}

#[derive(Debug, Clone, Copy)]
pub enum AttachMode {
    Full,
    Recent { limit: usize },
}

enum SessionCommand {
    AppendEvent {
        kind: SessionEventKind,
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
        reply: oneshot::Sender<Result<SessionAttachment, SessionError>>,
    },
    Detach {
        client_id: ClientId,
        activity_timestamp_ms: u64,
        reply: oneshot::Sender<Result<Option<SessionEvent>, SessionError>>,
    },
    Summary(oneshot::Sender<SessionSummary>),
    WorkingDirectory(oneshot::Sender<PathBuf>),
    AccessStatus(oneshot::Sender<SessionAccessStatus>),
    History(oneshot::Sender<Result<Vec<SessionEvent>, SessionError>>),
    HistoryPage {
        query: SessionHistoryQuery,
        reply: oneshot::Sender<Result<SessionHistoryPage, SessionError>>,
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
        state: SessionState,
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
            match command {
                SessionCommand::AppendEvent {
                    kind,
                    activity_timestamp_ms,
                    reply,
                } => {
                    let _ = reply.send(self.append_event(kind, activity_timestamp_ms).await);
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
                    reply,
                } => {
                    let _ = reply.send(self.attach(client_id, mode, activity_timestamp_ms).await);
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
                SessionCommand::AccessStatus(reply) => {
                    let _ = reply.send(self.state.access_status);
                }
                SessionCommand::History(reply) => {
                    let _ = reply.send(self.history().await);
                }
                SessionCommand::HistoryPage { query, reply } => {
                    let _ = reply.send(self.history_page(query).await);
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
                    self.state = state;
                    self.refresh_snapshot();
                    let _ = reply.send(());
                }
                SessionCommand::Shutdown(reply) => {
                    let _ = reply.send(());
                    break;
                }
            }
        }
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
        activity_timestamp_ms: u64,
    ) -> Result<SessionEvent, SessionError> {
        let event = self.state.build_next_event(kind)?;
        if let Some(store) = &self.store {
            store.append_event_frame(event.clone()).await?;
        }
        self.state
            .apply_persisted_event(event.clone(), activity_timestamp_ms);
        self.state.index_status = SessionIndexStatusKind::Current;
        if let Some(store) = &self.store {
            let metadata = PersistedSessionMetadata::from_state(&self.state);
            if let Err(error) = store.write_metadata_index(metadata).await {
                self.state.index_status = SessionIndexStatusKind::Stale;
                eprintln!(
                    "failed to update session index for {}: {error}",
                    self.state.summary.id
                );
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
                    activity_timestamp_ms,
                )
                .await?,
            );
        }
        events.push(
            self.append_event(
                SessionEventKind::UserMessage { client_id, text },
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
    ) -> Result<SessionAttachment, SessionError> {
        self.state.ensure_writable()?;
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
        };
        let input_history = self.input_history().await?;
        self.state.clients.insert(client_id);
        self.state.summary.client_count = self.state.clients.len();
        let events = self.state.sender.subscribe();
        let attached_event = self
            .append_event(
                SessionEventKind::ClientAttached { client_id },
                activity_timestamp_ms,
            )
            .await?;
        let session = self.state.summary();
        Ok(SessionAttachment {
            session,
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
        self.state.ensure_writable()?;
        if self.state.clients.remove(&client_id) {
            self.state.summary.client_count = self.state.clients.len();
            return Ok(Some(
                self.append_event(
                    SessionEventKind::ClientDetached { client_id },
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

    async fn model_context_events(&self) -> Result<Vec<SessionEvent>, SessionError> {
        if let Some(events) = &self.state.events {
            return Ok(model_context_events_from_history(events));
        }
        let store = self
            .store
            .as_ref()
            .ok_or(SessionError::NotFound(self.state.summary.id))?;
        Ok(store
            .read_model_context_events(self.state.summary.id)
            .await?)
    }

    fn publish_transient_event(&self, kind: SessionEventKind) -> Option<SessionEvent> {
        if self.state.sender.receiver_count() == 0 {
            return None;
        }
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: self.state.next_sequence,
            session_id: self.state.summary.id,
            kind,
        };
        let _ = self.state.sender.send(event.clone());
        Some(event)
    }
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
