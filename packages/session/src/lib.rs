#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
// Session mutations intentionally hold the manager lock while updating in-memory
// state and broadcasting the corresponding event so summaries/history/fanout stay
// consistent in this first in-memory implementation.
#![allow(clippy::significant_drop_tightening)]

//! Session lifecycle and attachment management for bcode.

use bcode_session_models::{ClientId, SessionEvent, SessionEventKind, SessionId, SessionSummary};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
use tokio::sync::{Mutex, broadcast};

/// Errors returned by session management operations.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(SessionId),
}

/// In-memory session manager.
#[derive(Debug, Default)]
pub struct SessionManager {
    inner: Mutex<SessionManagerInner>,
}

#[derive(Debug, Default)]
struct SessionManagerInner {
    sessions: BTreeMap<SessionId, SessionState>,
}

#[derive(Debug)]
struct SessionState {
    summary: SessionSummary,
    clients: BTreeSet<ClientId>,
    events: Vec<SessionEvent>,
    next_sequence: u64,
    sender: broadcast::Sender<SessionEvent>,
}

/// Active session attachment.
#[derive(Debug)]
pub struct SessionAttachment {
    pub history: Vec<SessionEvent>,
    pub events: broadcast::Receiver<SessionEvent>,
}

impl SessionManager {
    /// Create a new session.
    pub async fn create_session(&self, name: Option<String>) -> SessionSummary {
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
            events: Vec::new(),
            next_sequence: 0,
            sender,
        };
        state.push_event(SessionEventKind::SessionCreated { name });
        inner.sessions.insert(id, state);
        summary
    }

    /// List known sessions.
    pub async fn list_sessions(&self) -> Vec<SessionSummary> {
        let inner = self.inner.lock().await;
        inner.sessions.values().map(SessionState::summary).collect()
    }

    /// Attach a client to an existing session.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
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
            state.clients.insert(client_id);
            state.summary.client_count = state.clients.len();
            let events = state.sender.subscribe();
            state.push_event(SessionEventKind::ClientAttached { client_id });
            SessionAttachment {
                history: state.events.clone(),
                events,
            }
        };
        Ok(attachment)
    }

    /// Detach a client from a session if it is currently attached.
    pub async fn detach_session(&self, session_id: SessionId, client_id: ClientId) {
        {
            let mut inner = self.inner.lock().await;
            let Some(state) = inner.sessions.get_mut(&session_id) else {
                return;
            };
            if state.clients.remove(&client_id) {
                state.summary.client_count = state.clients.len();
                state.push_event(SessionEventKind::ClientDetached { client_id });
            }
        }
    }

    /// Append a user message to a session.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn append_user_message(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        text: String,
    ) -> Result<SessionEvent, SessionError> {
        let event = {
            let mut inner = self.inner.lock().await;
            let state = inner
                .sessions
                .get_mut(&session_id)
                .ok_or(SessionError::NotFound(session_id))?;
            state.push_event(SessionEventKind::UserMessage { client_id, text })
        };
        Ok(event)
    }
}

impl SessionState {
    fn summary(&self) -> SessionSummary {
        self.summary.clone()
    }

    fn push_event(&mut self, kind: SessionEventKind) -> SessionEvent {
        let event = SessionEvent {
            sequence: self.next_sequence,
            session_id: self.summary.id,
            kind,
        };
        self.next_sequence += 1;
        self.events.push(event.clone());
        let _ = self.sender.send(event.clone());
        event
    }
}
