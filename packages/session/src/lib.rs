#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
// Session mutations intentionally hold the manager lock while updating in-memory
// state and appending the corresponding event so summaries/history/fanout stay
// consistent in this first implementation.
#![allow(clippy::significant_drop_tightening)]

//! Session lifecycle, attachment management, and append-only event history.

pub(crate) mod index;
pub(crate) mod reader;

pub use index::SessionIndexHealth;

use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, ModelTurnOutcome, SessionEvent,
    SessionEventKind, SessionHistoryCursor, SessionHistoryDirection, SessionHistoryPage,
    SessionHistoryQuery, SessionId, SessionSummary, SessionTokenUsage, SessionTraceEvent,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::sync::{Mutex, broadcast};

/// Errors returned by session management operations.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(SessionId),
    #[error("session event store error: {0}")]
    Store(#[from] SessionStoreError),
    #[error("session has connected clients: {0}")]
    ConnectedClients(SessionId),
}

/// Errors returned by the append-only session event store.
#[derive(Debug, Error)]
pub enum SessionStoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to encode session event: {0}")]
    Encode(#[source] bmux_codec::Error),
    #[error("failed to decode session event: {0}")]
    Decode(#[source] bmux_codec::Error),
    #[error("session index error: {0}")]
    Index(#[source] serde_json::Error),
    #[error("session event frame is too large: {0} bytes")]
    FrameTooLarge(usize),
    #[error("session event file has a non-UTF-8 or missing file stem: {0:?}")]
    InvalidFileName(PathBuf),
    #[error("session event file name is not a session ID: {0}")]
    InvalidSessionId(String),
}

/// Append-only event store for session histories.
#[derive(Debug, Clone)]
pub struct SessionEventStore {
    root: PathBuf,
}

impl SessionEventStore {
    /// Create an event store rooted at the provided directory.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn load_sessions(&self) -> Result<BTreeMap<SessionId, SessionState>, SessionStoreError> {
        let mut sessions = BTreeMap::new();
        if !self.root.exists() {
            return Ok(sessions);
        }

        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("events") {
                continue;
            }
            let session_id = parse_session_file_name(&path)?;
            let index = match index::load_fresh_index(&self.root, session_id, &path)? {
                Some(index) => Some(index),
                None => index::rebuild_index(&self.root, session_id, &path)?.0,
            };
            if let Some(index) = index {
                sessions.insert(session_id, index.into_state());
            }
        }

        Ok(sessions)
    }

    fn append(&self, event: &SessionEvent) -> Result<(), SessionStoreError> {
        fs::create_dir_all(&self.root)?;
        let path = self.event_path(event.session_id);
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let payload = bmux_codec::to_vec(event).map_err(SessionStoreError::Encode)?;
        let payload_len = u32::try_from(payload.len())
            .map_err(|_| SessionStoreError::FrameTooLarge(payload.len()))?;
        file.write_all(&payload_len.to_le_bytes())?;
        file.write_all(&payload)?;
        file.flush()?;
        Ok(())
    }

    fn read_session_events(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, SessionStoreError> {
        let path = self.event_path(session_id);
        Ok(reader::read_events(&path)?.events)
    }

    fn write_state_index(&self, state: &SessionState) -> Result<(), SessionStoreError> {
        let path = self.event_path(state.summary.id);
        let file = index::fingerprint(&path)?;
        let index = index::SessionIndex {
            index_version: index::SESSION_INDEX_VERSION,
            session_id: state.summary.id,
            last_good_offset: file.len,
            file,
            summary: SessionSummary {
                client_count: 0,
                ..state.summary.clone()
            },
            next_sequence: state.next_sequence,
            event_count: state.event_count,
            has_user_message: state.has_user_message,
            current_provider: state.current_provider.clone(),
            current_model: state.current_model.clone(),
            current_agent: state.current_agent.clone(),
            latest_compaction_sequence: state.latest_compaction_sequence,
            total_metered_tokens: state.total_metered_tokens,
            issues: state.index_issues.clone(),
        };
        index::write_index(&self.root, &index)
    }

    /// Rebuild the sidecar index for one session from its canonical event log.
    ///
    /// # Errors
    ///
    /// Returns an error if the event file cannot be read or the index cannot be written.
    pub fn reindex_session(&self, session_id: SessionId) -> Result<(), SessionStoreError> {
        let path = self.event_path(session_id);
        let _ = index::rebuild_index(&self.root, session_id, &path)?;
        Ok(())
    }

    /// Rebuild every session sidecar index under this store.
    ///
    /// # Errors
    ///
    /// Returns an error if the session directory cannot be scanned or any index cannot be rebuilt.
    pub fn reindex_all(&self) -> Result<Vec<SessionId>, SessionStoreError> {
        let mut rebuilt = Vec::new();
        if !self.root.exists() {
            return Ok(rebuilt);
        }
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("events") {
                continue;
            }
            let session_id = parse_session_file_name(&path)?;
            self.reindex_session(session_id)?;
            rebuilt.push(session_id);
        }
        Ok(rebuilt)
    }

    /// Return index health for every persisted session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session directory or an index file cannot be read.
    pub fn doctor_all(&self) -> Result<Vec<SessionIndexHealth>, SessionStoreError> {
        let mut health = Vec::new();
        if !self.root.exists() {
            return Ok(health);
        }
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("events") {
                continue;
            }
            let session_id = parse_session_file_name(&path)?;
            let index = index::load_fresh_index(&self.root, session_id, &path)?;
            if let Some(index) = index {
                health.push(index.health(false));
            } else {
                let (index, _) = index::rebuild_index(&self.root, session_id, &path)?;
                if let Some(index) = index {
                    health.push(index.health(true));
                }
            }
        }
        Ok(health)
    }

    fn delete(&self, session_id: SessionId) -> Result<(), SessionStoreError> {
        let path = self.event_path(session_id);
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(SessionStoreError::Io(error)),
        }
        match fs::remove_file(index::index_path(&self.root, session_id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(SessionStoreError::Io(error)),
        }
    }

    fn event_path(&self, session_id: SessionId) -> PathBuf {
        self.root.join(format!("{session_id}.events"))
    }
}

