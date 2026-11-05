//! Submission acceptance is typed native correlation, not binding or settlement.
use bcode_harness_models::{
    HarnessCompleteness, HarnessContractError, HarnessObservation, HarnessObservationKind,
    MAX_HARNESS_ID_BYTES,
};

fn acceptance() -> HarnessObservation {
    HarnessObservation {
        observation_id: "observation".into(),
        session_id: "child".into(),
        generation: 7,
        native_correlations: vec!["native-request-distinct-from-turn".into()],
        completeness: HarnessCompleteness::Complete,
        kind: HarnessObservationKind::SubmissionAccepted {
            submission_id: "submission".into(),
            attempt_id: "attempt".into(),
            native_turn_id: "native-turn".into(),
        },
    }
}

#[test]
fn acceptance_roundtrip_preserves_separate_ids_and_enclosing_owner() {
    let observation = acceptance();
    assert!(observation.validate().is_ok());
    let encoded = serde_json::to_value(&observation).unwrap();
    assert_eq!(encoded["kind"]["kind"], "submission_accepted");
    assert_eq!(encoded["kind"]["submission_id"], "submission");
    assert_eq!(encoded["kind"]["attempt_id"], "attempt");
    assert_eq!(encoded["kind"]["native_turn_id"], "native-turn");
    assert_eq!(encoded["session_id"], "child");
    assert_eq!(encoded["generation"], 7);
    assert_eq!(
        serde_json::from_value::<HarnessObservation>(encoded).unwrap(),
        observation
    );
}

#[test]
fn acceptance_rejects_missing_and_unbounded_identities() {
    for field in ["submission_id", "attempt_id", "native_turn_id"] {
        for invalid in [
            String::new(),
            "bad\nidentity".into(),
            "x".repeat(MAX_HARNESS_ID_BYTES + 1),
        ] {
            let mut encoded = serde_json::to_value(acceptance()).unwrap();
            encoded["kind"][field] = serde_json::Value::String(invalid);
            let observation: HarnessObservation = serde_json::from_value(encoded).unwrap();
            assert_eq!(
                observation.validate(),
                Err(HarnessContractError::InvalidIdentity)
            );
        }
        let mut encoded = serde_json::to_value(acceptance()).unwrap();
        encoded["kind"].as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<HarnessObservation>(encoded).is_err());
    }
    let mut observation = acceptance();
    observation.generation = 0;
    assert_eq!(
        observation.validate(),
        Err(HarnessContractError::InvalidShape)
    );
}

#[test]
fn result_and_settlement_do_not_deserialize_as_acceptance_without_exact_pair() {
    for kind in [
        HarnessObservationKind::Result {
            text: "submission accepted".into(),
        },
        HarnessObservationKind::Settlement {
            native_turn_id: "native-turn".into(),
        },
    ] {
        let mut observation = acceptance();
        observation.kind = kind;
        let mut encoded = serde_json::to_value(observation).unwrap();
        encoded["kind"]["kind"] = serde_json::Value::String("submission_accepted".into());
        assert!(serde_json::from_value::<HarnessObservation>(encoded).is_err());
    }
}
