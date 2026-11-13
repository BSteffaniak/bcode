//! Behavioral protocol tests; deliberately not executed during the initial writer wave.
use bcode_harness_models::*;

fn binding() -> HarnessSessionBinding {
    HarnessSessionBinding {
        schema_version: 1,
        session_id: "session".into(),
        instance_id: "instance".into(),
        native_session_id: "native".into(),
        native_version: "1.0.0".into(),
        generation: 7,
    }
}
fn request(kind: HarnessRequestKind) -> HarnessRequest {
    HarnessRequest {
        schema_version: 1,
        request_id: "request".into(),
        instance: HarnessInstance {
            instance_id: "instance".into(),
            plugin_id: "plugin".into(),
            executable: "/bin/native".into(),
            config_namespace: "/config/native".into(),
            auth: HarnessAuthBinding {
                provider_id: "provider".into(),
                profile: "profile".into(),
                credential_namespace: "credentials".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        },
        kind,
    }
}
fn status() -> HarnessExecutionStatus {
    HarnessExecutionStatus {
        connectivity: HarnessConnectivity::Connected,
        activity: HarnessActivity::Running,
        attention: HarnessAttention::None,
        continuation: HarnessContinuation::Unknown,
        completeness: HarnessCompleteness::Partial,
    }
}
fn observation(name: &str) -> HarnessObservation {
    HarnessObservation {
        observation_id: name.into(),
        session_id: "session".into(),
        generation: 7,
        native_correlations: vec![],
        completeness: HarnessCompleteness::Complete,
        kind: HarnessObservationKind::Result {
            text: "result while background work remains".into(),
        },
    }
}
#[test]
fn unknown_versions_and_future_variants_fail_closed() {
    let mut request = request(HarnessRequestKind::Inspect);
    assert!(request.validate().is_ok());
    request.schema_version = 2;
    assert_eq!(
        request.validate(),
        Err(HarnessContractError::UnsupportedVersion)
    );
    assert!(serde_json::from_str::<HarnessDeliveryState>("\"future-state\"").is_err());
    assert!(serde_json::from_str::<HarnessPlacement>(r#"{"kind":"steer"}"#).is_err());
}
#[test]
fn retained_submission_and_steering_are_generation_fenced() {
    let mut submission = HarnessSubmission {
        submission_id: "submission".into(),
        attempt_id: "attempt".into(),
        session_id: "session".into(),
        generation: 6,
        text: "steer".into(),
        placement: HarnessPlacement::Steer {
            expected_native_turn_id: "turn".into(),
        },
    };
    assert_eq!(
        request(HarnessRequestKind::Submit {
            target: HarnessSessionTarget::Confirmed(binding()),
            submission: submission.clone()
        })
        .validate(),
        Err(HarnessContractError::StaleGeneration)
    );
    submission.generation = 7;
    assert!(
        request(HarnessRequestKind::Submit {
            target: HarnessSessionTarget::Confirmed(binding()),
            submission: submission.clone()
        })
        .validate()
        .is_ok()
    );
    submission.placement = HarnessPlacement::Steer {
        expected_native_turn_id: String::new(),
    };
    assert_eq!(
        submission.validate(),
        Err(HarnessContractError::InvalidIdentity)
    );
}
#[test]
fn native_result_does_not_imply_settlement_and_page_is_bounded() {
    let request = request(HarnessRequestKind::Observe(HarnessObserveRequest {
        retention: None,
        target: HarnessSessionTarget::Confirmed(binding()),
        after: None,
        limit: 1,
        wait_millis: 0,
    }));
    let mut response = HarnessServiceResponse {
        schema_version: 1,
        request_id: "request".into(),
        operation: HarnessOperation::Observe,
        kind: HarnessServiceResponseKind::Observations(HarnessObservationPage {
            live: Vec::new(),
            retention: None,
            hydration_requests: Vec::new(),
            inventory_page: None,
            observations: vec![observation("one")],
            next_after: Some("cursor".into()),
            completeness: HarnessCompleteness::Partial,
            status: status(),
        }),
    };
    assert!(response.validate_for(&request).is_ok());
    if let HarnessServiceResponseKind::Observations(page) = &mut response.kind {
        assert_eq!(page.status.activity, HarnessActivity::Running);
        page.observations.push(observation("two"));
    }
    assert_eq!(
        response.validate_for(&request),
        Err(HarnessContractError::BoundsExceeded)
    );
}
#[test]
fn stale_observation_and_duplicate_identity_are_rejected() {
    let request = request(HarnessRequestKind::Observe(HarnessObserveRequest {
        retention: None,
        target: HarnessSessionTarget::Confirmed(binding()),
        after: None,
        limit: 2,
        wait_millis: 0,
    }));
    let mut response = HarnessServiceResponse {
        schema_version: 1,
        request_id: "request".into(),
        operation: HarnessOperation::Observe,
        kind: HarnessServiceResponseKind::Observations(HarnessObservationPage {
            live: Vec::new(),
            retention: None,
            hydration_requests: Vec::new(),
            inventory_page: None,
            observations: vec![observation("one"), observation("one")],
            next_after: None,
            completeness: HarnessCompleteness::Complete,
            status: status(),
        }),
    };
    assert_eq!(
        response.validate_for(&request),
        Err(HarnessContractError::InvalidShape)
    );
    if let HarnessServiceResponseKind::Observations(page) = &mut response.kind {
        page.observations.pop();
        page.observations[0].generation = 6;
    }
    assert_eq!(
        response.validate_for(&request),
        Err(HarnessContractError::StaleGeneration)
    );
}
#[test]
fn uncertain_reconciliation_and_control_ack_are_not_settlement() {
    let request = request(HarnessRequestKind::Reconcile(HarnessReconcileRequest {
        session_id: "session".into(),
        generation: 7,
        open_id: None,
        submission_id: Some("submission".into()),
        attempt_id: Some("attempt".into()),
        binding: Some(binding()),
    }));
    let response = HarnessServiceResponse {
        schema_version: 1,
        request_id: "request".into(),
        operation: HarnessOperation::Reconcile,
        kind: HarnessServiceResponseKind::Reconciled(HarnessReconciliation::Uncertain),
    };
    assert!(response.validate_for(&request).is_ok());
    assert_eq!(
        serde_json::to_value(HarnessReconciliation::Uncertain).unwrap()["state"],
        "uncertain"
    );
    assert_ne!(
        HarnessDeliveryState::NativeAccepted,
        HarnessDeliveryState::Settled
    );
    assert_ne!(HarnessActivity::Idle, HarnessActivity::Settled);
}
#[test]
fn interaction_kind_choice_and_generation_are_checked() {
    let mut interaction = HarnessInteraction {
        input_preview: None,
        tool_name: None,
        request_id: "question".into(),
        session_id: "session".into(),
        generation: 7,
        kind: HarnessInteractionKind::Question,
        prompt: "Choose".into(),
        scope: "current request only".into(),
        choices: vec![HarnessChoice {
            choice_id: "yes".into(),
            label: "Yes".into(),
            description: None,
        }],
    };
    let mut response = HarnessResponse {
        response_id: "response".into(),
        request_id: "question".into(),
        session_id: "session".into(),
        generation: 7,
        value: HarnessResponseValue::Approval { approved: true },
    };
    assert_eq!(
        response.validate_for(&interaction),
        Err(HarnessContractError::InvalidShape)
    );
    response.value = HarnessResponseValue::Answer {
        choice_id: Some("absent".into()),
        text: String::new(),
    };
    assert_eq!(
        response.validate_for(&interaction),
        Err(HarnessContractError::InvalidShape)
    );
    response.value = HarnessResponseValue::Answer {
        choice_id: Some("yes".into()),
        text: String::new(),
    };
    assert!(response.validate_for(&interaction).is_ok());
    response.generation = 6;
    assert_eq!(
        response.validate_for(&interaction),
        Err(HarnessContractError::StaleGeneration)
    );
    interaction.kind = HarnessInteractionKind::Assistance;
    response.generation = 7;
    response.value = HarnessResponseValue::Assistance {
        text: "bounded parent assistance".into(),
    };
    assert!(response.validate_for(&interaction).is_ok());
}
#[test]
fn vendor_detail_and_native_correlations_are_bounded() {
    let mut value = observation("one");
    value.kind = HarnessObservationKind::Vendor {
        namespace: "vendor".into(),
        detail: serde_json::Value::String("x".repeat(MAX_HARNESS_VENDOR_BYTES)),
    };
    assert_eq!(value.validate(), Err(HarnessContractError::BoundsExceeded));
    value.kind = HarnessObservationKind::Tool {
        input_json: None,
        native_tool_id: "tool".into(),
        name: "read".into(),
        status: "observed".into(),
    };
    assert!(value.validate().is_ok());
    value.native_correlations = vec!["correlation".into(); MAX_HARNESS_ITEMS + 1];
    assert_eq!(value.validate(), Err(HarnessContractError::BoundsExceeded));
}
#[test]
fn auth_inputs_use_existing_redacted_ephemeral_transport_contract() {
    use bcode_provider_auth_models::managed::AuthSensitiveInput;
    let request = request(HarnessRequestKind::AuthContinue(HarnessAuthRequest {
        flow_id: "flow".into(),
        method_id: "native-login".into(),
        input: Some(AuthSensitiveInput::new("prompt".into(), "SECRET-CALLBACK".into()).unwrap()),
    }));
    assert!(request.validate().is_ok());
    assert!(!format!("{request:?}").contains("SECRET-CALLBACK"));
    assert!(request.kind.operation().is_sensitive());
}
#[test]
fn instance_and_native_binding_cannot_be_substituted() {
    let mut foreign = binding();
    foreign.instance_id = "other".into();
    assert_eq!(
        request(HarnessRequestKind::Observe(HarnessObserveRequest {
            retention: None,
            target: HarnessSessionTarget::Confirmed(foreign),
            after: None,
            limit: 1,
            wait_millis: 0
        }))
        .validate(),
        Err(HarnessContractError::WrongOwner)
    );
    let mut wrong_session = binding();
    wrong_session.session_id = "other".into();
    assert_eq!(
        wrong_session.validate_owner("session", 7),
        Err(HarnessContractError::WrongOwner)
    );
}

fn assistance_open() -> HarnessOpenRequest {
    HarnessOpenRequest {
        open_id: "open".into(),
        session_id: "child".into(),
        generation: 7,
        working_directory: "/workspace".into(),
        resume: None,
        parent_assistance: Some(HarnessAssistanceGrant {
            admission_id: "admission".into(),
            child_session_id: "child".into(),
            child_generation: 7,
            parent_session_id: "parent".into(),
            parent_generation: 3,
            scope: "Ask the admitted parent for implementation guidance".into(),
        }),
    }
}

#[test]
fn absent_parent_assistance_decodes_without_enabling_a_grant() {
    let open: HarnessOpenRequest = serde_json::from_value(serde_json::json!({
        "open_id": "open", "session_id": "child", "generation": 7,
        "working_directory": "/workspace", "resume": null
    }))
    .unwrap();
    assert!(open.parent_assistance.is_none());
    assert!(request(HarnessRequestKind::Open(open)).validate().is_ok());
}

#[test]
fn malformed_parent_assistance_fails_closed() {
    let open = assistance_open();
    let grant = open.parent_assistance.as_ref().unwrap();
    // Shape validity is not an admission or authority assertion.
    assert!(
        request(HarnessRequestKind::Open(open.clone()))
            .validate()
            .is_ok()
    );
    for field in ["admission_id", "child_session_id", "parent_session_id"] {
        for invalid in [
            String::new(),
            "bad\nidentity".into(),
            "x".repeat(MAX_HARNESS_ID_BYTES + 1),
        ] {
            let mut encoded = serde_json::to_value(grant).unwrap();
            encoded[field] = serde_json::Value::String(invalid);
            let malformed: HarnessAssistanceGrant = serde_json::from_value(encoded).unwrap();
            let mut invalid_open = open.clone();
            invalid_open.parent_assistance = Some(malformed);
            assert_eq!(
                request(HarnessRequestKind::Open(invalid_open)).validate(),
                Err(HarnessContractError::InvalidIdentity)
            );
        }
    }
    for field in ["child_generation", "parent_generation"] {
        let mut encoded = serde_json::to_value(grant).unwrap();
        encoded[field] = serde_json::json!(0);
        let mut invalid_open = open.clone();
        invalid_open.parent_assistance = Some(serde_json::from_value(encoded).unwrap());
        assert_eq!(
            request(HarnessRequestKind::Open(invalid_open)).validate(),
            Err(HarnessContractError::InvalidShape)
        );
    }
    for scope in [
        String::new(),
        "  \t".into(),
        "x".repeat(MAX_HARNESS_TEXT_BYTES + 1),
    ] {
        let mut invalid_open = open.clone();
        invalid_open.parent_assistance.as_mut().unwrap().scope = scope.clone();
        let expected = if scope.len() > MAX_HARNESS_TEXT_BYTES {
            HarnessContractError::BoundsExceeded
        } else {
            HarnessContractError::InvalidShape
        };
        assert_eq!(
            request(HarnessRequestKind::Open(invalid_open)).validate(),
            Err(expected)
        );
    }
    let mut same_session = open.clone();
    same_session
        .parent_assistance
        .as_mut()
        .unwrap()
        .parent_session_id = "child".into();
    assert_eq!(
        request(HarnessRequestKind::Open(same_session)).validate(),
        Err(HarnessContractError::InvalidShape)
    );
    let mut missing_field = serde_json::to_value(grant).unwrap();
    missing_field
        .as_object_mut()
        .unwrap()
        .remove("admission_id");
    assert!(serde_json::from_value::<HarnessAssistanceGrant>(missing_field).is_err());
}

#[test]
fn parent_assistance_must_match_exact_child_open_identity_and_generation() {
    let mut open = assistance_open();
    open.parent_assistance.as_mut().unwrap().child_session_id = "other-child".into();
    assert_eq!(
        request(HarnessRequestKind::Open(open)).validate(),
        Err(HarnessContractError::WrongOwner)
    );
    let mut open = assistance_open();
    open.parent_assistance.as_mut().unwrap().child_generation = 6;
    assert_eq!(
        request(HarnessRequestKind::Open(open)).validate(),
        Err(HarnessContractError::StaleGeneration)
    );
}
#[test]
fn page_transferring_nothing_may_omit_retention_after_an_acknowledged_predecessor() {
    let digest = harness_observations_digest(&[observation("one")]).unwrap();
    let request = request(HarnessRequestKind::Observe(HarnessObserveRequest {
        retention: Some(HarnessRetentionContext {
            connection_id: "connection".into(),
            acknowledgment: Some(HarnessCanonicalAck {
                page_id: "connection:1".into(),
                canonical_revision: 9,
                observations_digest: digest,
            }),
            hydration: Vec::new(),
        }),
        target: HarnessSessionTarget::Confirmed(binding()),
        after: Some("one".into()),
        limit: 4,
        wait_millis: 0,
    }));
    let page = |observations: Vec<HarnessObservation>, retention| HarnessServiceResponse {
        schema_version: 1,
        request_id: "request".into(),
        operation: HarnessOperation::Observe,
        kind: HarnessServiceResponseKind::Observations(HarnessObservationPage {
            live: Vec::new(),
            retention,
            hydration_requests: Vec::new(),
            inventory_page: None,
            next_after: observations.last().map(|item| item.observation_id.clone()),
            observations,
            completeness: HarnessCompleteness::Complete,
            status: status(),
        }),
    };
    // An idle or live-only window transfers nothing and needs no acknowledgment.
    assert!(page(Vec::new(), None).validate_for(&request).is_ok());
    // Evidence after an acknowledged page must remain retained for exact post-commit ack.
    assert_eq!(
        page(vec![observation("two")], None).validate_for(&request),
        Err(HarnessContractError::InvalidShape)
    );
    let retained = |connection: &str, observations: &[HarnessObservation]| HarnessRetentionPage {
        connection_id: connection.into(),
        page_id: format!("{connection}:2"),
        observations_digest: harness_observations_digest(observations).unwrap(),
    };
    let two = vec![observation("two")];
    assert!(
        page(two.clone(), Some(retained("connection", &two)))
            .validate_for(&request)
            .is_ok()
    );
    assert_eq!(
        page(Vec::new(), Some(retained("other", &[]))).validate_for(&request),
        Err(HarnessContractError::WrongOwner)
    );
}
