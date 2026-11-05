use bcode_harness_models::*;

fn observation(id: &str) -> HarnessObservation {
    HarnessObservation {
        observation_id: id.into(),
        session_id: "session".into(),
        generation: 7,
        native_correlations: vec!["turn".into()],
        completeness: HarnessCompleteness::Complete,
        kind: HarnessObservationKind::SubmissionAccepted {
            submission_id: "submission".into(),
            attempt_id: "attempt".into(),
            native_turn_id: "turn".into(),
        },
    }
}
#[test]
fn digest_has_explicit_domain_and_structural_framing() {
    use sha2::{Digest as _, Sha256};
    let actual = harness_observations_digest(&[]).unwrap();
    let mut hash = Sha256::new();
    hash.update(b"bcode.harness/ordered-observations/v1\0");
    hash.update(b"[]");
    let encode = |bytes: &[u8]| -> String {
        bytes
            .iter()
            .flat_map(|byte| {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                [
                    char::from(HEX[usize::from(byte >> 4)]),
                    char::from(HEX[usize::from(byte & 15)]),
                ]
            })
            .collect()
    };
    assert_eq!(actual, encode(&hash.finalize()));
    assert_ne!(actual, encode(&Sha256::digest(b"[]")));
}

#[test]
fn native_reference_join_preserves_namespace_and_rejects_display_correlations() {
    let mut value = observation("a");
    let turn = HarnessNativeReference {
        kind: HarnessNativeReferenceKind::Turn,
        native_id: "turn".into(),
        native_turn_id: None,
    };
    assert!(turn.matches_observation(&value));
    let process = HarnessNativeReference {
        kind: HarnessNativeReferenceKind::Process,
        ..turn.clone()
    };
    assert!(!process.matches_observation(&value));
    value.kind = HarnessObservationKind::Text {
        text: "turn".into(),
    };
    assert!(!turn.matches_observation(&value));
    value.kind = HarnessObservationKind::Execution {
        event: HarnessExecutionEvent::BackgroundWork {
            work_id: "own-id".into(),
            native_task_id: Some("native-process".into()),
            origin: None,
            parent_work_id: None,
            work_kind: HarnessWorkKind::Command,
            label: "command".into(),
            state: HarnessWorkState::Running,
            notification: HarnessNotificationState::Pending,
        },
    };
    value.native_correlations = vec!["item".into(), "turn".into()];
    let item = HarnessNativeReference {
        kind: HarnessNativeReferenceKind::Item,
        native_id: "item".into(),
        native_turn_id: Some("turn".into()),
    };
    assert!(item.matches_observation(&value));
    assert!(
        !HarnessNativeReference {
            native_turn_id: Some("wrong-turn".into()),
            ..item
        }
        .matches_observation(&value)
    );
    let process = HarnessNativeReference {
        kind: HarnessNativeReferenceKind::Process,
        native_id: "native-process".into(),
        native_turn_id: None,
    };
    assert!(process.matches_observation(&value));
    assert!(
        !HarnessNativeReference {
            kind: HarnessNativeReferenceKind::Agent,
            ..process
        }
        .matches_observation(&value)
    );
}

#[test]
fn globally_scoped_item_hydration_needs_no_fabricated_turn() {
    let reference = HarnessNativeReference {
        kind: HarnessNativeReferenceKind::Item,
        native_id: "global-tool-item".into(),
        native_turn_id: None,
    };
    reference.validate().unwrap();
    let mut value = observation("global-item-observation");
    value.native_correlations = vec!["global-tool-item".into()];
    value.kind = HarnessObservationKind::Execution {
        event: HarnessExecutionEvent::BackgroundWork {
            work_id: "adapter-work".into(),
            native_task_id: Some("native-task".into()),
            origin: None,
            parent_work_id: None,
            work_kind: HarnessWorkKind::Tool,
            label: "native tool".into(),
            state: HarnessWorkState::Running,
            notification: HarnessNotificationState::Unknown,
        },
    };
    assert!(reference.matches_observation(&value));
    let reply = HarnessHydrationReply {
        request_id: "lookup".into(),
        reference: reference.clone(),
        canonical_revision: 1,
        resolution: HarnessHydrationResolution::Found {
            observations: vec![value.clone()],
        },
    };
    reply.validate_for("session", 7).unwrap();
    assert!(reply.validate_for("other-session", 7).is_err());
    assert!(
        !HarnessNativeReference {
            native_turn_id: Some("absent-turn".into()),
            ..reference.clone()
        }
        .matches_observation(&value)
    );
    assert!(
        !HarnessNativeReference {
            native_id: "other-item".into(),
            ..reference.clone()
        }
        .matches_observation(&value)
    );
    value.kind = HarnessObservationKind::Text {
        text: "global-tool-item".into(),
    };
    assert!(!reference.matches_observation(&value));
}

