//! Offline canonical-store behavior; no providers, plugins or native subprocesses.
use bcode_harness_models::*;
use bcode_session::{SessionManager, lease::SessionLeaseOwnerContext};
use bcode_session_models::{ExternalHarnessClaim, ExternalHarnessOwner, SessionId};
#[tokio::test]
async fn foreign_daemon_reads_full_retained_submit_without_delivery_authority() {
    Box::pin(check_foreign_retained_submit(false)).await;
    Box::pin(check_foreign_retained_submit(true)).await;
}

async fn check_foreign_retained_submit(completed: bool) {
    let root = tempfile::tempdir().unwrap();
    let sessions = manager(root.path());
    let session = sessions
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let owner = owner(session.id);
    sessions
        .configure_external_harness(&owner, instance(), capabilities())
        .await
        .unwrap();
    let opening = open(&owner);
    sessions
        .claim_external_harness_request(&owner, &opening)
        .await
        .unwrap();
    sessions
        .complete_external_harness_request(
            &owner,
            &opening,
            &HarnessServiceResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: opening.request_id.clone(),
                operation: HarnessOperation::Open,
                kind: HarnessServiceResponseKind::Prepared(prepared(&owner)),
            },
        )
        .await
        .unwrap();
    let request = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "retained-submit".into(),
        instance: instance(),
        kind: HarnessRequestKind::Submit {
            target: HarnessSessionTarget::Prepared(prepared(&owner)),
            submission: HarnessSubmission {
                submission_id: "retained-input".into(),
                attempt_id: "retained-attempt".into(),
                session_id: session.id.to_string(),
                generation: 1,
                text: "exact retained input".into(),
                placement: HarnessPlacement::NextTurn,
            },
        },
    };
    assert!(
        sessions
            .read_external_harness_retained_request(session.id, &request)
            .await
            .unwrap()
            .is_none()
    );
    sessions
        .claim_external_harness_request(&owner, &request)
        .await
        .unwrap();
    if completed {
        sessions
            .complete_external_harness_request(
                &owner,
                &request,
                &HarnessServiceResponse {
                    schema_version: HARNESS_SCHEMA_VERSION,
                    request_id: request.request_id.clone(),
                    operation: HarnessOperation::Submit,
                    kind: HarnessServiceResponseKind::Receipt(HarnessReceipt {
                        submission_id: "retained-input".into(),
                        attempt_id: "retained-attempt".into(),
                        session_id: session.id.to_string(),
                        generation: 1,
                        state: HarnessDeliveryState::NativeAccepted,
                        native_request_id: Some("native-request".into()),
                    }),
                },
            )
            .await
            .unwrap();
    }
    sessions
        .release_idle_session_resources(session.id)
        .await
        .unwrap();
    let observer = SessionManager::persistent_with_metrics_and_lease_owner(
        root.path(),
        bcode_metrics::MetricsRegistry::disabled(),
        SessionLeaseOwnerContext {
            build_fingerprint: Some("new-artifact".into()),
            daemon_instance_id: Some("new-daemon".into()),
            ..SessionLeaseOwnerContext::default()
        },
    )
    .unwrap();
    let result = observer
        .read_external_harness_retained_request(session.id, &request)
        .await
        .unwrap()
        .unwrap();
    if completed {
        let ExternalHarnessClaim::Recorded(response) = result else {
            panic!("retained response required")
        };
        assert!(
            matches!(response.kind, bcode_session_models::ExternalHarnessResponseKind::Receipt(ref receipt)
            if receipt.native_request_id.as_deref() == Some("native-request"))
        );
    } else {
        assert_eq!(result, ExternalHarnessClaim::Uncertain);
    }
    // A retained read must not create execution authority.
    assert!(
        bcode_session::lease::active_session_owners(root.path(), session.id)
            .unwrap()
            .is_empty()
    );
    assert!(
        observer
            .claim_external_harness_request(&owner, &request)
            .await
            .is_err()
    );
    let mut changed = request;
    if let HarnessRequestKind::Submit { submission, .. } = &mut changed.kind {
        submission.text.push_str(" changed");
    }
    assert!(
        observer
            .read_external_harness_retained_request(session.id, &changed)
            .await
            .is_err()
    );
    changed.request_id = "different-request".into();
    assert!(
        observer
            .read_external_harness_retained_request(session.id, &changed)
            .await
            .is_err()
    );
    assert!(
        observer
            .read_external_harness_retained_request(SessionId::new(), &changed)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn typed_submission_acceptance_is_exact_indexed_and_offline_readable() {
    Box::pin(check_typed_submission_acceptance()).await;
}

async fn check_typed_submission_acceptance() {
    let root = tempfile::tempdir().unwrap();
    let sessions = manager(root.path());
    let session = sessions
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let owner = owner(session.id);
    sessions
        .configure_external_harness(&owner, instance(), capabilities())
        .await
        .unwrap();
    let opening = open(&owner);
    sessions
        .claim_external_harness_request(&owner, &opening)
        .await
        .unwrap();
    sessions
        .complete_external_harness_request(
            &owner,
            &opening,
            &HarnessServiceResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: opening.request_id.clone(),
                operation: HarnessOperation::Open,
                kind: HarnessServiceResponseKind::Prepared(prepared(&owner)),
            },
        )
        .await
        .unwrap();
    let submit = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "acceptance-submit".into(),
        instance: instance(),
        kind: HarnessRequestKind::Submit {
            target: HarnessSessionTarget::Prepared(prepared(&owner)),
            submission: HarnessSubmission {
                submission_id: "accepted-input".into(),
                attempt_id: "accepted-attempt".into(),
                session_id: session.id.to_string(),
                generation: 1,
                text: "accepted input".into(),
                placement: HarnessPlacement::NextTurn,
            },
        },
    };
    let mut accepted = page(&owner);
    accepted.observations[0].kind = HarnessObservationKind::SubmissionAccepted {
        submission_id: "accepted-input".into(),
        attempt_id: "accepted-attempt".into(),
        native_turn_id: "actual-native-turn".into(),
    };
    assert!(
        sessions
            .commit_external_harness_page(&owner, None, &accepted)
            .await
            .is_err()
    );
    sessions
        .claim_external_harness_request(&owner, &submit)
        .await
        .unwrap();
    // Native request correlation is intentionally different and cannot substitute for acceptance.
    sessions
        .complete_external_harness_request(
            &owner,
            &submit,
            &HarnessServiceResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: submit.request_id.clone(),
                operation: HarnessOperation::Submit,
                kind: HarnessServiceResponseKind::Receipt(HarnessReceipt {
                    submission_id: "accepted-input".into(),
                    attempt_id: "accepted-attempt".into(),
                    session_id: session.id.to_string(),
                    generation: 1,
                    state: HarnessDeliveryState::NativeAccepted,
                    native_request_id: Some("not-a-native-turn".into()),
                }),
            },
        )
        .await
        .unwrap();
    assert!(
        sessions
            .session_external_harness_submission_acceptance(
                session.id,
                "accepted-input",
                "accepted-attempt"
            )
            .await
            .unwrap()
            .is_none()
    );
    let mut wrong = accepted.clone();
    if let HarnessObservationKind::SubmissionAccepted { attempt_id, .. } =
        &mut wrong.observations[0].kind
    {
        *attempt_id = "wrong-attempt".into();
    }
    assert!(
        sessions
            .commit_external_harness_page(&owner, None, &wrong)
            .await
            .is_err()
    );
    sessions
        .commit_external_harness_page(&owner, None, &accepted)
        .await
        .unwrap();
    sessions
        .commit_external_harness_page(&owner, None, &accepted)
        .await
        .unwrap();
    assert_eq!(
        sessions
            .session_external_harness_submission_acceptance(
                session.id,
                "accepted-input",
                "accepted-attempt"
            )
            .await
            .unwrap(),
        Some(accepted.observations[0].clone())
    );
    assert!(
        sessions
            .session_external_harness_submission_acceptance(
                session.id,
                "accepted-input",
                "other-attempt"
            )
            .await
            .unwrap()
            .is_none()
    );
    let mut conflict = accepted.clone();
    conflict.next_after = Some("cursor-2".into());
    conflict.observations[0].observation_id = "conflicting-acceptance".into();
    if let HarnessObservationKind::SubmissionAccepted { native_turn_id, .. } =
        &mut conflict.observations[0].kind
    {
        *native_turn_id = "different-turn".into();
    }
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("native-cursor"), &conflict)
            .await
            .is_err()
    );
    sessions
        .release_idle_session_resources(session.id)
        .await
        .unwrap();
    let observer = SessionManager::persistent_with_metrics_and_lease_owner(
        root.path(),
        bcode_metrics::MetricsRegistry::disabled(),
        SessionLeaseOwnerContext {
            build_fingerprint: Some("offline-artifact".into()),
            daemon_instance_id: Some("offline-daemon".into()),
            ..SessionLeaseOwnerContext::default()
        },
    )
    .unwrap();
    assert_eq!(
        observer
            .session_external_harness_submission_acceptance(
                session.id,
                "accepted-input",
                "accepted-attempt"
            )
            .await
            .unwrap(),
        Some(accepted.observations[0].clone())
    );
    assert!(
        bcode_session::lease::active_session_owners(root.path(), session.id)
            .unwrap()
            .is_empty()
    );
}