/// In-memory session manager with optional append-only persistence.
#[derive(Debug, Default)]
pub struct SessionManager {
    inner: Mutex<SessionManagerInner>,
    store: Option<SessionEventStore>,
}

#[derive(Debug, Default)]
struct SessionManagerInner {
    sessions: BTreeMap<SessionId, SessionState>,
}

#[derive(Debug)]
pub(crate) struct SessionState {
    summary: SessionSummary,
    clients: BTreeSet<ClientId>,
    events: Option<Vec<SessionEvent>>,
    next_sequence: u64,
    event_count: usize,
    has_user_message: bool,
    current_provider: Option<String>,
    current_model: Option<String>,
    current_agent: Option<String>,
    latest_compaction_sequence: Option<u64>,
    total_metered_tokens: u64,
    index_issues: Vec<index::SessionIndexIssue>,
    sender: broadcast::Sender<SessionEvent>,
}

/// Active session attachment.
#[derive(Debug)]
pub struct SessionAttachment {
    pub history: Vec<SessionEvent>,
    pub attached_event: SessionEvent,
    pub events: broadcast::Receiver<SessionEvent>,
}

impl SessionManager {
    /// Create a session manager backed by an append-only event store.
    ///
    /// # Errors
    ///
    /// Returns an error if persisted session history cannot be loaded.
    pub fn persistent(root: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        let store = SessionEventStore::new(root);
        let sessions = store.load_sessions()?;
        Ok(Self {
            inner: Mutex::new(SessionManagerInner { sessions }),
            store: Some(store),
        })
    }

    /// Create a new session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session-created event cannot be persisted.
    pub async fn create_session(
        &self,
        name: Option<String>,
    ) -> Result<SessionSummary, SessionError> {
        let mut inner = self.inner.lock().await;
        let id = SessionId::new();
        let (sender, _) = broadcast::channel(512);
        let summary = SessionSummary {
            id,
            name: name.clone(),
            client_count: 0,
        };
        let mut state = SessionState {
            summary: summary.clone(),
            clients: BTreeSet::new(),
            events: Some(Vec::new()),
            next_sequence: 0,
            event_count: 0,
            has_user_message: false,
            current_provider: None,
            current_model: None,
            current_agent: None,
            latest_compaction_sequence: None,
            total_metered_tokens: 0,
            index_issues: Vec::new(),
            sender,
        };
        state.push_event(
            SessionEventKind::SessionCreated { name },
            self.store.as_ref(),
        )?;
        inner.sessions.insert(id, state);
        Ok(summary)
    }

