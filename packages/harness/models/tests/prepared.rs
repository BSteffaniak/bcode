//! Prepared transport is not native-session confirmation or replay authority.
use bcode_harness_models::*;

fn prepared() -> HarnessPreparedSession {
    HarnessPreparedSession {
        open_id: "open".into(),
        session_id: "child".into(),
        instance_id: "instance".into(),
        generation: 7,
        native_version: "1.0.0".into(),
        requested_native_session_id: Some("requested".into()),
    }
}
fn confirmed() -> HarnessSessionBinding {
    HarnessSessionBinding {
        schema_version: 1,
        session_id: "child".into(),
        instance_id: "instance".into(),
        native_session_id: "requested".into(),
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
            executable: "/native/bin".into(),
            config_namespace: "/native/config".into(),
            auth: HarnessAuthBinding {
                provider_id: "provider".into(),
                profile: "profile".into(),
                credential_namespace: "namespace".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        },
        kind,
    }
}
fn open() -> HarnessOpenRequest {
    HarnessOpenRequest {
        open_id: "open".into(),
        session_id: "child".into(),
        generation: 7,
        working_directory: "/workspace".into(),
        resume: None,
        parent_assistance: None,
    }
}
fn status() -> HarnessExecutionStatus {
    HarnessExecutionStatus {
        connectivity: HarnessConnectivity::Connected,
        activity: HarnessActivity::Unknown,
        attention: HarnessAttention::None,
        continuation: HarnessContinuation::Unknown,
        completeness: HarnessCompleteness::Partial,
    }
}
fn reply(operation: HarnessOperation, kind: HarnessServiceResponseKind) -> HarnessServiceResponse {
    HarnessServiceResponse {
        schema_version: 1,
        request_id: "request".into(),
        operation,
        kind,
    }
}
#[test]
fn prepared_open_is_distinct_from_native_confirmation_and_cannot_resume() {
    let response = reply(
        HarnessOperation::Open,
        HarnessServiceResponseKind::Prepared(prepared()),
    );
    assert!(
        response
            .validate_for(&request(HarnessRequestKind::Open(open())))
            .is_ok()
    );
    let encoded = serde_json::to_value(&response).unwrap();
    assert_eq!(encoded["kind"]["kind"], "prepared");
    assert!(
        encoded["kind"]["response"]
            .get("native_session_id")
            .is_none()
    );
    assert!(
        serde_json::from_value::<HarnessSessionBinding>(encoded["kind"]["response"].clone())
            .is_err()
    );
    let mut resume = open();
    resume.resume = Some(confirmed());
    assert_eq!(
        response.validate_for(&request(HarnessRequestKind::Open(resume))),
        Err(HarnessContractError::InvalidShape)
    );
    let mut wrong_open = open();
    wrong_open.open_id = "different-open".into();
    assert_eq!(
        response.validate_for(&request(HarnessRequestKind::Open(wrong_open))),
        Err(HarnessContractError::WrongOwner)
    );
}
#[test]
fn first_submit_requires_no_fabricated_binding_and_rejects_preconfirmation_steering() {
    let submission = HarnessSubmission {
        submission_id: "submission".into(),
        attempt_id: "attempt".into(),
        session_id: "child".into(),
        generation: 7,
        text: "actual user input".into(),
        placement: HarnessPlacement::NextTurn,
    };
    let target = HarnessSessionTarget::Prepared(prepared());
    assert!(
        request(HarnessRequestKind::Submit {
            target: target.clone(),
            submission: submission.clone()
        })
        .validate()
        .is_ok()
    );
    let mut steering = submission.clone();
    steering.placement = HarnessPlacement::Steer {
        expected_native_turn_id: "turn".into(),
    };
    assert_eq!(
        request(HarnessRequestKind::Submit {
            target: target.clone(),
            submission: steering
        })
        .validate(),
        Err(HarnessContractError::InvalidShape)
    );
    let mut stale = submission;
    stale.generation = 6;
    assert_eq!(
        request(HarnessRequestKind::Submit {
            target,
            submission: stale
        })
        .validate(),
        Err(HarnessContractError::StaleGeneration)
    );
}
#[test]
fn prepared_observe_and_control_reject_foreign_instance_and_zero_generation() {
    for invalid in [
        HarnessPreparedSession {
            instance_id: "foreign".into(),
            ..prepared()
        },
        HarnessPreparedSession {
            generation: 0,
            ..prepared()
        },
    ] {
        let target = HarnessSessionTarget::Prepared(invalid);
        assert!(
            request(HarnessRequestKind::Observe(HarnessObserveRequest {
                retention: None,
                target: target.clone(),
                after: None,
                limit: 1,
                wait_millis: 0
            }))
            .validate()
            .is_err()
        );
        assert!(
            request(HarnessRequestKind::Interrupt(HarnessControlRequest {
                target: target.clone(),
                mutation_id: "interrupt".into()
            }))
            .validate()
            .is_err()
        );
        assert!(
            request(HarnessRequestKind::Close(HarnessControlRequest {
                target,
                mutation_id: "close".into()
            }))
            .validate()
            .is_err()
        );
    }
    let target = HarnessSessionTarget::Prepared(prepared());
    assert_eq!(
        target.validate_owner("foreign-child", 7),
        Err(HarnessContractError::WrongOwner)
    );
    assert_eq!(
        target.validate_owner("child", 6),
        Err(HarnessContractError::StaleGeneration)
    );
    assert!(
        request(HarnessRequestKind::Observe(HarnessObserveRequest {
            retention: None,
            target: target.clone(),
            after: None,
            limit: 1,
            wait_millis: 0
        }))
        .validate()
        .is_ok()
    );
    assert!(
        request(HarnessRequestKind::Interrupt(HarnessControlRequest {
            target: target.clone(),
            mutation_id: "interrupt".into()
        }))
        .validate()
        .is_ok()
    );
    assert!(
        request(HarnessRequestKind::Close(HarnessControlRequest {
            target,
            mutation_id: "close".into()
        }))
        .validate()
        .is_ok()
    );
}
#[test]
fn confirmation_is_fenced_to_exact_prepared_selection_and_native_evidence() {
    let target = HarnessSessionTarget::Prepared(prepared());
    assert!(target.validate_confirmation("open", &confirmed()).is_ok());
    assert_eq!(
        target.validate_confirmation("wrong-open", &confirmed()),
        Err(HarnessContractError::WrongOwner)
    );
    for wrong in [
        HarnessSessionBinding {
            native_session_id: "different".into(),
            ..confirmed()
        },
        HarnessSessionBinding {
            instance_id: "foreign".into(),
            ..confirmed()
        },
        HarnessSessionBinding {
            session_id: "foreign".into(),
            ..confirmed()
        },
        HarnessSessionBinding {
            native_version: "different".into(),
            ..confirmed()
        },
    ] {
        assert_eq!(
            target.validate_confirmation("open", &wrong),
            Err(HarnessContractError::WrongOwner)
        );
    }
    assert_eq!(
        target.validate_confirmation(
            "open",
            &HarnessSessionBinding {
                generation: 6,
                ..confirmed()
            }
        ),
        Err(HarnessContractError::StaleGeneration)
    );
    let observe = request(HarnessRequestKind::Observe(HarnessObserveRequest {
        retention: None,
        target,
        after: None,
        limit: 1,
        wait_millis: 0,
    }));
    let observation = HarnessObservation {
        observation_id: "binding-observed".into(),
        session_id: "child".into(),
        generation: 7,
        native_correlations: vec![],
        completeness: HarnessCompleteness::Complete,
        kind: HarnessObservationKind::BindingConfirmed {
            open_id: "open".into(),
            binding: confirmed(),
        },
    };
    let response = reply(
        HarnessOperation::Observe,
        HarnessServiceResponseKind::Observations(HarnessObservationPage {
            retention: None,
            hydration_requests: Vec::new(),
            inventory_page: None,
            observations: vec![observation],
            next_after: Some("binding-observed".into()),
            completeness: HarnessCompleteness::Complete,
            status: status(),
        }),
    );
    assert!(response.validate_for(&observe).is_ok());
    let mut invalid_response = response;
    if let HarnessServiceResponseKind::Observations(page) = &mut invalid_response.kind
        && let HarnessObservationKind::BindingConfirmed { binding, .. } =
            &mut page.observations[0].kind
    {
        binding.native_session_id = "unrequested-native".into();
    }
    assert_eq!(
        invalid_response.validate_for(&observe),
        Err(HarnessContractError::WrongOwner)
    );
    let unselected = HarnessSessionTarget::Prepared(HarnessPreparedSession {
        requested_native_session_id: None,
        ..prepared()
    });
    assert!(
        unselected
            .validate_confirmation("open", &confirmed())
            .is_ok()
    );
}
#[test]
fn prepared_response_cannot_address_another_child_or_generation() {
    let response = HarnessResponse {
        response_id: "answer".into(),
        request_id: "question".into(),
        session_id: "child".into(),
        generation: 7,
        value: HarnessResponseValue::Cancel,
    };
    let target = HarnessSessionTarget::Prepared(prepared());
    assert!(
        request(HarnessRequestKind::Respond {
            target: target.clone(),
            response: response.clone()
        })
        .validate()
        .is_ok()
    );
    let mut wrong = response.clone();
    wrong.session_id = "other-child".into();
    assert_eq!(
        request(HarnessRequestKind::Respond {
            target: target.clone(),
            response: wrong
        })
        .validate(),
        Err(HarnessContractError::WrongOwner)
    );
    let mut stale = response;
    stale.generation = 6;
    assert_eq!(
        request(HarnessRequestKind::Respond {
            target,
            response: stale
        })
        .validate(),
        Err(HarnessContractError::StaleGeneration)
    );
}
#[test]
fn process_loss_remains_uncertain_and_prepared_reconciliation_is_not_a_binding() {
    let request = request(HarnessRequestKind::Reconcile(HarnessReconcileRequest {
        session_id: "child".into(),
        generation: 7,
        open_id: Some("open".into()),
        submission_id: None,
        attempt_id: None,
        binding: None,
    }));
    assert!(
        reply(
            HarnessOperation::Reconcile,
            HarnessServiceResponseKind::Reconciled(HarnessReconciliation::Uncertain)
        )
        .validate_for(&request)
        .is_ok()
    );
    let response = reply(
        HarnessOperation::Reconcile,
        HarnessServiceResponseKind::Reconciled(HarnessReconciliation::KnownPrepared {
            prepared: prepared(),
            receipt: None,
            status: status(),
        }),
    );
    assert!(response.validate_for(&request).is_ok());
    let mut mismatched = request;
    if let HarnessRequestKind::Reconcile(value) = &mut mismatched.kind {
        value.open_id = Some("other-open".into());
    }
    assert_eq!(
        response.validate_for(&mismatched),
        Err(HarnessContractError::WrongOwner)
    );
}