fn instance() -> HarnessInstance {
    HarnessInstance {
        instance_id: "fixture".into(),
        plugin_id: "fixture".into(),
        executable: "/not-invoked".into(),
        config_namespace: "fixture".into(),
        auth: HarnessAuthBinding {
            provider_id: "fixture".into(),
            profile: "native".into(),
            credential_namespace: "fixture".into(),
            custody: HarnessAuthCustody::NativeManaged,
        },
    }
}
fn capabilities() -> HarnessCapabilities {
    let c = HarnessCapability {
        support: HarnessSupportLevel::Supported,
        constraints: vec![],
    };
    HarnessCapabilities {
        resume: c.clone(),
        steering: c.clone(),
        approval: c.clone(),
        questions: c.clone(),
        assistance: c.clone(),
        interrupt: c.clone(),
        authoritative_settlement: c.clone(),
        background_continuation: c.clone(),
        authentication: c,
    }
}
fn owner(session_id: SessionId) -> ExternalHarnessOwner {
    ExternalHarnessOwner {
        session_id,
        generation: 1,
        artifact_id: "artifact".into(),
        daemon_id: "daemon".into(),
    }
}
fn manager(root: &std::path::Path) -> SessionManager {
    SessionManager::persistent_with_metrics_and_lease_owner(
        root,
        bcode_metrics::MetricsRegistry::disabled(),
        SessionLeaseOwnerContext {
            build_fingerprint: Some("artifact".into()),
            daemon_instance_id: Some("daemon".into()),
            ..SessionLeaseOwnerContext::default()
        },
    )
    .unwrap()
}
fn open(owner: &ExternalHarnessOwner) -> HarnessRequest {
    HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "open-request".into(),
        instance: instance(),
        kind: HarnessRequestKind::Open(HarnessOpenRequest {
            open_id: "open".into(),
            session_id: owner.session_id.to_string(),
            generation: 1,
            working_directory: "/tmp".into(),
            resume: None,
            parent_assistance: None,
        }),
    }
}
fn prepared(owner: &ExternalHarnessOwner) -> HarnessPreparedSession {
    HarnessPreparedSession {
        open_id: "open".into(),
        session_id: owner.session_id.to_string(),
        instance_id: "fixture".into(),
        generation: 1,
        native_version: "fixture-v1".into(),
        requested_native_session_id: None,
    }
}
fn page(owner: &ExternalHarnessOwner) -> HarnessObservationPage {
    HarnessObservationPage {
        observations: vec![HarnessObservation {
            observation_id: "observation".into(),
            session_id: owner.session_id.to_string(),
            generation: 1,
            native_correlations: vec!["turn".into()],
            completeness: HarnessCompleteness::Complete,
            kind: HarnessObservationKind::Text {
                text: "retained offline".into(),
            },
        }],
        next_after: Some("native-cursor".into()),
        completeness: HarnessCompleteness::Complete,
        status: HarnessExecutionStatus {
            connectivity: HarnessConnectivity::Connected,
            activity: HarnessActivity::Running,
            attention: HarnessAttention::None,
            continuation: HarnessContinuation::Available,
            completeness: HarnessCompleteness::Complete,
        },
    }
}
#[tokio::test]
async fn claim_and_completion_are_exact_retry_fenced_and_uncertain_without_ack() {
    let root = tempfile::tempdir().unwrap();
    let sessions = manager(root.path());
    let session = sessions
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let owner = owner(session.id);
    sessions
        .configure_external_harness(&owner, instance(), capabilities())
        .await
        .unwrap();
    let guard = sessions
        .acquire_external_harness_ownership(&owner)
        .await
        .unwrap();
    let request = open(&owner);
    assert_eq!(
        sessions
            .claim_external_harness_request(&owner, &request)
            .await
            .unwrap(),
        ExternalHarnessClaim::New
    );
    assert_eq!(
        sessions
            .claim_external_harness_request(&owner, &request)
            .await
            .unwrap(),
        ExternalHarnessClaim::Uncertain
    );
    // Losing the caller's acknowledgement must not turn the next invocation into New.
    drop(guard);
    sessions
        .release_idle_session_resources(session.id)
        .await
        .unwrap();
    let restarted = manager(root.path());
    assert_eq!(
        restarted
            .claim_external_harness_request(&owner, &request)
            .await
            .unwrap(),
        ExternalHarnessClaim::Uncertain
    );
    restarted
        .release_idle_session_resources(session.id)
        .await
        .unwrap();
    let guard = sessions
        .acquire_external_harness_ownership(&owner)
        .await
        .unwrap();
    let mut conflicting = open(&owner);
    conflicting.instance.config_namespace = "other".into();
    assert!(
        sessions
            .claim_external_harness_request(&owner, &conflicting)
            .await
            .is_err()
    );
    let response = HarnessServiceResponse {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: request.request_id.clone(),
        operation: HarnessOperation::Open,
        kind: HarnessServiceResponseKind::Prepared(prepared(&owner)),
    };
    sessions
        .complete_external_harness_request(&owner, &request, &response)
        .await
        .unwrap();
    sessions
        .complete_external_harness_request(&owner, &request, &response)
        .await
        .unwrap();
    assert!(matches!(
        sessions
            .claim_external_harness_request(&owner, &request)
            .await
            .unwrap(),
        ExternalHarnessClaim::Recorded(_)
    ));
    let state = sessions.external_harness_state(&owner).await.unwrap();
    assert!(state.binding.is_none());
    assert!(matches!(
        state.target,
        Some(HarnessSessionTarget::Prepared(_))
    ));
    let mut foreign = owner.clone();
    foreign.daemon_id = "foreign".into();
    assert!(
        sessions
            .claim_external_harness_request(&foreign, &request)
            .await
            .is_err()
    );
    let mut stale = owner.clone();
    stale.generation = 2;
    assert!(sessions.external_harness_state(&stale).await.is_err());
    drop(guard);
}
#[tokio::test]
async fn page_cursor_and_observations_commit_atomically_and_survive_without_plugin() {
    let root = tempfile::tempdir().unwrap();
    let sessions = manager(root.path());
    let session = sessions
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let owner = owner(session.id);
    sessions
        .configure_external_harness(&owner, instance(), capabilities())
        .await
        .unwrap();
    let guard = sessions
        .acquire_external_harness_ownership(&owner)
        .await
        .unwrap();
    let request = open(&owner);
    sessions
        .claim_external_harness_request(&owner, &request)
        .await
        .unwrap();
    sessions
        .complete_external_harness_request(
            &owner,
            &request,
            &HarnessServiceResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: request.request_id.clone(),
                operation: HarnessOperation::Open,
                kind: HarnessServiceResponseKind::Prepared(prepared(&owner)),
            },
        )
        .await
        .unwrap();
    let native = page(&owner);
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("skipped"), &native)
            .await
            .is_err()
    );
    sessions
        .commit_external_harness_page(&owner, None, &native)
        .await
        .unwrap();
    sessions
        .commit_external_harness_page(&owner, None, &native)
        .await
        .unwrap();
    let history = sessions
        .external_harness_history(session.id, None, 1)
        .await
        .unwrap();
    assert_eq!(history.observations, native.observations);
    let empty = sessions
        .external_harness_history(session.id, history.next_after.clone(), 1)
        .await
        .unwrap();
    assert!(empty.observations.is_empty());
    let mut conflict = native.clone();
    conflict.observations[0].kind = HarnessObservationKind::Text {
        text: "different".into(),
    };
    assert!(
        sessions
            .commit_external_harness_page(&owner, None, &conflict)
            .await
            .is_err()
    );
    let mut repeated = native.clone();
    repeated.next_after = Some("native-cursor-2".into());
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("native-cursor"), &repeated)
            .await
            .is_err()
    );
    let mut confirmation = page(&owner);
    confirmation.next_after = Some("confirmed-cursor".into());
    confirmation.observations[0].observation_id = "confirmed-binding".into();
    let binding = HarnessSessionBinding {
        schema_version: HARNESS_SCHEMA_VERSION,
        session_id: session.id.to_string(),
        instance_id: "fixture".into(),
        native_session_id: "native-confirmed".into(),
        native_version: "fixture-v1".into(),
        generation: 1,
    };
    confirmation.observations[0].kind = HarnessObservationKind::BindingConfirmed {
        open_id: "open".into(),
        binding: binding.clone(),
    };
    sessions
        .commit_external_harness_page(&owner, Some("native-cursor"), &confirmation)
        .await
        .unwrap();
    assert_eq!(
        sessions
            .external_harness_state(&owner)
            .await
            .unwrap()
            .binding,
        Some(binding)
    );
    let binding_event = sessions
        .session_external_backend_binding(session.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        bcode_session_models::ExternalBackendEnvelope::from_event(&binding_event)
            .unwrap()
            .is_some()
    );
    drop(guard);
    sessions
        .release_idle_session_resources(session.id)
        .await
        .unwrap();
    let reopened = manager(root.path());
    assert_eq!(
        reopened
            .external_harness_history(session.id, None, 128)
            .await
            .unwrap()
            .observations,
        native.observations
    );
}
#[tokio::test]
async fn prepared_first_submit_is_fenced_until_matching_native_confirmation() {
    let root = tempfile::tempdir().unwrap();
    let sessions = manager(root.path());
    let session = sessions
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let owner = owner(session.id);
    sessions
        .configure_external_harness(&owner, instance(), capabilities())
        .await
        .unwrap();
    let request = open(&owner);
    sessions
        .claim_external_harness_request(&owner, &request)
        .await
        .unwrap();
    let target = HarnessSessionTarget::Prepared(prepared(&owner));
    let submit =
        |request_id: &str, submission_id: &str, target: HarnessSessionTarget| HarnessRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request_id.into(),
            instance: instance(),
            kind: HarnessRequestKind::Submit {
                target,
                submission: HarnessSubmission {
                    submission_id: submission_id.into(),
                    attempt_id: "attempt".into(),
                    session_id: owner.session_id.to_string(),
                    generation: 1,
                    text: "real user input".into(),
                    placement: HarnessPlacement::NextTurn,
                },
            },
        };
    let first = submit("submit-1", "input-1", target.clone());
    // Live preparation is not canonical routing until its response is committed.
    assert!(
        sessions
            .claim_external_harness_request(&owner, &first)
            .await
            .is_err()
    );
    sessions
        .complete_external_harness_request(
            &owner,
            &request,
            &HarnessServiceResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: request.request_id.clone(),
                operation: HarnessOperation::Open,
                kind: HarnessServiceResponseKind::Prepared(prepared(&owner)),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        sessions
            .claim_external_harness_request(&owner, &first)
            .await
            .unwrap(),
        ExternalHarnessClaim::New
    );
    assert_eq!(
        sessions
            .claim_external_harness_request(&owner, &first)
            .await
            .unwrap(),
        ExternalHarnessClaim::Uncertain
    );
    assert!(
        sessions
            .claim_external_harness_request(&owner, &submit("submit-2", "input-2", target.clone()))
            .await
            .is_err()
    );
    let binding = HarnessSessionBinding {
        schema_version: HARNESS_SCHEMA_VERSION,
        session_id: session.id.to_string(),
        instance_id: "fixture".into(),
        native_session_id: "native-confirmed".into(),
        native_version: "fixture-v1".into(),
        generation: 1,
    };
    let mut confirmation = page(&owner);
    confirmation.observations[0].kind = HarnessObservationKind::BindingConfirmed {
        open_id: "wrong-open".into(),
        binding: binding.clone(),
    };
    assert!(
        sessions
            .commit_external_harness_page(&owner, None, &confirmation)
            .await
            .is_err()
    );
    assert!(
        sessions
            .external_harness_state(&owner)
            .await
            .unwrap()
            .binding
            .is_none()
    );
    confirmation.observations[0].kind = HarnessObservationKind::BindingConfirmed {
        open_id: "open".into(),
        binding: binding.clone(),
    };
    sessions
        .commit_external_harness_page(&owner, None, &confirmation)
        .await
        .unwrap();
    assert_eq!(
        sessions
            .external_harness_state(&owner)
            .await
            .unwrap()
            .target,
        Some(HarnessSessionTarget::Confirmed(binding.clone()))
    );
    assert!(
        sessions
            .claim_external_harness_request(&owner, &submit("submit-3", "input-3", target))
            .await
            .is_err()
    );
    assert_eq!(
        sessions
            .claim_external_harness_request(
                &owner,
                &submit(
                    "submit-4",
                    "input-4",
                    HarnessSessionTarget::Confirmed(binding)
                )
            )
            .await
            .unwrap(),
        ExternalHarnessClaim::New
    );
}

