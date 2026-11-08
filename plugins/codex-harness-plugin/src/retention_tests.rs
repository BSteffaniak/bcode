use super::*;
use bcode_harness_models::{
    HarnessCompleteness, HarnessHydrationResolution, HarnessObservationKind,
};
fn event() -> HarnessObservation {
    HarnessObservation {
        observation_id: "event".into(),
        session_id: "session".into(),
        generation: 1,
        native_correlations: Vec::new(),
        completeness: HarnessCompleteness::Complete,
        kind: HarnessObservationKind::Text {
            segment_id: None,
            text: "recorded".into(),
        },
    }
}
#[test]
fn exact_postcommit_ack_is_idempotent_and_wrong_connection_or_digest_cannot_evict() {
    let mut retention = Retention::default();
    let page = retention.deliver(&[event()]).unwrap();
    let mut context = HarnessRetentionContext {
        connection_id: page.connection_id.clone(),
        acknowledgment: Some(HarnessCanonicalAck {
            page_id: page.page_id.clone(),
            canonical_revision: 1,
            observations_digest: page.observations_digest.clone(),
        }),
        hydration: Vec::new(),
    };
    context.connection_id = "wrong".into();
    assert!(retention.apply(&context, "session", 1).is_err());
    context.connection_id = page.connection_id;
    context.acknowledgment.as_mut().unwrap().observations_digest = "0".repeat(64);
    assert!(retention.apply(&context, "session", 1).is_err());
    context.acknowledgment.as_mut().unwrap().observations_digest = page.observations_digest;
    assert_eq!(
        retention.apply(&context, "session", 1).unwrap().0,
        vec![event()]
    );
    assert_eq!(
        retention.apply(&context, "session", 1).unwrap().0,
        [] as [bcode_harness_models::HarnessObservation; 0]
    );
    assert!(retention.ready());
}
#[test]
fn unsolicited_hydration_is_rejected_and_matching_read_is_correlated() {
    let mut retention = Retention::default();
    let page = retention.deliver(&[]).unwrap();
    let reference = HarnessNativeReference {
        kind: bcode_harness_models::HarnessNativeReferenceKind::Turn,
        native_id: "turn".into(),
        native_turn_id: None,
    };
    let mut context = HarnessRetentionContext {
        connection_id: page.connection_id,
        acknowledgment: None,
        hydration: vec![HarnessHydrationReply {
            request_id: "unsolicited".into(),
            reference: reference.clone(),
            canonical_revision: 1,
            resolution: HarnessHydrationResolution::Absent,
        }],
    };
    assert!(retention.apply(&context, "session", 1).is_err());
    retention.request(reference).unwrap();
    context.hydration[0].request_id = retention.requests()[0].request_id.clone();
    retention.apply(&context, "session", 1).unwrap();
    assert_eq!(
        retention.requests(),
        [] as [bcode_harness_models::HarnessHydrationRequest; 0]
    );
}
