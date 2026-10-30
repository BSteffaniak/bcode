//! Explicit durable interaction codec and retention behavior, not repository source assertions.
use super::*;
use crate::persisted::{
    EventDecodeBudget, EventDecodeError, PersistedSessionEventError, decode_session_event,
    decode_session_event_with_budget, encode_session_event,
};
use bcode_session_models::{ObservationRetentionLimits, SessionObservationSourceLimits};

const fn limits() -> SessionObservationSourceLimits {
    SessionObservationSourceLimits {
        max_scalar_capacity_bytes: 1024 * 1024,
        max_collection_entries: 16384,
        max_value_nodes: 65536,
        max_depth: 64,
        max_queued_events: 1,
        max_in_flight_events: 0,
    }
}

const fn budget() -> EventDecodeBudget {
    EventDecodeBudget::new(
        1024 * 1024,
        ObservationRetentionLimits {
            scalar_capacity_bytes: 1024 * 1024,
            collection_entries: 16384,
            value_nodes: 65536,
            depth: 64,
            copies: 1,
        },
    )
}

#[test]
fn canonical_codec_round_trips_each_transition_and_rejects_unsupported_envelope() {
    let initial = operation();
    let first = advance(None, &initial, 1).unwrap();
    let completion = complete(&first);
    let mut publication = initial.clone();
    publication.operation_id = "publish".into();
    publication.expected_revision = Some(2);
    publication.transition = DurableInteractionTransition::PublishContinuation {
        publication_id: "continuation".into(),
    };
    for operation in [initial, completion, publication] {
        let source = event(SessionId::new(), 8, operation);
        let encoded = encode_session_event(&source).unwrap();
        assert_eq!(decode_session_event(&encoded).unwrap(), source);
        assert_eq!(
            decode_session_event_with_budget(&encoded, &mut budget()).unwrap(),
            source
        );
        // Persisted JSON is product data; mutate its envelope, never implementation text.
        let mut future: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        future["kind"]["durable_interaction_committed"]["operation"]["version"] =
            serde_json::json!(DURABLE_INTERACTION_VERSION + 1);
        let future = serde_json::to_string(&future).unwrap();
        assert!(matches!(
            decode_session_event(&future),
            Err(PersistedSessionEventError::InvalidDurableInteraction)
        ));
        assert!(matches!(
            decode_session_event_with_budget(&future, &mut budget()),
            Err(EventDecodeError::Canonical(
                PersistedSessionEventError::InvalidDurableInteraction
            ))
        ));
    }
}

#[test]
fn codec_rejects_invalid_digest_and_oversized_payload_without_echoing_payload() {
    let mut op = operation();
    op.request_digest = "A".repeat(64);
    let source = event(SessionId::new(), 1, op);
    assert!(encode_session_event(&source).is_err());
    let mut op = operation();
    if let DurableInteractionTransition::Checkpoint { payload } = &mut op.transition {
        payload.json = "x".repeat(bcode_session_models::MAX_DURABLE_INTERACTION_PAYLOAD_BYTES + 1);
    }
    let source = event(SessionId::new(), 1, op);
    assert!(encode_session_event(&source).is_err());
    // Bypass the validated encoder to exercise damaged on-disk DTO admission.
    let damaged = serde_json::to_string(&source).unwrap();
    assert!(matches!(
        decode_session_event(&damaged),
        Err(PersistedSessionEventError::InvalidDurableInteraction)
    ));
}

#[test]
fn opaque_payload_capacity_and_nested_completion_are_structurally_bounded() {
    let mut op = operation();
    let DurableInteractionTransition::Checkpoint { payload } = &mut op.transition else {
        unreachable!()
    };
    let mut reserved = String::with_capacity(128 * 1024);
    reserved.push_str("{}");
    payload.json = reserved;
    let source = event(SessionId::new(), 1, op);
    let usage = bcode_session_models::inspect_durable(&source, &limits()).unwrap();
    assert!(usage.scalar_capacity_bytes >= 128 * 1024);
    let mut constrained = limits();
    constrained.max_scalar_capacity_bytes = 64 * 1024;
    assert!(bcode_session_models::inspect_durable(&source, &constrained).is_err());
    let first = advance(None, &operation(), 1).unwrap();
    let mut completion = complete(&first);
    let DurableInteractionTransition::Complete {
        exchange_resolution,
        ..
    } = &mut completion.transition
    else {
        unreachable!()
    };
    exchange_resolution.resolution = bcode_session_models::ToolExchangeResolution::Responded {
        payload: serde_json::json!({"nested":[{"secret":"retained-answer"}]}),
    };
    let source = event(SessionId::new(), 2, completion);
    let usage = bcode_session_models::inspect_durable(&source, &limits()).unwrap();
    let mut constrained = limits();
    constrained.max_depth = usage.depth - 1;
    assert!(bcode_session_models::inspect_durable(&source, &constrained).is_err());
    let encoded = encode_session_event(&source).unwrap();
    let mut tiny = EventDecodeBudget::new(16, limits().retention_limits().unwrap());
    assert!(matches!(
        decode_session_event_with_budget(&encoded, &mut tiny),
        Err(EventDecodeError::ResourceLimit)
    ));
    // Public debug output exposes contract identities/byte counts, not opaque answers/results.
    assert!(!format!("{source:?}").contains("retained-answer"));
}
