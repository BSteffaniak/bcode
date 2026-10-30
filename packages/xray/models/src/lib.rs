#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Portable, transient X-ray diagnostic contracts, independent of any renderer or transport.
//!
//! These are not canonical session events or a durable replay protocol. Callers reject
//! unsupported envelope versions and preserve unknown domain event names and data.

use serde::{Deserialize, Serialize};

/// Independently evolving observation and payload-access protocol version.
pub const XRAY_SCHEMA_VERSION: u16 = 1;

/// Origin identities supplied by the instrumented domain, never inferred from payloads.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct XrayCorrelation {
    /// Canonical origin session identity, supplied by the host.
    pub session_id: String,
    /// Originating application/runtime turn, when available.
    pub turn_id: Option<String>,
    /// Originating model or service request, when available.
    pub request_id: Option<String>,
    /// Originating tool invocation, when available.
    pub tool_call_id: Option<String>,
    /// Originating workflow run, when available.
    pub run_id: Option<String>,
    /// Originating workflow activation, when available.
    pub activation_id: Option<String>,
    /// Originating execution attempt, when available.
    pub attempt_id: Option<String>,
}

/// A domain-owned observation, not an authorization fact or public frontend event.
///
/// Sensitive content must exclude credentials, auth headers and configuration secrets.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct XrayEvent {
    /// Fixed instrumenting domain name; `xray` is reserved for engine controls.
    pub domain: String,
    /// Fixed domain-owned operation/stage name; unknown names remain opaque.
    pub name: String,
    /// Origin metadata; never sufficient to authorize capture or access.
    pub correlation: XrayCorrelation,
    /// Bounded domain payload, independent of frontend event contracts.
    pub data: serde_json::Value,
    /// Restricts delivery to observations explicitly authorized for deep capture.
    pub sensitive: bool,
}

/// Ordered delivery within one observation. Sequence numbers start at one (`xray.ready`).
///
/// Ordinary events and `xray.finalized` must have the next expected sequence.
/// A gap is the only exception: its envelope sequence equals `first_sequence`, which
/// must equal the consumer's next expected sequence, and it covers the inclusive range
/// through `last_sequence`. The next envelope is therefore `last_sequence + 1`, not
/// the gap envelope's sequence plus one. The gap itself allocates no extra sequence.
/// Finalization follows all queued events and tail gaps at its own next sequence;
/// its payload's `final_sequence` equals its envelope sequence. It ends this observation
/// only, not execution. No reconnect, durable replay or persistence is promised.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct XrayEnvelope {
    /// Protocol version; unsupported versions must be rejected.
    pub schema_version: u16,
    /// Exact live observation identity, stable through finalization.
    pub observation_id: String,
    /// Delivery coordinate, with inclusive-range semantics for gap records.
    pub sequence: u64,
    /// Wall-clock timestamp for correlation, not elapsed-duration arithmetic.
    pub timestamp_unix_ms: u64,
    /// Domain observation or engine-owned control record.
    pub event: XrayEvent,
}

/// Attach after application authorization.
///
/// Session and sensitive-scope approval must precede calling the engine.
/// Deep capture is explicit and lasts only for this observation's active lifetime.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct XrayObserveRequest {
    /// Requested independently versioned protocol.
    pub schema_version: u16,
    /// Session authorized by the application before attachment.
    pub session_id: String,
    /// Explicit request for sensitive diagnostic capture during this observation.
    pub deep_capture: bool,
}

/// Effective bounds and guarantees advertised in the initial `xray.ready` event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct XrayCapabilities {
    /// Negotiated protocol version.
    pub schema_version: u16,
    /// Whether this observation accepts explicitly sensitive diagnostics.
    pub deep_capture: bool,
    /// Maximum buffered producer events.
    pub queue_capacity: usize,
    /// Maximum serialized producer-event bytes.
    pub max_event_bytes: usize,
    /// Maximum bytes retained in one payload capture.
    pub max_payload_bytes: usize,
    /// Maximum aggregate payload bytes retained by this observation.
    pub max_retained_payload_bytes: usize,
    /// Maximum payload count retained by this observation.
    pub max_retained_payloads: usize,
    /// Maximum returned bytes per authorized payload read.
    pub max_payload_range_bytes: usize,
    /// Always false in v1: observations are live, transient and not resumable.
    pub durable_replay: bool,
}

