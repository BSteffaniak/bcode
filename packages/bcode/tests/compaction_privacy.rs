#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! External-consumer privacy checks at serialized public frontend boundaries.
//! Canonical session injection and the actual host request assembly are exercised by
//! the server companion tests; this target needs no private host implementation API.

use bcode::{
    AgentEvent, FrontendEventCursor, FrontendSessionSnapshot, MessageRole, ModelContentBlock,
    ModelMessage, SessionId,
};
use bcode_session_models::CompactionSynthesisCheckpoint;

const CANARY: &str = "PRIVATE-COMPACTION-CHECKPOINT-8066e1e9";
const KEY: &str = "PRIVATE-COMPACTION-NAMESPACE-c27d1004";

fn checkpoint() -> CompactionSynthesisCheckpoint {
    CompactionSynthesisCheckpoint {
        version: 1,
        operation_key: KEY.into(),
        host_payload: Some(serde_json::json!({"accepted_carry": CANARY}).to_string()),
    }
}

fn assert_private(value: &impl serde::Serialize) {
    let json = serde_json::to_string(value).unwrap();
    assert!(!json.contains(CANARY));
    assert!(!json.contains(KEY));
}

#[test]
fn operational_provider_metadata_does_not_enter_public_event_or_snapshot() {
    let id = SessionId::new();
    let mut snapshot = FrontendSessionSnapshot::new(id, &[]);
    let mut cursor = FrontendEventCursor::new(id, "turn", 0);
    let started = cursor.project(&AgentEvent::TurnStarted).unwrap();
    snapshot.apply_event(&started).unwrap();
    let metadata = AgentEvent::ProviderMetadata {
        key: bcode_session_models::COMPACTION_SYNTHESIS_CHECKPOINT_EVENT.into(),
        value: serde_json::to_string(&checkpoint()).unwrap(),
    };
    assert!(cursor.project(&metadata).is_none());
    let visible = cursor
        .project(&AgentEvent::TextDelta("Public answer".into()))
        .unwrap();
    assert_eq!(visible.sequence, started.sequence + 1);
    snapshot.apply_event(&visible).unwrap();
    assert_private(&visible);
    assert_private(&snapshot);
    assert!(
        serde_json::to_string(&snapshot)
            .unwrap()
            .contains("Public answer")
    );
}

#[test]
fn opaque_compaction_state_is_not_serialized_in_frontend_transcript() {
    let messages = vec![ModelMessage {
        role: MessageRole::Assistant,
        content: vec![
            ModelContentBlock::Text {
                text: "Portable task fact".into(),
            },
            ModelContentBlock::ProviderExtension {
                value: serde_json::to_value(checkpoint()).unwrap(),
            },
        ],
    }];
    // A positive private-side assertion prevents an empty fixture from passing.
    assert!(serde_json::to_string(&messages).unwrap().contains(CANARY));
    let snapshot = FrontendSessionSnapshot::new(SessionId::new(), &messages);
    assert_private(&snapshot);
    let json = serde_json::to_string(&snapshot).unwrap();
    assert!(json.contains("Portable task fact"));
    let roundtrip: FrontendSessionSnapshot = serde_json::from_str(&json).unwrap();
    assert_private(&roundtrip);
}

#[test]
fn unsupported_checkpoint_envelope_fails_closed_without_echoing_payload() {
    for version in [0, 2, u32::MAX] {
        let mut value = checkpoint();
        value.version = version;
        let error = value.into_event_kind(0).unwrap_err();
        assert_private(&error);
    }
    let valid_kind = checkpoint().into_event_kind(0).unwrap();
    assert_eq!(
        CompactionSynthesisCheckpoint::from_event_kind(&valid_kind).unwrap(),
        Some(checkpoint()),
    );
    let mut future_kind = valid_kind;
    let bcode_session_models::SessionEventKind::TraceEvent { trace } = &mut future_kind else {
        panic!("checkpoint did not produce operational trace")
    };
    let bcode_session_models::SessionTracePayload::ProviderEvent { detail, .. } =
        &mut trace.payload
    else {
        panic!("checkpoint did not produce provider evidence")
    };
    let mut future: serde_json::Value = serde_json::from_str(detail.as_ref().unwrap()).unwrap();
    future["version"] = serde_json::json!(2);
    *detail = Some(serde_json::to_string(&future).unwrap());
    let error = CompactionSynthesisCheckpoint::from_event_kind(&future_kind).unwrap_err();
    assert_private(&error);
}