#[tokio::test]
async fn retained_top_level_prepare_preserves_requested_identity_and_exact_open() {
    let root = tempfile::tempdir().unwrap();
    let sessions = manager(root.path());
    let owner = owner(SessionId::new());
    let mut request = open(&owner);
    if let HarnessRequestKind::Open(open) = &mut request.kind {
        open.working_directory = root.path().to_string_lossy().into_owned();
    }
    let first = sessions
        .prepare_external_harness_open(&owner, &request, capabilities(), None)
        .await
        .unwrap();
    assert_eq!(first.id, owner.session_id);
    assert!(first.parent_id.is_none());
    let second = sessions
        .prepare_external_harness_open(&owner, &request, capabilities(), None)
        .await
        .unwrap();
    assert_eq!(second.id, first.id);
    let selected = sessions.external_harness_state(&owner).await.unwrap();
    assert!(selected.binding.is_none());
    assert!(selected.prepared_open.is_some());
    let mut changed = open(&owner);
    if let HarnessRequestKind::Open(v) = &mut changed.kind {
        v.open_id = "different".into();
        v.working_directory = root.path().to_string_lossy().into_owned();
    }
    assert!(
        sessions
            .prepare_external_harness_open(&owner, &changed, capabilities(), None)
            .await
            .is_err()
    );
    assert_eq!(
        sessions
            .claim_external_harness_request(&owner, &request)
            .await
            .unwrap(),
        ExternalHarnessClaim::New
    );
    sessions
        .mark_external_harness_uncertain(&owner, &request)
        .await
        .unwrap();
    assert!(
        matches!(sessions.claim_external_harness_request(&owner,&request).await.unwrap(),ExternalHarnessClaim::Recorded(response)
        if matches!(response.kind,bcode_session_models::ExternalHarnessResponseKind::Failure(HarnessFailure::Uncertain)))
    );
}

