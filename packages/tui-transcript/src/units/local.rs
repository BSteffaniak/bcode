//! Process-local rows.
//!
//! The host shell owns presentation that is not canonical session history: submissions it
//! staged and has not seen accepted ("You · sending", "queued") and process-local notices
//! (errors, slash-command output, plugin activity). The engine never derives these rows from
//! the session view; the host passes them as plain data through
//! [`crate::engine::TranscriptEngine::set_local_rows`], and they become [`UnitKey::Local`]
//! units placed relative to the canonical items.

use std::fmt;

use bcode_session_view_models::{TranscriptViewItem, TranscriptViewItemId};

/// Stable identity of one local row.
///
/// [`UnitKey::Local`](super::UnitKey::Local) units are keyed by it, so a reading anchor on a
/// local row survives updates, and it is never reused for a different row.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LocalKey {
    /// A process-local notice, by the host's notice identity.
    Notice(u64),
    /// A submission the host staged and the session has not accepted yet, by submission id.
    Submission(String),
}

impl fmt::Display for LocalKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Notice(id) => write!(f, "notice:{id}"),
            Self::Submission(id) => write!(f, "submission:{id}"),
        }
    }
}

/// Where a local row stands relative to canonical items.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LocalPlacement {
    /// Directly after the canonical item `item` (before the next canonical item), the
    /// chronology boundary the row was recorded at. When `item` is not resident, the row
    /// follows the last resident item whose sequence is at most `sequence`; without either it
    /// precedes every resident item.
    After {
        /// The canonical item the row follows.
        item: Option<TranscriptViewItemId>,
        /// The newest canonical sequence at or before the row (fallback placement).
        sequence: Option<u64>,
    },
    /// After every canonical item (and after the newer-history boundary of a detached window):
    /// pending submissions, which the session has not ordered yet.
    Tail,
}

/// One local row: identity, placement and the semantic item it shows.
///
/// `item` is a renderer-neutral item (a system message for notices, a user message for pending
/// submissions) with a reserved identity that never names a session item. Its display label is
/// the notice title (`None` shows `System`) or the submission state (`sending`, `queued #2`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRow {
    /// Stable row identity.
    pub key: LocalKey,
    /// Placement relative to canonical items.
    pub placement: LocalPlacement,
    /// What the row shows.
    pub item: TranscriptViewItem,
}

impl LocalRow {
    /// A row keyed `key` at `placement` showing `item`, whose identity is replaced by the
    /// reserved local identity of `key` ([`local_item_id`]).
    #[must_use]
    pub fn new(key: LocalKey, placement: LocalPlacement, mut item: TranscriptViewItem) -> Self {
        item.id = local_item_id(&key);
        item.sequence = None;
        Self {
            key,
            placement,
            item,
        }
    }
}

/// Reserved item identity of a local row: the `local:` namespace, which no session item uses.
#[must_use]
pub fn local_item_id(key: &LocalKey) -> TranscriptViewItemId {
    TranscriptViewItemId::new(format!("local:{key}"))
}
