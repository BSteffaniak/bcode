//! Non-owning, bounded activity and direct-child window state transfer.

use crate::{
    SessionActivityPage, SessionActivityQuery, SessionChildPage, SessionChildQuery, SessionId,
};
use serde::{Deserialize, Serialize};

/// One selected activity query. Windows never recursively expand descendants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivityTarget {
    /// Selected canonical session.
    pub session_id: SessionId,
    /// Own activity or one bounded direct-child window.
    pub query: SessionActivityQuery,
}

/// Loaded frontend scope. Change scope by closing and opening a new watch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivityWatchScope {
    /// At most 16 selected activity queries.
    pub activity: Vec<SessionActivityTarget>,
    /// At most 8 loaded direct-child windows.
    pub children: Vec<SessionChildQuery>,
}

/// Host-bound observation identity, not execution authority or a reconnect checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivityWatchHandle {
    /// Unique subscription identity.
    pub watch_id: SessionId,
    /// Fresh lifetime epoch; reopening never resumes an old epoch.
    pub epoch: SessionId,
}

/// Complete bounded replacement for precisely the requested scope, in request order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivityWatchSnapshot {
    /// Query failures remain explicit; unknown does not mean idle or absent.
    pub activity: Vec<Result<SessionActivityPage, crate::application::SessionApplicationFailure>>,
    /// Loaded relationship windows, independently of activity queries.
    pub children: Vec<Result<SessionChildPage, crate::application::SessionApplicationFailure>>,
}

/// Why a complete replacement is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionActivityWatchUpdate {
    /// First state in this subscription epoch.
    Initial,
    /// A semantic change, not merely an observation timestamp change.
    Changed,
    /// Source lag or skipped shared revisions invalidated the previous view.
    /// Discard the previous view and replace it with this bounded resnapshot.
    Reset,
}

/// Ordered subscription-local delivery. No durable replay or acknowledgment is implied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivityWatchDelivery {
    /// Identity and fresh epoch of this subscription.
    pub handle: SessionActivityWatchHandle,
    /// Zero-based contiguous delivery order, unrelated to durable event sequences.
    pub sequence: u64,
    /// Replacement semantics, including explicit lag invalidation.
    pub update: SessionActivityWatchUpdate,
    /// Full bounded replacement for the admitted scope.
    pub snapshot: SessionActivityWatchSnapshot,
}
