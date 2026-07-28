//! Session attachment, subscription, and committed-mutation boundary types.

use bcode_session_models::{
    SessionEvent, SessionId, SessionInputHistoryEntry, SessionLiveEvent, SessionSummary,
};
use tokio::sync::broadcast;

/// Active session attachment with replay history and event receivers.
#[derive(Debug)]
pub struct SessionAttachment {
    pub session: SessionSummary,
    pub history: Vec<SessionEvent>,
    pub input_history: Vec<SessionInputHistoryEntry>,
    pub live_checkpoints: Vec<SessionLiveEvent>,
    pub events: broadcast::Receiver<SessionEvent>,
    pub live_events: broadcast::Receiver<SessionLiveEvent>,
}

/// Non-mutating event subscription for a session.
#[derive(Debug)]
pub struct SessionEventSubscription {
    pub session: SessionSummary,
    pub events: broadcast::Receiver<SessionEvent>,
    pub live_events: broadcast::Receiver<SessionLiveEvent>,
}

/// Notification emitted after a durable session mutation is committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMutationCommitted {
    pub session_id: SessionId,
    pub event: SessionEvent,
    pub summary: SessionSummary,
}

/// Active session attachment plus projection-window metadata.
#[derive(Debug)]
pub struct SessionProjectionWindowAttachment {
    pub attachment: SessionAttachment,
    pub projection_window: bcode_session_models::ProjectionWindow,
}