/// Missing delivery within this observation.
///
/// An inclusive sequence range is represented by
/// one `xray.gap` envelope at `first_sequence`; consumers advance through `last_sequence`.
/// Multiple dropped events may thus be represented by a single delivered envelope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct XrayGap {
    /// First missing sequence, also the gap envelope's sequence.
    pub first_sequence: u64,
    /// Last missing sequence, inclusive; never less than `first_sequence`.
    pub last_sequence: u64,
    /// Stable engine-owned explanation, never raw producer error text.
    pub reason: String,
}

/// An engine delivery cutoff, not a claim of task, transport or recording completion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct XrayFinalization {
    /// Exact observation scope of this cutoff.
    pub observation_id: String,
    /// Sequence of the final envelope, emitted after queued events and tail gaps.
    pub final_sequence: u64,
    /// Total queue-overflow omissions, already represented by gap ranges in delivery.
    pub dropped_events: u64,
    /// Hub-wide contention uncertainty during the observation.
    ///
    /// Includes activity hints that returned false before a producer could build or emit an event.
    /// This counts failed operations, not an exact number of missing envelopes;
    /// no sequence can be allocated without the lock. A nonzero value prohibits a
    /// completeness claim. Scope is conservative: contention from another session
    /// may mark this observation incomplete because affected sessions cannot be
    /// determined without blocking the producer.
    pub hub_contention_drops: u64,
    /// Number of payloads evicted from this observation's bounded transient retention.
    pub evicted_payloads: u64,
}

/// Reference to explicit sensitive bytes retained only while an observation exists.
///
/// Content is unredacted diagnostic data; a digest does not imply secret safety.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct XrayPayloadRef {
    /// Payload-access protocol version.
    pub schema_version: u16,
    /// Opaque transient identity, never a filesystem path.
    pub payload_id: String,
    /// Authorized originating session.
    pub session_id: String,
    /// Validated diagnostic media type, not trusted rendering instructions.
    pub content_type: String,
    /// Complete retained length; captures are never silently truncated.
    pub byte_len: u64,
    /// SHA-256 of all retained bytes, as lowercase hex.
    pub sha256: String,
}

/// Bounded byte access.
///
/// The application must authorize the exact observation/session
/// and sensitive scope before forwarding this request; discovery confers no authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct XrayReadPayloadRequest {
    /// Requested payload-access protocol version.
    pub schema_version: u16,
    /// Exact live observation that owns the retained bytes.
    pub observation_id: String,
    /// Exact authorized originating session.
    pub session_id: String,
    /// Opaque payload identity within the observation.
    pub payload_id: String,
    /// Byte offset, at most retained EOF.
    pub offset: u64,
    /// Positive requested byte count, bounded by advertised capabilities.
    pub length: u32,
}

/// A retained range, not reconstructed content. EOF permits an empty range.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct XrayPayloadRange {
    /// Response protocol version, to be checked before interpretation.
    pub schema_version: u16,
    /// Exact observation that owns these bytes.
    pub observation_id: String,
    /// Complete retained identity and digest, unchanged by range selection.
    pub payload: XrayPayloadRef,
    /// Starting byte offset of the returned range.
    pub offset: u64,
    /// Exact retained bytes in the bounded requested range.
    pub bytes: Vec<u8>,
    /// Whether this range reaches retained EOF.
    pub eof: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_request_fields_fail_closed() {
        let request = serde_json::json!({
            "schema_version": 1, "session_id": "s", "deep_capture": false,
            "enable_persistence": true
        });
        assert!(serde_json::from_value::<XrayObserveRequest>(request).is_err());
    }

    #[test]
    fn unknown_domain_events_round_trip_without_reinterpretation() {
        let event = XrayEnvelope {
            schema_version: XRAY_SCHEMA_VERSION,
            observation_id: "o".into(),
            sequence: 2,
            timestamp_unix_ms: 0,
            event: XrayEvent {
                domain: "future-domain".into(),
                name: "future-event".into(),
                correlation: XrayCorrelation::default(),
                sensitive: false,
                data: serde_json::json!({"future": [1, 2]}),
            },
        };
        let bytes = serde_json::to_vec(&event).expect("serialize envelope");
        assert_eq!(
            serde_json::from_slice::<XrayEnvelope>(&bytes).unwrap(),
            event
        );
    }
}
