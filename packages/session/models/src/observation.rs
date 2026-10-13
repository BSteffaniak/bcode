//! Portable bounded session observation contracts.
//!
//! Delivery sequences belong to one observation, not canonical history or a durable resume token.
//! Payloads are supplied by the owning semantic view boundary, keeping this leaf independent of
//! renderers, transport, and session-view implementation crates.

use crate::{ProjectionWindowRequest, SessionId};
use serde::{Deserialize, Serialize};

use std::future::Future;

/// Domain-owned pull observation contract for generic Rust consumers.
///
/// Implementations reuse the owning semantic projector. Payload selection is associated rather
/// than a dependency on session-view, transport, or daemon implementation types. One handle owns
/// one delivery sequence; it does not confer canonical mutation authority or durable resume.
pub trait SessionObservationStream: Send {
    /// Existing portable semantic state transferred by this observation.
    type Snapshot: Send;

    /// Receive ordered state or one stable terminal outcome, then None forever.
    /// Cancelling this future must leave the handle usable without cancelling execution.
    fn recv(
        &mut self,
    ) -> impl Future<Output = Option<SessionObservationEnvelope<Self::Snapshot>>> + Send;

    /// Release observation resources without cancelling execution. Repeated closes are harmless
    /// and cannot replace an already-established terminal outcome.
    fn close(&mut self);
}

/// Authority-retaining application access to domain-owned session observation.
///
/// This is an operation contract, not a host/framework lifecycle. Implementations perform their
/// normal selected-location, namespace, and caller access checks before constructing the stream.
pub trait SessionObservationApplication {
    /// Observation implementation returned to the consumer without private construction inputs.
    type Observation: SessionObservationStream;

    /// Observe a latest bounded semantic session window without attaching as a client.
    ///
    /// # Errors
    /// Returns normalized failures for unsupported contracts/requests, unavailable sessions,
    /// unverifiable reads, or failed caller access checks. Failure does not cancel session work.
    fn observe_session(
        &self,
        request: SessionObservationRequest,
    ) -> impl Future<Output = Result<Self::Observation, SessionObservationError>> + Send;
}

/// Compatibility version for observation requests and delivery envelopes.
pub const SESSION_OBSERVATION_SCHEMA_VERSION: u16 = 1;

/// Read-only observation of the latest bounded session window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionObservationRequest {
    /// Contract version; unknown versions are rejected without subscribing.
    pub schema_version: u16,
    /// Canonical session to observe.
    pub session_id: SessionId,
    /// Existing semantic window query. Observation requires the latest transcript window.
    pub window: ProjectionWindowRequest,
    /// Maximum source updates examined during this observation, including initial history and
    /// buffered overlap with that history. This also bounds projector-owned bookkeeping that is
    /// not serialized in a snapshot. The snapshot byte cap alone is not a resident-memory cap.
    /// Reaching this cumulative bound ends observation explicitly with `LimitReached`: consumers
    /// must open a fresh observation for further state. Execution continues independently.
    pub max_updates: usize,
    /// Maximum encoded bytes in one semantic snapshot or source update. Initial source history
    /// must also fit this budget. Oversized inputs are not projected and snapshots are not delivered.
    pub max_snapshot_bytes: usize,
}

/// One ordered delivery in a single observation lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionObservationEnvelope<T> {
    /// Observation compatibility version.
    pub schema_version: u16,
    /// Session owning the semantic payload.
    pub session_id: SessionId,
    /// Zero-based contiguous delivery sequence, local to this observation only.
    pub sequence: u64,
    /// Latest durable source sequence represented, when available.
    pub source_sequence: Option<u64>,
    /// Semantic state transfer or stable observation terminal outcome.
    pub event: SessionObservationEvent<T>,
}

/// Semantic state transfer; never raw IPC or canonical event-log records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionObservationEvent<T> {
    /// Initial bounded window is always delivered before subsequent updates.
    Snapshot {
        /// True only for the first delivery.
        initial: bool,
        /// Existing canonical semantic projection, not an independently interpreted event log.
        snapshot: T,
    },
    /// Final delivery; subsequent receives return no value.
    Ended {
        /// Reason observation stopped. This is not an execution cancellation receipt.
        reason: SessionObservationEnd,
    },
}

/// Stable observation terminal reasons. Resync requires a fresh observation, not cursor replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionObservationEnd {
    /// Consumer explicitly stopped observing; session execution is unaffected.
    Closed,
    /// Both canonical source subscriptions ended.
    SourceClosed,
    /// Some source updates were lost; no subsequent semantic state is delivered.
    Gap {
        /// Source for which continuity was lost.
        source: SessionObservationSource,
        /// Broadcast-reported lost updates, or None for a sequence/integrity gap.
        skipped: Option<u64>,
    },
    /// Configured cumulative work, source-input, or semantic snapshot size bound was reached.
    /// Open a fresh bounded observation to resync; this is not an execution terminal outcome.
    LimitReached,
    /// Unsupported durable schema or inconsistent session identity encountered.
    IncompatibleSource,
}

/// Independent source continuity; durable and live sources do not share an ordering clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionObservationSource {
    /// Committed canonical session history.
    Durable,
    /// Ephemeral semantic updates, unavailable for durable replay.
    Live,
}

/// Normalized secret-safe observation admission failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum SessionObservationError {
    /// Observation schema version is unsupported.
    #[error("unsupported session observation schema")]
    UnsupportedSchema,
    /// Invalid bounds or a window other than the latest transcript window.
    #[error("unsupported session observation request")]
    InvalidRequest,
    /// Canonical session was not found.
    #[error("session not found")]
    NotFound,
    /// Bounded canonical read could not be verified; explicit maintenance may be necessary.
    #[error("session observation unavailable; inspect session storage status")]
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip_preserves_gap_and_correlation() {
        let envelope: SessionObservationEnvelope<String> = SessionObservationEnvelope {
            schema_version: SESSION_OBSERVATION_SCHEMA_VERSION,
            session_id: SessionId::new(),
            sequence: 7,
            source_sequence: Some(12),
            event: SessionObservationEvent::Ended {
                reason: SessionObservationEnd::Gap {
                    source: SessionObservationSource::Live,
                    skipped: Some(3),
                },
            },
        };
        let encoded = serde_json::to_vec(&envelope).unwrap();
        assert_eq!(
            serde_json::from_slice::<SessionObservationEnvelope<String>>(&encoded).unwrap(),
            envelope
        );
    }
}
