//! Narrow final-publication contract for durable compaction synthesis.
use bcode_session_models::{SessionEvent, SessionEventKind};
use sha2::{Digest, Sha256};

#[path = "compaction_checkpoint_memory.rs"]
mod memory;
pub use memory::validate_memory_commit;

/// Maximum source identities accepted by one bounded publication.
pub const MAX_COMPACTION_COMMIT_SOURCE_EVENTS: usize = 4096;

/// Exact canonical event identity, computed by [`compaction_event_identity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionEventIdentity {
    /// Canonical event sequence.
    pub sequence: u64,
    /// SHA-256 of the event's `serde_json` serialization.
    pub sha256: String,
}

/// Compare-and-publish request; source is the ordered model-visible prefix excluding markers.
#[derive(Debug, Clone)]
pub struct CompactionCheckpointCommit {
    /// Exact operational checkpoint namespace.
    pub operation_key: String,
    /// Latest active checkpoint sequence observed by the host.
    pub expected_checkpoint_sequence: u64,
    /// Exact current prior marker, or absence before the first compaction.
    pub expected_prior_marker: Option<CompactionEventIdentity>,
    /// Exact bounded prefix through the new marker's boundary, excluding the prior marker.
    pub source: Vec<CompactionEventIdentity>,
    /// Existing local or provider compaction marker, never a partial checkpoint.
    pub marker: SessionEventKind,
}

/// Compute a shared exact-source identity without hashing presentation text.
///
/// # Errors
/// Returns a content-free error if canonical event serialization fails.
pub fn compaction_event_identity(event: &SessionEvent) -> Result<CompactionEventIdentity, String> {
    let event = crate::submission::context_event(event.clone());
    let bytes = serde_json::to_vec(&event)
        .map_err(|_| "cannot encode compaction source identity".to_owned())?;
    Ok(CompactionEventIdentity {
        sequence: event.sequence,
        sha256: hex::encode(Sha256::digest(bytes)),
    })
}