#[tokio::test]
async fn retained_child_prepare_uses_normal_parentage() {
    let root = tempfile::tempdir().unwrap();
    let sessions = manager(root.path());
    let parent = sessions
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let owner = owner(SessionId::new());
    let mut request = open(&owner);
    if let HarnessRequestKind::Open(v) = &mut request.kind {
        v.working_directory = root.path().to_string_lossy().into_owned();
    }
    let child = sessions
        .prepare_external_harness_open(&owner, &request, capabilities(), Some(parent.id))
        .await
        .unwrap();
    assert_eq!(child.id, owner.session_id);
    assert_eq!(child.parent_id, Some(parent.id));
    assert!(
        sessions
            .prepare_external_harness_open(&owner, &request, capabilities(), None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn offline_snapshot_preserves_prepared_and_confirmed_without_owner_acquisition() {
    use bcode_session::lease::active_session_owners;
    let root = tempfile::tempdir().unwrap();
    let sessions = manager(root.path());
    assert!(
        sessions
            .session_external_harness_state(SessionId::new())
            .await
            .unwrap()
            .is_none()
    );
    let session = sessions
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let owner = owner(session.id);
    assert!(
        sessions
            .session_external_harness_state(session.id)
            .await
            .unwrap()
            .is_none()
    );
    sessions
        .configure_external_harness(&owner, instance(), capabilities())
        .await
        .unwrap();
    let request = open(&owner);
    sessions
        .claim_external_harness_request(&owner, &request)
        .await
        .unwrap();
    sessions
        .complete_external_harness_request(
            &owner,
            &request,
            &HarnessServiceResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: request.request_id.clone(),
                operation: HarnessOperation::Open,
                kind: HarnessServiceResponseKind::Prepared(prepared(&owner)),
            },
        )
        .await
        .unwrap();
    let live = sessions
        .session_external_harness_state(session.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        live.target,
        Some(HarnessSessionTarget::Prepared(prepared(&owner)))
    );
    assert!(live.binding.is_none());
    sessions
        .release_idle_session_resources(session.id)
        .await
        .unwrap();
    assert!(
        active_session_owners(root.path(), session.id)
            .unwrap()
            .is_empty()
    );
    // This observer has a different daemon/artifact identity and no native/plugin runtime.
    let observer = SessionManager::persistent_with_metrics_and_lease_owner(
        root.path(),
        bcode_metrics::MetricsRegistry::disabled(),
        SessionLeaseOwnerContext {
            build_fingerprint: Some("observer-artifact".into()),
            daemon_instance_id: Some("observer-daemon".into()),
            ..SessionLeaseOwnerContext::default()
        },
    )
    .unwrap();
    assert_eq!(
        observer
            .session_external_harness_state(session.id)
            .await
            .unwrap(),
        Some(live.clone())
    );
    assert!(
        active_session_owners(root.path(), session.id)
            .unwrap()
            .is_empty()
    );
    // The original loaded-but-idle actor path must also remain non-owning.
    assert_eq!(
        sessions
            .session_external_harness_state(session.id)
            .await
            .unwrap(),
        Some(live)
    );
    assert!(
        active_session_owners(root.path(), session.id)
            .unwrap()
            .is_empty()
    );
    let binding = HarnessSessionBinding {
        schema_version: HARNESS_SCHEMA_VERSION,
        session_id: session.id.to_string(),
        instance_id: "fixture".into(),
        native_session_id: "confirmed".into(),
        native_version: "fixture-v1".into(),
        generation: 1,
    };
    let mut confirmation = page(&owner);
    confirmation.observations[0].kind = HarnessObservationKind::BindingConfirmed {
        open_id: "open".into(),
        binding: binding.clone(),
    };
    sessions
        .commit_external_harness_page(&owner, None, &confirmation)
        .await
        .unwrap();
    let active = sessions
        .session_external_harness_state(session.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        active.target,
        Some(HarnessSessionTarget::Confirmed(binding.clone()))
    );
    assert_eq!(active.binding, Some(binding));
    sessions
        .release_idle_session_resources(session.id)
        .await
        .unwrap();
    assert_eq!(
        observer
            .session_external_harness_state(session.id)
            .await
            .unwrap(),
        Some(active)
    );
    assert!(
        active_session_owners(root.path(), session.id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn offline_snapshot_rejects_missing_exact_index_without_repair_or_history_mutation() {
    use bcode_session::db::SessionDb;
    let root = tempfile::tempdir().unwrap();
    let sessions = manager(root.path());
    let session = sessions
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let owner = owner(session.id);
    sessions
        .configure_external_harness(&owner, instance(), capabilities())
        .await
        .unwrap();
    sessions
        .release_idle_session_resources(session.id)
        .await
        .unwrap();
    let db = SessionDb::open_existing_turso_in_root(session.id, root.path())
        .await
        .unwrap();
    let tail = db.last_event_sequence().await.unwrap();
    switchy::database::schema::drop_index("idx_turn_evidence_sequence", "turn_evidence")
        .execute(db.database())
        .await
        .unwrap();
    assert!(db.external_harness_snapshot().await.is_err());
    assert_eq!(db.last_event_sequence().await.unwrap(), tail);
    drop(db);
    assert!(
        sessions
            .session_external_harness_state(session.id)
            .await
            .is_err()
    );
    assert!(
        bcode_session::lease::active_session_owners(root.path(), session.id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn authentication_and_unconfigured_native_claims_are_never_retained() {
    let root = tempfile::tempdir().unwrap();
    let sessions = manager(root.path());
    let session = sessions
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let owner = owner(session.id);
    assert!(
        sessions
            .claim_external_harness_request(&owner, &open(&owner))
            .await
            .is_err()
    );
    sessions
        .configure_external_harness(&owner, instance(), capabilities())
        .await
        .unwrap();
    let auth = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "auth".into(),
        instance: instance(),
        kind: HarnessRequestKind::AuthStatus { verify: false },
    };
    assert!(
        sessions
            .claim_external_harness_request(&owner, &auth)
            .await
            .is_err()
    );
    assert!(
        sessions
            .external_harness_history(session.id, Some("native-cursor".into()), 128)
            .await
            .is_err()
    );
}