#[test]
fn digest_preserves_order_and_payload_and_rejects_duplicates() {
    let a = observation("a");
    let b = observation("b");
    assert_ne!(
        harness_observations_digest(&[a.clone(), b.clone()]).unwrap(),
        harness_observations_digest(&[b, a.clone()]).unwrap()
    );
    assert!(harness_observations_digest(&[a.clone(), a]).is_err());
}
#[test]
fn legacy_page_deserializes_without_retention_extensions() {
    let page: HarnessObservationPage = serde_json::from_value(serde_json::json!({
        "observations":[],"next_after":null,"completeness":"partial",
        "status":{"connectivity":"unknown","activity":"unknown","attention":"none","continuation":"unknown","completeness":"partial"}
    })).unwrap();
    assert!(
        page.retention.is_none()
            && page.inventory_page.is_none()
            && page.hydration_requests.is_empty()
    );
    page.validate_retention().unwrap();
}
#[test]
fn hydration_is_bounded_target_scoped_and_revision_checked() {
    let mut reply = HarnessHydrationReply {
        request_id: "lookup".into(),
        reference: HarnessNativeReference {
            kind: HarnessNativeReferenceKind::Turn,
            native_id: "turn".into(),
            native_turn_id: None,
        },
        canonical_revision: 1,
        resolution: HarnessHydrationResolution::Found {
            observations: vec![observation("a")],
        },
    };
    reply.validate_for("session", 7).unwrap();
    assert!(reply.validate_for("other", 7).is_err());
    reply.canonical_revision = 0;
    assert!(reply.validate_for("session", 7).is_err());
    reply.canonical_revision = 1;
    reply.resolution = HarnessHydrationResolution::Found {
        observations: (0..33)
            .map(|n| observation(&format!("event-{n}")))
            .collect(),
    };
    assert!(reply.validate_for("session", 7).is_err());
}
#[test]
fn page_digest_and_duplicate_lookup_requests_are_checked() {
    let observations = vec![observation("a")];
    let mut page = HarnessObservationPage {
        retention: Some(HarnessRetentionPage {
            connection_id: "connection".into(),
            page_id: "page".into(),
            observations_digest: harness_observations_digest(&observations).unwrap(),
        }),
        hydration_requests: Vec::new(),
        inventory_page: None,
        observations,
        next_after: None,
        completeness: HarnessCompleteness::Partial,
        status: HarnessExecutionStatus {
            connectivity: HarnessConnectivity::Connected,
            activity: HarnessActivity::Unknown,
            attention: HarnessAttention::None,
            continuation: HarnessContinuation::Unknown,
            completeness: HarnessCompleteness::Partial,
        },
    };
    page.validate_retention().unwrap();
    page.observations[0].kind = HarnessObservationKind::Text {
        text: "different".into(),
    };
    assert!(page.validate_retention().is_err());
    page.retention = None;
    let request = HarnessHydrationRequest {
        request_id: "same".into(),
        reference: HarnessNativeReference {
            kind: HarnessNativeReferenceKind::Process,
            native_id: "42".into(),
            native_turn_id: None,
        },
    };
    page.hydration_requests = vec![request.clone(), request];
    assert!(page.validate_retention().is_err());
}
