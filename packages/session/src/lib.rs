#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
// Session mutations intentionally hold the manager lock while updating in-memory
// state and appending the corresponding event so summaries/history/fanout stay
// consistent in this first implementation.
#![allow(clippy::significant_drop_tightening)]

//! Session lifecycle, attachment management, and append-only event history.

use bcode_session_models::{ClientId, SessionEvent, SessionEventKind, SessionId, SessionSummary};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::sync::{Mutex, broadcast};

const FRAME_LEN_BYTES: usize = 4;

/// Errors returned by session management operations.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(SessionId),
    #[error("session event store error: {0}")]
    Store(#[from] SessionStoreError),
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

    /// Load persisted sessions from the event store.
    ///
    /// # Errors
    ///
    /// Returns an error if the store directory cannot be read, a session file
    /// has an invalid name, or a persisted event frame cannot be decoded.
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
            let events = read_events(&path)?;
            if let Some(state) = SessionState::from_events(session_id, events) {
                sessions.insert(session_id, state);
            }
        }

        Ok(sessions)
    }

    fn append(&self, event: &SessionEvent) -> Result<(), SessionStoreError> {
        fs::create_dir_all(&self.root)?;
        let path = self.event_path(event.session_id);
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let payload = bmux_codec::to_vec(event).map_err(SessionStoreError::Encode)?;
        let payload_len = u32::try_from(payload.len())
            .map_err(|_| SessionStoreError::FrameTooLarge(payload.len()))?;
        file.write_all(&payload_len.to_le_bytes())?;
        file.write_all(&payload)?;
        file.flush()?;
        Ok(())
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
            events: Vec::new(),
            next_sequence: 0,
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

    /// Return replayable history for a session.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when the session does not exist.
    pub async fn session_history(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        let inner = self.inner.lock().await;
        let state = inner
            .sessions
            .get(&session_id)
            .ok_or(SessionError::NotFound(session_id))?;
        Ok(state.events.clone())
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
            state.clients.insert(client_id);
            state.summary.client_count = state.clients.len();
            let history = state.events.clone();
            let events = state.sender.subscribe();
            state.push_event(
                SessionEventKind::ClientAttached { client_id },
                self.store.as_ref(),
            )?;
            SessionAttachment { history, events }
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
    ) -> Result<(), SessionError> {
        {
            let mut inner = self.inner.lock().await;
            let Some(state) = inner.sessions.get_mut(&session_id) else {
                return Ok(());
            };
            if state.clients.remove(&client_id) {
                state.summary.client_count = state.clients.len();
                state.push_event(
                    SessionEventKind::ClientDetached { client_id },
                    self.store.as_ref(),
                )?;
            }
        }
        Ok(())
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
    ) -> Result<SessionEvent, SessionError> {
        let event = {
            let mut inner = self.inner.lock().await;
            let state = inner
                .sessions
                .get_mut(&session_id)
                .ok_or(SessionError::NotFound(session_id))?;
            state.push_event(
                SessionEventKind::UserMessage { client_id, text },
                self.store.as_ref(),
            )?
        };
        Ok(event)
    }
}

impl SessionState {
    fn from_events(session_id: SessionId, events: Vec<SessionEvent>) -> Option<Self> {
        if events.is_empty() {
            return None;
        }
        let name = events.iter().find_map(|event| match &event.kind {
            SessionEventKind::SessionCreated { name } => Some(name.clone()),
            _ => None,
        });
        let next_sequence = events
            .iter()
            .map(|event| event.sequence)
            .max()
            .map_or(0, |sequence| sequence + 1);
        let (sender, _) = broadcast::channel(512);
        Some(Self {
            summary: SessionSummary {
                id: session_id,
                name: name.flatten(),
                client_count: 0,
            },
            clients: BTreeSet::new(),
            events,
            next_sequence,
            sender,
        })
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
            sequence: self.next_sequence,
            session_id: self.summary.id,
            kind,
        };
        if let Some(store) = store {
            store.append(&event)?;
        }
        self.next_sequence += 1;
        self.events.push(event.clone());
        let _ = self.sender.send(event.clone());
        Ok(event)
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

fn read_events(path: &Path) -> Result<Vec<SessionEvent>, SessionStoreError> {
    let mut file = File::open(path)?;
    let mut events = Vec::new();
    loop {
        let mut len_bytes = [0_u8; FRAME_LEN_BYTES];
        match file.read_exact(&mut len_bytes) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error.into()),
        }
        let payload_len = u32::from_le_bytes(len_bytes) as usize;
        let mut payload = vec![0_u8; payload_len];
        file.read_exact(&mut payload)?;
        events.push(bmux_codec::from_bytes(&payload).map_err(SessionStoreError::Decode)?);
    }
    Ok(events)
}