    /// List known sessions.
    pub async fn list_sessions(&self) -> Vec<SessionSummary> {
        let inner = self.inner.lock().await;
        inner.sessions.values().map(SessionState::summary).collect()
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
        let normalized_name = normalize_session_name(name);
        let event = {
            let mut inner = self.inner.lock().await;
            let state = inner
                .sessions
                .get_mut(&session_id)
                .ok_or(SessionError::NotFound(session_id))?;
            state.summary.name.clone_from(&normalized_name);
            state.push_event(
                SessionEventKind::SessionRenamed {
                    name: normalized_name,
                },
                self.store.as_ref(),
            )?
        };
        Ok(event)
    }

    /// Delete a session.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the session has connected clients
    /// * the persistent event file cannot be removed
    pub async fn delete_session(
        &self,
        session_id: SessionId,
    ) -> Result<SessionSummary, SessionError> {
        let mut inner = self.inner.lock().await;
        let state = inner
            .sessions
            .get(&session_id)
            .ok_or(SessionError::NotFound(session_id))?;
        if !state.clients.is_empty() {
            return Err(SessionError::ConnectedClients(session_id));
        }
        if let Some(store) = &self.store {
            store.delete(session_id)?;
        }
        let removed = inner
            .sessions
            .remove(&session_id)
            .ok_or(SessionError::NotFound(session_id))?;
        Ok(removed.summary)
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
        let inner = self.inner.lock().await;
        inner
            .sessions
            .get(&session_id)
            .map(SessionState::summary)
            .ok_or(SessionError::NotFound(session_id))
    }

    /// Return replayable history for a session.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn session_history(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        let mut inner = self.inner.lock().await;
        let state = inner
            .sessions
            .get_mut(&session_id)
            .ok_or(SessionError::NotFound(session_id))?;
        if state.events.is_none() {
            let store = self
                .store
                .as_ref()
                .ok_or(SessionError::NotFound(session_id))?;
            state.events = Some(store.read_session_events(session_id)?);
        }
        Ok(state.events.clone().unwrap_or_default())
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
        let history = self.session_history(session_id).await?;
        let limit = query.limit.max(1);
        let events = match query.direction {
            SessionHistoryDirection::Forward => history
                .into_iter()
                .filter(|event| {
                    query
                        .cursor
                        .is_none_or(|cursor| event.sequence >= cursor.sequence)
                })
                .take(limit.saturating_add(1))
                .collect::<Vec<_>>(),
            SessionHistoryDirection::Backward => {
                let mut events = history
                    .into_iter()
                    .rev()
                    .filter(|event| {
                        query
                            .cursor
                            .is_none_or(|cursor| event.sequence <= cursor.sequence)
                    })
                    .take(limit.saturating_add(1))
                    .collect::<Vec<_>>();
                events.reverse();
                events
            }
        };
        let has_more = events.len() > limit;
        let page_events = if has_more {
            match query.direction {
                SessionHistoryDirection::Forward => events.into_iter().take(limit).collect(),
                SessionHistoryDirection::Backward => events.into_iter().skip(1).collect(),
            }
        } else {
            events
        };
        let next_cursor = if has_more {
            page_events.last().map(|event| SessionHistoryCursor {
                sequence: match query.direction {
                    SessionHistoryDirection::Forward => event.sequence.saturating_add(1),
                    SessionHistoryDirection::Backward => event.sequence.saturating_sub(1),
                },
            })
        } else {
            None
        };
        Ok(SessionHistoryPage {
            session_id,
            events: page_events,
            next_cursor,
            has_more,
        })
    }

    /// Return the latest session-specific model selection if one has been set.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn current_model_selection(
        &self,
        session_id: SessionId,
    ) -> Result<Option<(String, String)>, SessionError> {
        let inner = self.inner.lock().await;
        let state = inner
            .sessions
            .get(&session_id)
            .ok_or(SessionError::NotFound(session_id))?;
        Ok(state
            .current_provider
            .clone()
            .zip(state.current_model.clone()))
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
        let inner = self.inner.lock().await;
        let state = inner
            .sessions
            .get(&session_id)
            .ok_or(SessionError::NotFound(session_id))?;
        Ok(state.current_agent.clone())
    }

    /// Attach a client to an existing session.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the client-attached event cannot be persisted
    pub async fn attach_session(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<SessionAttachment, SessionError> {
        let attachment = {
            let mut inner = self.inner.lock().await;
            let state = inner
                .sessions
                .get_mut(&session_id)
                .ok_or(SessionError::NotFound(session_id))?;
            if state.events.is_none() {
                let store = self
                    .store
                    .as_ref()
                    .ok_or(SessionError::NotFound(session_id))?;
                state.events = Some(store.read_session_events(session_id)?);
            }
            state.clients.insert(client_id);
            state.summary.client_count = state.clients.len();
            let history = state.events.clone().unwrap_or_default();
            let events = state.sender.subscribe();
            let attached_event = state.push_event(
                SessionEventKind::ClientAttached { client_id },
                self.store.as_ref(),
            )?;
            SessionAttachment {
                history,
                attached_event,
                events,
            }
        };
        Ok(attachment)
    }

    /// Detach a client from a session if it is currently attached.
    ///
    /// # Errors
    ///
    /// Returns an error if the client-detached event cannot be persisted.
    pub async fn detach_session(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<Option<SessionEvent>, SessionError> {
        let mut inner = self.inner.lock().await;
        let Some(state) = inner.sessions.get_mut(&session_id) else {
            return Ok(None);
        };
        if state.clients.remove(&client_id) {
            state.summary.client_count = state.clients.len();
            return Ok(Some(state.push_event(
                SessionEventKind::ClientDetached { client_id },
                self.store.as_ref(),
            )?));
        }
        Ok(None)
    }

    /// Append a user message to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the user-message event cannot be persisted
    pub async fn append_user_message(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        text: String,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        let events = {
            let mut inner = self.inner.lock().await;
            let state = inner
                .sessions
                .get_mut(&session_id)
                .ok_or(SessionError::NotFound(session_id))?;
            let mut events = Vec::new();
            if state.summary.name.is_none() && !state.has_user_message {
                let title = title_from_first_prompt(&text);
                state.summary.name = Some(title.clone());
                events.push(state.push_event(
                    SessionEventKind::SessionRenamed { name: Some(title) },
                    self.store.as_ref(),
                )?);
            }
            events.push(state.push_event(
                SessionEventKind::UserMessage { client_id, text },
                self.store.as_ref(),
            )?);
            events
        };
        Ok(events)
    }

    /// Append an assistant streaming delta to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_assistant_delta(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::AssistantDelta { text })
            .await
    }

    /// Append a complete assistant message to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_assistant_message(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::AssistantMessage { text })
            .await
    }

    /// Append a tool-call request event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_tool_call_requested(
        &self,
        session_id: SessionId,
        tool_call_id: String,
        tool_name: String,
        arguments_json: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ToolCallRequested {
                tool_call_id,
                tool_name,
                arguments_json,
            },
        )
        .await
    }

    /// Append a tool-call finished event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_tool_call_finished(
        &self,
        session_id: SessionId,
        tool_call_id: String,
        result: String,
        is_error: bool,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
            },
        )
        .await
    }

    /// Append a permission-requested event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_permission_requested(
        &self,
        session_id: SessionId,
        permission_id: String,
        tool_call_id: String,
        tool_name: String,
        arguments_json: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                tool_name,
                arguments_json,
            },
        )
        .await
    }

    /// Append a permission-resolved event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_permission_resolved(
        &self,
        session_id: SessionId,
        permission_id: String,
        approved: bool,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::PermissionResolved {
                permission_id,
                approved,
            },
        )
        .await
    }

    /// Append a model-changed event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_model_changed(
        &self,
        session_id: SessionId,
        provider: String,
        model: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ModelChanged { provider, model },
        )
        .await
    }

    /// Append an agent-changed event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_agent_changed(
        &self,
        session_id: SessionId,
        agent_id: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::AgentChanged { agent_id })
            .await
    }

    /// Append a model-turn-started event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_model_turn_started(
        &self,
        session_id: SessionId,
        turn_id: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::ModelTurnStarted { turn_id })
            .await
    }

    /// Append a model-turn-finished event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_model_turn_finished(
        &self,
        session_id: SessionId,
        turn_id: String,
        outcome: ModelTurnOutcome,
        message: Option<String>,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ModelTurnFinished {
                turn_id,
                outcome,
                message,
            },
        )
        .await
    }

    /// Append provider-neutral token usage to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_model_usage(
        &self,
        session_id: SessionId,
        turn_id: String,
        usage: SessionTokenUsage,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::ModelUsage { turn_id, usage })
            .await
    }

    /// Append a system message to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_system_message(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(session_id, SessionEventKind::SystemMessage { text })
            .await
    }

    /// Append a context-compaction summary to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_context_compacted(
        &self,
        session_id: SessionId,
        summary: String,
        compacted_through_sequence: u64,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::ContextCompacted {
                summary,
                compacted_through_sequence,
            },
        )
        .await
    }

    /// Append a diagnostic trace event.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the event cannot be persisted.
    pub async fn append_trace_event(
        &self,
        session_id: SessionId,
        trace: SessionTraceEvent,
    ) -> Result<SessionEvent, SessionError> {
        self.append_event(
            session_id,
            SessionEventKind::TraceEvent {
                trace: Box::new(trace),
            },
        )
        .await
    }

    /// Append an event to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the event cannot be persisted
    pub async fn append_event(
        &self,
        session_id: SessionId,
        kind: SessionEventKind,
    ) -> Result<SessionEvent, SessionError> {
        let event = {
            let mut inner = self.inner.lock().await;
            let state = inner
                .sessions
                .get_mut(&session_id)
                .ok_or(SessionError::NotFound(session_id))?;
            state.push_event(kind, self.store.as_ref())?
        };
        Ok(event)
    }
}

impl SessionState {
    pub(crate) fn from_index(index: index::SessionIndex) -> Self {
        let (sender, _) = broadcast::channel(512);
        Self {
            summary: index.summary,
            clients: BTreeSet::new(),
            events: None,
            next_sequence: index.next_sequence,
            event_count: index.event_count,
            has_user_message: index.has_user_message,
            current_provider: index.current_provider,
            current_model: index.current_model,
            current_agent: index.current_agent,
            latest_compaction_sequence: index.latest_compaction_sequence,
            total_metered_tokens: index.total_metered_tokens,
            index_issues: index.issues,
            sender,
        }
    }

    fn summary(&self) -> SessionSummary {
        self.summary.clone()
    }

    fn push_event(
        &mut self,
        kind: SessionEventKind,
        store: Option<&SessionEventStore>,
    ) -> Result<SessionEvent, SessionStoreError> {
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: self.next_sequence,
            session_id: self.summary.id,
            kind,
        };
        if let Some(store) = store {
            store.append(&event)?;
        }
        self.next_sequence += 1;
        self.event_count = self.event_count.saturating_add(1);
        match &event.kind {
            SessionEventKind::UserMessage { .. } => self.has_user_message = true,
            SessionEventKind::ModelChanged { provider, model } => {
                self.current_provider = Some(provider.clone());
                self.current_model = Some(model.clone());
            }
            SessionEventKind::AgentChanged { agent_id } => {
                self.current_agent = Some(agent_id.clone());
            }
            SessionEventKind::ContextCompacted {
                compacted_through_sequence,
                ..
            } => {
                self.latest_compaction_sequence = Some(*compacted_through_sequence);
            }
            SessionEventKind::ModelUsage { usage, .. } => {
                if let Some(total) = usage.metered_total_tokens() {
                    self.total_metered_tokens =
                        self.total_metered_tokens.saturating_add(u64::from(total));
                }
            }
            _ => {}
        }
        if let Some(events) = &mut self.events {
            events.push(event.clone());
        }
        if let Some(store) = store
            && let Err(error) = store.write_state_index(self)
        {
            eprintln!(
                "failed to update session index for {}: {error}",
                self.summary.id
            );
        }
        let _ = self.sender.send(event.clone());
        Ok(event)
    }
}

fn normalize_session_name(name: Option<String>) -> Option<String> {
    name.map(|value| squish_whitespace(&value))
        .filter(|value| !value.is_empty())
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

fn parse_session_file_name(path: &Path) -> Result<SessionId, SessionStoreError> {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| SessionStoreError::InvalidFileName(path.to_path_buf()))?;
    stem.parse()
        .map_err(|_| SessionStoreError::InvalidSessionId(stem.to_string()))
}

#[cfg(test)]
mod tests {
    use super::SessionManager;
    use bcode_session_models::{
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, SessionEvent, SessionEventKind,
        SessionTraceEvent, SessionTracePayload, SessionTracePhase,
    };
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn trace_event_round_trips_through_bmux_codec() {
        let mut metadata = BTreeMap::new();
        metadata.insert("conversation_hash".to_string(), "abc123".to_string());
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 0,
            session_id: bcode_session_models::SessionId::new(),
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
    #[allow(clippy::too_many_lines)]
    async fn persistent_manager_restores_session_history() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("test".to_string()))
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
                "tool-1".to_string(),
                "read".to_string(),
                r#"{"path":"README.md"}"#.to_string(),
            )
            .await
            .expect("tool request should append");
        manager
            .append_tool_call_finished(session.id, "tool-1".to_string(), "ok".to_string(), false)
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
        let sessions = restored.list_sessions().await;
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
            SessionEventKind::ToolCallFinished { tool_call_id, result, is_error }
                if tool_call_id == "tool-1" && result == "ok" && !is_error
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

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn persistent_manager_ignores_corrupt_session_tail() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("test".to_string()))
            .await
            .expect("session should be created");
        manager
            .append_user_message(session.id, ClientId::new(), "hello".to_string())
            .await
            .expect("message should append");

        let path = root.join(format!("{}.events", session.id));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("event file should open");
        file.write_all(&3_u32.to_le_bytes())
            .expect("corrupt frame length should append");
        file.write_all(&[1_u8])
            .expect("partial corrupt frame should append");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let history = restored
            .session_history(session.id)
            .await
            .expect("history should load");

        assert!(history.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::UserMessage { text, .. } if text == "hello"
        )));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn unnamed_session_uses_first_prompt_as_title() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(None)
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
        let sessions = manager.list_sessions().await;
        assert_eq!(
            sessions[0].name.as_deref(),
            Some("Fix session selection UX")
        );

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let restored_sessions = restored.list_sessions().await;
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
            .create_session(Some("Manual title".to_string()))
            .await
            .expect("session should be created");

        let events = manager
            .append_user_message(session.id, ClientId::new(), "Different title".to_string())
            .await
            .expect("message should append");

        assert_eq!(events.len(), 1);
        let sessions = manager.list_sessions().await;
        assert_eq!(sessions[0].name.as_deref(), Some("Manual title"));
    }

    #[tokio::test]
    async fn rename_session_restores_latest_name() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("Old title".to_string()))
            .await
            .expect("session should be created");

        manager
            .rename_session(session.id, Some("  New   title  ".to_string()))
            .await
            .expect("session should rename");

        let restored = SessionManager::persistent(&root).expect("manager should restore");
        let sessions = restored.list_sessions().await;
        assert_eq!(sessions[0].name.as_deref(), Some("New title"));

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[tokio::test]
    async fn delete_session_removes_persisted_history() {
        let root = unique_temp_dir();
        let manager = SessionManager::persistent(&root).expect("manager should initialize");
        let session = manager
            .create_session(Some("Delete me".to_string()))
            .await
            .expect("session should be created");

        manager
            .delete_session(session.id)
            .await
            .expect("session should delete");

        assert!(manager.list_sessions().await.is_empty());
        let restored = SessionManager::persistent(&root).expect("manager should restore");
        assert!(restored.list_sessions().await.is_empty());

        std::fs::remove_dir_all(root).expect("temp dir should clean up");
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
