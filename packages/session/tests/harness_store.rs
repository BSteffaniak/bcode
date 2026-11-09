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
    Box::pin(check_typed_submission_acceptance(
        false,
        HarnessTerminalDisposition::Completed,
    ))
    .await;
    Box::pin(check_typed_submission_acceptance(
        false,
        HarnessTerminalDisposition::Interrupted,
    ))
    .await;
    Box::pin(check_typed_submission_acceptance(
        true,
        HarnessTerminalDisposition::Failed,
    ))
    .await;
}

async fn check_typed_submission_acceptance(
    with_gap: bool,
    terminal_disposition: HarnessTerminalDisposition,
) {
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
    let mut frames = page(&owner);
    frames.next_after = Some("native-frames".into());
    let mut consumed = frames.observations[0].clone();
    consumed.observation_id = "consumed".into();
    consumed.native_correlations.clear();
    consumed.kind = HarnessObservationKind::SubmissionConsumed {
        submission_id: "accepted-input".into(),
        attempt_id: "accepted-attempt".into(),
        native_frame_id: "consumed-frame".into(),
    };
    let mut observed = consumed.clone();
    observed.observation_id = "result-observed".into();
    observed.kind = HarnessObservationKind::SubmissionResultObserved {
        submission_id: "accepted-input".into(),
        attempt_id: "accepted-attempt".into(),
        native_frame_id: "result-frame".into(),
        is_error: false,
        queued_user_sends: Some(0),
    };
    frames.observations = vec![consumed, observed];
    for index in 0..2 {
        let mut wrong = frames.clone();
        wrong.observations = vec![wrong.observations[index].clone()];
        match &mut wrong.observations[0].kind {
            HarnessObservationKind::SubmissionConsumed { attempt_id, .. }
            | HarnessObservationKind::SubmissionResultObserved { attempt_id, .. } => {
                *attempt_id = "foreign-attempt".into()
            }
            _ => unreachable!(),
        }
        assert!(
            sessions
                .commit_external_harness_page(&owner, None, &wrong)
                .await
                .is_err()
        );
    }
    let mut foreign_generation = frames.clone();
    foreign_generation.observations[0].generation += 1;
    assert!(
        sessions
            .commit_external_harness_page(&owner, None, &foreign_generation)
            .await
            .is_err()
    );
    let mut duplicate = frames.clone();
    duplicate.observations[1] = duplicate.observations[0].clone();
    duplicate.observations[1].observation_id = "renamed-duplicate".into();
    assert!(
        sessions
            .commit_external_harness_page(&owner, None, &duplicate)
            .await
            .is_err()
    );
    sessions
        .commit_external_harness_page(&owner, None, &frames)
        .await
        .unwrap();
    sessions
        .commit_external_harness_page(&owner, None, &frames)
        .await
        .unwrap();
    for index in 0..2 {
        let mut duplicate = frames.clone();
        duplicate.next_after = Some(format!("duplicate-{index}"));
        duplicate.observations = vec![duplicate.observations[index].clone()];
        duplicate.observations[0].observation_id = format!("renamed-frame-{index}");
        assert!(
            sessions
                .commit_external_harness_page(&owner, Some("native-frames"), &duplicate)
                .await
                .is_err()
        );
    }
    let mut conflicting_frame = frames.clone();
    conflicting_frame.next_after = Some("conflicting-frame".into());
    conflicting_frame.observations = vec![conflicting_frame.observations[1].clone()];
    conflicting_frame.observations[0].observation_id = "changed-result-observed".into();
    if let HarnessObservationKind::SubmissionResultObserved {
        is_error,
        queued_user_sends,
        ..
    } = &mut conflicting_frame.observations[0].kind
    {
        *is_error = true;
        *queued_user_sends = Some(2);
    }
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("native-frames"), &conflicting_frame)
            .await
            .is_err()
    );
    let mut cross_kind = frames.clone();
    cross_kind.next_after = Some("cross-kind-frame".into());
    cross_kind.observations = vec![cross_kind.observations[1].clone()];
    cross_kind.observations[0].observation_id = "cross-kind-frame".into();
    if let HarnessObservationKind::SubmissionResultObserved {
        native_frame_id, ..
    } = &mut cross_kind.observations[0].kind
    {
        *native_frame_id = "consumed-frame".into();
    }
    sessions
        .commit_external_harness_page(&owner, Some("native-frames"), &cross_kind)
        .await
        .unwrap();
    // The prepared target permits only its first submit. Further native sends must use
    // the native-confirmed binding, just like the production multi-attempt path.
    let binding = HarnessSessionBinding {
        schema_version: HARNESS_SCHEMA_VERSION,
        session_id: session.id.to_string(),
        instance_id: "fixture".into(),
        native_session_id: "native-confirmed".into(),
        native_version: "fixture-v1".into(),
        generation: owner.generation,
    };
    let mut confirmation = page(&owner);
    confirmation.next_after = Some("merged-binding-confirmed".into());
    confirmation.observations[0].observation_id = "merged-binding-confirmed".into();
    confirmation.observations[0].native_correlations.clear();
    confirmation.observations[0].kind = HarnessObservationKind::BindingConfirmed {
        open_id: "open".into(),
        binding: binding.clone(),
    };
    sessions
        .commit_external_harness_page(&owner, Some("cross-kind-frame"), &confirmation)
        .await
        .unwrap();
    // A merged native frame may attribute a second exact retained attempt, even in
    // the same stage. Claim it canonically before publishing that attribution.
    let mut merged_submit: HarnessRequest =
        serde_json::from_value(serde_json::to_value(&submit).unwrap()).unwrap();
    merged_submit.request_id = "merged-submit".into();
    if let HarnessRequestKind::Submit { target, submission } = &mut merged_submit.kind {
        *target = HarnessSessionTarget::Confirmed(binding);
        submission.submission_id = "merged-input".into();
        submission.attempt_id = "merged-attempt".into();
    }
    sessions
        .claim_external_harness_request(&owner, &merged_submit)
        .await
        .unwrap();
    let mut merged = frames.clone();
    merged.next_after = Some("merged-native-frame".into());
    for (index, observation) in merged.observations.iter_mut().enumerate() {
        observation.observation_id = format!("merged-frame-{index}");
        match &mut observation.kind {
            HarnessObservationKind::SubmissionConsumed {
                submission_id,
                attempt_id,
                ..
            }
            | HarnessObservationKind::SubmissionResultObserved {
                submission_id,
                attempt_id,
                ..
            } => {
                *submission_id = "merged-input".into();
                *attempt_id = "merged-attempt".into();
            }
            _ => unreachable!(),
        }
    }
    let mut another_submit: HarnessRequest =
        serde_json::from_value(serde_json::to_value(&merged_submit).unwrap()).unwrap();
    another_submit.request_id = "another-merged-submit".into();
    if let HarnessRequestKind::Submit { submission, .. } = &mut another_submit.kind {
        submission.submission_id = "another-merged-input".into();
        submission.attempt_id = "another-merged-attempt".into();
    }
    sessions
        .claim_external_harness_request(&owner, &another_submit)
        .await
        .unwrap();
    let mut another_attribution = merged.observations.clone();
    for (index, observation) in another_attribution.iter_mut().enumerate() {
        observation.observation_id = format!("another-merged-frame-{index}");
        match &mut observation.kind {
            HarnessObservationKind::SubmissionConsumed {
                submission_id,
                attempt_id,
                ..
            }
            | HarnessObservationKind::SubmissionResultObserved {
                submission_id,
                attempt_id,
                ..
            } => {
                *submission_id = "another-merged-input".into();
                *attempt_id = "another-merged-attempt".into();
            }
            _ => unreachable!(),
        }
    }
    merged.observations.extend(another_attribution);
    sessions
        .commit_external_harness_page(&owner, Some("merged-binding-confirmed"), &merged)
        .await
        .unwrap();
    sessions
        .commit_external_harness_page(&owner, Some("merged-binding-confirmed"), &merged)
        .await
        .unwrap();
    let mut merged_duplicate = merged.clone();
    merged_duplicate.next_after = Some("merged-duplicate".into());
    merged_duplicate.observations[0].observation_id = "renamed-merged".into();
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("merged-native-frame"), &merged_duplicate)
            .await
            .is_err()
    );
    let frame_only = sessions
        .session_external_harness_submission_evidence(
            session.id,
            "accepted-input",
            "accepted-attempt",
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame_only.1.completeness, HarnessCompleteness::Partial);
    assert!(frame_only.1.observations.is_empty());
    assert!(
        sessions
            .session_external_harness_submission_acceptance(
                session.id,
                "accepted-input",
                "accepted-attempt",
            )
            .await
            .unwrap()
            .is_none()
    );
    // Acceptance is beyond the old first-1024-observation scan window.
    let mut prefix_cursor = merged.next_after;
    for batch in 0..9 {
        let mut prefix = page(&owner);
        prefix.next_after = Some(format!("prefix-{batch}"));
        prefix.observations = (0..128)
            .map(|offset| {
                let mut observation = prefix.observations[0].clone();
                observation.observation_id = format!("prefix-{batch}-{offset}");
                observation.native_correlations.clear();
                observation
            })
            .collect();
        // Loss before acceptance is not evidence loss for this submission.
        if batch == 0 {
            prefix.completeness = HarnessCompleteness::Gap;
        }
        sessions
            .commit_external_harness_page(&owner, prefix_cursor.as_deref(), &prefix)
            .await
            .unwrap();
        prefix_cursor = prefix.next_after;
    }
    sessions
        .commit_external_harness_page(&owner, prefix_cursor.as_deref(), &accepted)
        .await
        .unwrap();
    sessions
        .commit_external_harness_page(&owner, prefix_cursor.as_deref(), &accepted)
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
    let missing = sessions
        .session_external_harness_submission_evidence(
            session.id,
            "accepted-input",
            "accepted-attempt",
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(missing.0, owner);
    assert_eq!(missing.1.completeness, HarnessCompleteness::Partial);
    assert_eq!(missing.1.observations, accepted.observations);
    assert!(missing.1.next_after.as_deref().unwrap().starts_with("h1:"));
    let mut terminal = page(&owner);
    terminal.next_after = Some("terminal".into());
    let mut result = terminal.observations[0].clone();
    result.observation_id = "result".into();
    result.native_correlations = vec!["actual-native-turn".into()];
    result.kind = HarnessObservationKind::Result {
        text: "late result".into(),
    };
    let mut settlement = result.clone();
    settlement.observation_id = "settlement".into();
    settlement.kind = HarnessObservationKind::Settlement {
        native_turn_id: "actual-native-turn".into(),
    };
    terminal.observations = vec![result, settlement];
    let mut result_page = terminal.clone();
    result_page.next_after = Some("result-only".into());
    result_page.observations.truncate(1);
    sessions
        .commit_external_harness_page(&owner, Some("native-cursor"), &result_page)
        .await
        .unwrap();
    let mut conflicting = result_page.clone();
    conflicting.next_after = Some("conflict-result".into());
    conflicting.observations[0].observation_id = "conflict-result".into();
    conflicting.observations[0].kind = HarnessObservationKind::Result {
        text: "different complete result".into(),
    };
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("result-only"), &conflicting)
            .await
            .is_err()
    );
    let mut native_terminal = terminal.observations[0].clone();
    native_terminal.observation_id = "native-terminal".into();
    native_terminal.kind = HarnessObservationKind::NativeTurnTerminal {
        native_turn_id: "actual-native-turn".into(),
        disposition: terminal_disposition,
    };
    let mut native_terminal_page = terminal.clone();
    native_terminal_page.next_after = Some("native-terminal".into());
    native_terminal_page.observations = vec![native_terminal.clone()];
    let mut wrong_terminal = native_terminal_page.clone();
    wrong_terminal.observations[0].kind = HarnessObservationKind::NativeTurnTerminal {
        native_turn_id: "not-accepted".into(),
        disposition: HarnessTerminalDisposition::Completed,
    };
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("result-only"), &wrong_terminal)
            .await
            .is_err()
    );
    sessions
        .commit_external_harness_page(&owner, Some("result-only"), &native_terminal_page)
        .await
        .unwrap();
    sessions
        .commit_external_harness_page(&owner, Some("result-only"), &native_terminal_page)
        .await
        .unwrap();
    for disposition in [
        HarnessTerminalDisposition::Completed,
        HarnessTerminalDisposition::Interrupted,
        HarnessTerminalDisposition::Failed,
    ] {
        let mut conflicting = native_terminal_page.clone();
        conflicting.next_after = Some("conflicting-native-terminal".into());
        conflicting.observations[0].observation_id = "renamed-native-terminal".into();
        conflicting.observations[0].kind = HarnessObservationKind::NativeTurnTerminal {
            native_turn_id: "actual-native-turn".into(),
            disposition,
        };
        assert!(
            sessions
                .commit_external_harness_page(&owner, Some("native-terminal"), &conflicting)
                .await
                .is_err()
        );
    }
    let terminal_without_settlement = sessions
        .session_external_harness_submission_evidence(
            session.id,
            "accepted-input",
            "accepted-attempt",
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        terminal_without_settlement.1.completeness,
        HarnessCompleteness::Partial
    );
    assert_eq!(terminal_without_settlement.1.observations.len(), 3);
    terminal.observations.insert(1, native_terminal);
    let mut settlement_page = terminal.clone();
    settlement_page.observations.drain(..2);
    sessions
        .commit_external_harness_page(&owner, Some("native-terminal"), &settlement_page)
        .await
        .unwrap();
    let complete = sessions
        .session_external_harness_submission_evidence(
            session.id,
            "accepted-input",
            "accepted-attempt",
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(complete.0, owner);
    assert_eq!(complete.1.completeness, HarnessCompleteness::Complete);
    assert_eq!(complete.1.observations.len(), 4);
    assert_eq!(
        &complete.1.observations[1..],
        terminal.observations.as_slice()
    );
    // An uncorrelated gap after acceptance (even after settlement) remains a fence.
    let mut gap = page(&owner);
    gap.next_after = Some("gap".into());
    gap.observations[0].observation_id = "gap".into();
    gap.observations[0].native_correlations.clear();
    gap.status.completeness = HarnessCompleteness::Gap;
    if with_gap {
        sessions
            .commit_external_harness_page(&owner, Some("terminal"), &gap)
            .await
            .unwrap();
    }
    let fenced = sessions
        .session_external_harness_submission_evidence(
            session.id,
            "accepted-input",
            "accepted-attempt",
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fenced.1.completeness,
        if with_gap {
            HarnessCompleteness::Gap
        } else {
            HarnessCompleteness::Complete
        }
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
    let reopened_frames = observer
        .external_harness_history(session.id, None, 2)
        .await
        .unwrap();
    assert_eq!(reopened_frames.observations.len(), 2);
    assert!(matches!(&reopened_frames.observations[0].kind,
        HarnessObservationKind::SubmissionConsumed { submission_id, attempt_id, native_frame_id }
            if submission_id == "accepted-input" && attempt_id == "accepted-attempt"
                && native_frame_id == "consumed-frame"));
    assert!(matches!(&reopened_frames.observations[1].kind,
        HarnessObservationKind::SubmissionResultObserved {
            submission_id, attempt_id, native_frame_id, is_error: false, queued_user_sends: Some(0),
        } if submission_id == "accepted-input" && attempt_id == "accepted-attempt"
            && native_frame_id == "result-frame"));
    assert_eq!(
        observer
            .session_external_harness_submission_evidence(
                session.id,
                "accepted-input",
                "accepted-attempt",
            )
            .await
            .unwrap(),
        Some(fenced.clone()),
    );
    let maintenance =
        bcode_session::lease::acquire_session_maintenance_guard(root.path(), session.id).unwrap();
    let write = bcode_session::lease::acquire_maintenance_session_write_lock(
        &maintenance,
        root.path(),
        session.id,
    )
    .unwrap();
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(session.id, root.path())
        .await
        .unwrap();
    let before_reindex = db.last_event_sequence().await.unwrap();
    use switchy::database::query::FilterableQuery as _;
    db.database()
        .delete("turn_evidence")
        .where_eq("evidence_kind", "harness_operation_v1")
        .where_eq(
            "turn_id",
            bcode_session_models::external_backend::external_backend_key(
                "terminal_coverage_v2",
                "current",
            ),
        )
        .execute(db.database())
        .await
        .unwrap();
    let error = db
        .external_harness_submission_evidence("accepted-input", "accepted-attempt")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("maintenance"));
    assert!(
        db.reindex_external_harness_evidence(&maintenance, &write)
            .await
            .unwrap()
            > 0
    );
    assert_eq!(db.last_event_sequence().await.unwrap(), before_reindex);
    assert_eq!(
        db.external_harness_submission_evidence("accepted-input", "accepted-attempt")
            .await
            .unwrap(),
        Some(fenced)
    );
    drop(db);
    drop(write);
    drop(maintenance);
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
        model_switch: bcode_harness_models::HarnessCapability::unsupported(),
        permission_mode: bcode_harness_models::HarnessCapability::unsupported(),
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
        live: Vec::new(),
        retention: None,
        hydration_requests: Vec::new(),
        inventory_page: None,
        observations: vec![HarnessObservation {
            observation_id: "observation".into(),
            session_id: owner.session_id.to_string(),
            generation: 1,
            native_correlations: vec!["turn".into()],
            completeness: HarnessCompleteness::Complete,
            kind: HarnessObservationKind::Text {
                segment_id: None,
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
        segment_id: None,
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
    // One bounded page aggregates every committed canonical page.
    let mut committed = native.observations.clone();
    committed.extend(confirmation.observations.iter().cloned());
    assert_eq!(
        reopened
            .external_harness_history(session.id, None, 128)
            .await
            .unwrap()
            .observations,
        committed
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
    assert!(
        db.external_harness_submission_evidence("submission", "attempt")
            .await
            .is_err()
    );
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

async fn execution_fixture(root: &std::path::Path) -> (SessionManager, ExternalHarnessOwner) {
    execution_fixture_with_manager(root, manager(root)).await
}

async fn execution_fixture_with_manager(
    root: &std::path::Path,
    sessions: SessionManager,
) -> (SessionManager, ExternalHarnessOwner) {
    let session = sessions.create_session(None, root.into()).await.unwrap();
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
        request_id: "execution-submit".into(),
        instance: instance(),
        kind: HarnessRequestKind::Submit {
            target: HarnessSessionTarget::Prepared(prepared(&owner)),
            submission: HarnessSubmission {
                submission_id: "execution-input".into(),
                attempt_id: "execution-attempt".into(),
                session_id: owner.session_id.to_string(),
                generation: owner.generation,
                text: "execution".into(),
                placement: HarnessPlacement::NextTurn,
            },
        },
    };
    sessions
        .claim_external_harness_request(&owner, &submit)
        .await
        .unwrap();
    (sessions, owner)
}

fn execution_attempt() -> HarnessAttemptIdentity {
    HarnessAttemptIdentity {
        submission_id: "execution-input".into(),
        attempt_id: "execution-attempt".into(),
    }
}

fn execution_page(
    owner: &ExternalHarnessOwner,
    id: &str,
    event: HarnessExecutionEvent,
) -> HarnessObservationPage {
    let mut result = page(owner);
    result.next_after = Some(id.into());
    result.observations[0].observation_id = id.into();
    result.observations[0].native_correlations.clear();
    result.observations[0].kind = HarnessObservationKind::Execution { event };
    result
}

fn execution_terminal() -> HarnessExecutionEvent {
    HarnessExecutionEvent::SubmissionTerminal {
        attempt: execution_attempt(),
        native_turn_id: None,
        native_frame_id: Some("native-result".into()),
        disposition: HarnessTerminalDisposition::Completed,
        output: Some("foreground done".into()),
    }
}

fn execution_work(
    origin: Option<HarnessAttemptIdentity>,
    state: HarnessWorkState,
) -> HarnessExecutionEvent {
    HarnessExecutionEvent::BackgroundWork {
        work_id: "work-1".into(),
        native_task_id: Some("native-task".into()),
        origin,
        parent_work_id: None,
        work_kind: HarnessWorkKind::Command,
        label: "background".into(),
        state,
        notification: HarnessNotificationState::Pending,
    }
}

#[tokio::test]
async fn retained_pages_allow_null_cursor_reuse_without_losing_exact_retry_fences() {
    use bcode_harness_models::HarnessRetentionPage;
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let target = sessions
        .session_external_harness_state(owner.session_id)
        .await
        .unwrap()
        .unwrap()
        .target
        .unwrap();
    let mut first = execution_page(
        &owner,
        "cursor-a",
        execution_work(None, HarnessWorkState::Running),
    );
    let mut empty = first.clone();
    empty.observations.clear();
    empty.next_after = None;
    let mut third = first.clone();
    third.observations[0].observation_id = "cursor-b".into();
    third.next_after = Some("cursor-b".into());
    for (index, page) in [&mut first, &mut empty, &mut third].into_iter().enumerate() {
        page.retention = Some(HarnessRetentionPage {
            connection_id: "cursor-connection".into(),
            page_id: format!("cursor-page-{index}"),
            observations_digest: bcode_harness_models::harness_observations_digest(
                &page.observations,
            )
            .unwrap(),
        });
    }
    let mut previous_revision = 0;
    for (after, page) in [(None, &first), (Some("cursor-a"), &empty), (None, &third)] {
        sessions
            .commit_external_harness_page(&owner, after, page)
            .await
            .unwrap();
        let context = sessions
            .session_harness_retention_context(&owner, &target, page.next_after.as_deref())
            .await
            .unwrap()
            .unwrap();
        let ack = context.acknowledgment.unwrap();
        assert_eq!(ack.page_id, page.retention.as_ref().unwrap().page_id);
        assert_eq!(
            ack.observations_digest,
            page.retention.as_ref().unwrap().observations_digest
        );
        assert!(ack.canonical_revision > previous_revision);
        previous_revision = ack.canonical_revision;
        sessions
            .commit_external_harness_page(&owner, after, page)
            .await
            .unwrap();
        assert_eq!(
            sessions
                .session_harness_canonical_page_ack(&owner, page.retention.as_ref().unwrap(),)
                .await
                .unwrap(),
            ack
        );
        let mut changed_digest = page.clone();
        changed_digest
            .retention
            .as_mut()
            .unwrap()
            .observations_digest = "0".repeat(64);
        assert!(
            sessions
                .commit_external_harness_page(&owner, after, &changed_digest)
                .await
                .is_err()
        );
        assert!(
            sessions
                .session_harness_canonical_page_ack(
                    &owner,
                    changed_digest.retention.as_ref().unwrap(),
                )
                .await
                .is_err()
        );
        let repeated_context = sessions
            .session_harness_retention_context(&owner, &target, page.next_after.as_deref())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(repeated_context.acknowledgment.as_ref(), Some(&ack));
        let mut changed_record = page.clone();
        changed_record.status.activity = bcode_harness_models::HarnessActivity::Unknown;
        changed_record.status.attention = bcode_harness_models::HarnessAttention::Assistance;
        assert!(
            sessions
                .commit_external_harness_page(&owner, after, &changed_record)
                .await
                .is_err()
        );
    }
    // Historical exact retries stay idempotent after their cursor has been reused.
    sessions
        .commit_external_harness_page(&owner, None, &first)
        .await
        .unwrap();
    sessions
        .commit_external_harness_page(&owner, Some("cursor-a"), &empty)
        .await
        .unwrap();
    let current = sessions
        .session_harness_retention_context(&owner, &target, Some("cursor-b"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        current.acknowledgment.unwrap().canonical_revision,
        previous_revision
    );
    assert!(
        sessions
            .session_harness_retention_context(&owner, &target, None)
            .await
            .is_err()
    );
    let mut stale = third.clone();
    stale.retention.as_mut().unwrap().page_id = "fresh-but-stale-input".into();
    assert!(
        sessions
            .commit_external_harness_page(&owner, None, &stale)
            .await
            .is_err()
    );
    let mut changed_observations = first.clone();
    changed_observations.observations[0].observation_id = "conflicting-observation".into();
    changed_observations.next_after = Some("conflicting-observation".into());
    changed_observations
        .retention
        .as_mut()
        .unwrap()
        .observations_digest =
        bcode_harness_models::harness_observations_digest(&changed_observations.observations)
            .unwrap();
    assert!(
        sessions
            .commit_external_harness_page(&owner, None, &changed_observations)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn unretained_empty_pages_extend_a_recurring_null_cursor_without_losing_retry_fences() {
    use bcode_harness_models::HarnessRetentionPage;
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let mut first = execution_page(
        &owner,
        "cursor-a",
        execution_work(None, HarnessWorkState::Running),
    );
    first.retention = Some(HarnessRetentionPage {
        connection_id: "null-cursor-connection".into(),
        page_id: "null-cursor-page".into(),
        observations_digest: bcode_harness_models::harness_observations_digest(&first.observations)
            .unwrap(),
    });
    sessions
        .commit_external_harness_page(&owner, None, &first)
        .await
        .unwrap();
    // An idle wait transfers nothing: no retention and no successor cursor.
    let mut empty = first.clone();
    empty.observations.clear();
    empty.next_after = None;
    empty.retention = None;
    sessions
        .commit_external_harness_page(&owner, Some("cursor-a"), &empty)
        .await
        .unwrap();
    // The next idle or live-only window reuses the null input cursor of the first page; it
    // extends the current committed predecessor and is not a conflicting retry of that page.
    let mut idle = empty.clone();
    idle.status.activity = bcode_harness_models::HarnessActivity::Unknown;
    sessions
        .commit_external_harness_page(&owner, None, &idle)
        .await
        .unwrap();
    sessions
        .commit_external_harness_page(&owner, None, &idle)
        .await
        .unwrap();
    // Evidence still follows the current predecessor; a stale predecessor stays refused.
    let mut later = execution_page(
        &owner,
        "cursor-b",
        execution_work(None, HarnessWorkState::Running),
    );
    later.observations[0].observation_id = "cursor-b".into();
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("cursor-a"), &later)
            .await
            .is_err()
    );
    sessions
        .commit_external_harness_page(&owner, None, &later)
        .await
        .unwrap();
    let state = sessions
        .session_external_harness_state(owner.session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.observation_after.as_deref(), Some("cursor-b"));
}

#[tokio::test]
async fn retention_context_reads_exact_previous_page_and_committed_requests() {
    use bcode_harness_models::{
        HarnessHydrationRequest, HarnessHydrationResolution, HarnessNativeReference,
        HarnessNativeReferenceKind, HarnessRetentionPage,
    };
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let state = sessions
        .session_external_harness_state(owner.session_id)
        .await
        .unwrap()
        .unwrap();
    let target = state.target.unwrap();
    assert_eq!(
        sessions
            .session_harness_retention_context(&owner, &target, None)
            .await
            .unwrap(),
        None
    );
    assert!(
        sessions
            .session_harness_retention_context(&owner, &target, Some("uncommitted"))
            .await
            .is_err()
    );
    let mut page = execution_page(
        &owner,
        "context-work",
        execution_work(None, HarnessWorkState::Running),
    );
    page.hydration_requests.push(HarnessHydrationRequest {
        request_id: "context-hydrate".into(),
        reference: HarnessNativeReference {
            kind: HarnessNativeReferenceKind::Process,
            native_id: "native-task".into(),
            native_turn_id: None,
        },
    });
    page.retention = Some(HarnessRetentionPage {
        connection_id: "context-connection".into(),
        page_id: "context-page".into(),
        observations_digest: bcode_harness_models::harness_observations_digest(&page.observations)
            .unwrap(),
    });
    sessions
        .commit_external_harness_page(&owner, None, &page)
        .await
        .unwrap();
    let first = sessions
        .session_harness_retention_context(&owner, &target, Some("context-work"))
        .await
        .unwrap()
        .unwrap();
    let second = sessions
        .session_harness_retention_context(&owner, &target, Some("context-work"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(first.connection_id, "context-connection");
    let ack = first.acknowledgment.as_ref().unwrap();
    assert_eq!(ack.page_id, "context-page");
    assert_eq!(
        ack.observations_digest,
        page.retention.as_ref().unwrap().observations_digest
    );
    assert_eq!(first.hydration.len(), 1);
    assert_eq!(first.hydration[0].request_id, "context-hydrate");
    assert!(
        matches!(&first.hydration[0].resolution, HarnessHydrationResolution::Found { observations }
        if observations.contains(&page.observations[0]))
    );
    assert!(
        sessions
            .session_harness_retention_context(&owner, &target, None)
            .await
            .is_err()
    );
    let oversized: Vec<_> = (0..33)
        .map(|index| HarnessHydrationRequest {
            request_id: format!("excess-{index}"),
            reference: page.hydration_requests[0].reference.clone(),
        })
        .collect();
    assert!(
        sessions
            .session_harness_native_hydration(&owner, &oversized)
            .await
            .is_err()
    );
    assert!(
        sessions
            .session_harness_retention_context(&owner, &target, Some("foreign-cursor"))
            .await
            .is_err()
    );
    let mut foreign = owner.clone();
    foreign.generation += 1;
    assert!(
        sessions
            .session_harness_retention_context(&foreign, &target, Some("context-work"))
            .await
            .is_err()
    );
    sessions
        .release_idle_session_resources(owner.session_id)
        .await
        .unwrap();
    let restarted = sessions
        .session_harness_retention_context(&owner, &target, Some("context-work"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restarted, first);
    let without_metadata = execution_page(
        &owner,
        "context-no-metadata",
        execution_work(None, HarnessWorkState::Completed),
    );
    sessions
        .commit_external_harness_page(&owner, Some("context-work"), &without_metadata)
        .await
        .unwrap();
    assert!(
        sessions
            .session_harness_retention_context(&owner, &target, Some("context-no-metadata"))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn retention_hydration_damage_is_unavailable_not_indexed_absence() {
    use bcode_harness_models::{
        HarnessHydrationRequest, HarnessHydrationResolution, HarnessNativeReference,
        HarnessNativeReferenceKind, HarnessRetentionPage,
    };
    use switchy::database::query::FilterableQuery;
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let state = sessions
        .session_external_harness_state(owner.session_id)
        .await
        .unwrap()
        .unwrap();
    let target = state.target.unwrap();
    let mut page = execution_page(
        &owner,
        "damage-work",
        execution_work(None, HarnessWorkState::Running),
    );
    page.hydration_requests.push(HarnessHydrationRequest {
        request_id: "damage-request".into(),
        reference: HarnessNativeReference {
            kind: HarnessNativeReferenceKind::Process,
            native_id: "never-indexed".into(),
            native_turn_id: None,
        },
    });
    page.retention = Some(HarnessRetentionPage {
        connection_id: "damage-connection".into(),
        page_id: "damage-page".into(),
        observations_digest: bcode_harness_models::harness_observations_digest(&page.observations)
            .unwrap(),
    });
    sessions
        .commit_external_harness_page(&owner, None, &page)
        .await
        .unwrap();
    let before = sessions
        .session_harness_retention_context(&owner, &target, Some("damage-work"))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        before.hydration[0].resolution,
        HarnessHydrationResolution::Absent
    ));
    sessions
        .release_idle_session_resources(owner.session_id)
        .await
        .unwrap();
    let db =
        bcode_session::db::SessionDb::open_existing_turso_in_root(owner.session_id, root.path())
            .await
            .unwrap();
    let tail = db.last_event_sequence().await.unwrap();
    let namespace = format!(
        "execution_v3:{}:inventory:native_reference_v1",
        owner.generation
    );
    db.database()
        .delete("snapshots")
        .where_eq("snapshot_name", namespace.clone())
        .execute(db.database())
        .await
        .unwrap();
    let damaged = db
        .harness_retention_context(&owner, &target, Some("damage-work"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(damaged.acknowledgment, before.acknowledgment);
    assert!(matches!(
        damaged.hydration[0].resolution,
        HarnessHydrationResolution::Unavailable
    ));
    assert_eq!(db.last_event_sequence().await.unwrap(), tail);
    assert!(
        db.database()
            .select("snapshots")
            .columns(&["snapshot_name"])
            .where_eq("snapshot_name", namespace)
            .execute(db.database())
            .await
            .unwrap()
            .is_empty()
    );
    let page_key = bcode_session_models::external_backend::external_backend_key("page", "current");
    db.database()
        .delete("turn_evidence")
        .where_eq("turn_id", page_key)
        .where_eq("evidence_kind", "harness_operation_v1")
        .execute(db.database())
        .await
        .unwrap();
    assert!(
        db.harness_retention_context(&owner, &target, Some("damage-work"))
            .await
            .is_err()
    );
    assert_eq!(db.last_event_sequence().await.unwrap(), tail);
}

#[tokio::test]
async fn native_hydration_uses_exact_canonical_reference_and_ack_digest() {
    use bcode_harness_models::{
        HarnessHydrationRequest, HarnessHydrationResolution, HarnessNativeReference,
        HarnessNativeReferenceKind, HarnessRetentionPage,
    };
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let mut work = execution_page(
        &owner,
        "hydrate-running",
        execution_work(Some(execution_attempt()), HarnessWorkState::Running),
    );
    let retention = HarnessRetentionPage {
        connection_id: "hydrate-connection".into(),
        page_id: "hydrate-page".into(),
        observations_digest: bcode_harness_models::harness_observations_digest(&work.observations)
            .unwrap(),
    };
    work.retention = Some(retention.clone());
    sessions
        .commit_external_harness_page(&owner, None, &work)
        .await
        .unwrap();
    let ack = sessions
        .session_harness_canonical_page_ack(&owner, &retention)
        .await
        .unwrap();
    assert_eq!(ack.page_id, retention.page_id);
    assert_eq!(ack.observations_digest, retention.observations_digest);
    let mut forged = retention.clone();
    forged.observations_digest = "0".repeat(64);
    assert!(
        sessions
            .session_harness_canonical_page_ack(&owner, &forged)
            .await
            .is_err()
    );
    let completed = execution_page(
        &owner,
        "hydrate-completed",
        execution_work(Some(execution_attempt()), HarnessWorkState::Completed),
    );
    sessions
        .commit_external_harness_page(&owner, Some("hydrate-running"), &completed)
        .await
        .unwrap();
    let request = HarnessHydrationRequest {
        request_id: "hydrate-request".into(),
        reference: HarnessNativeReference {
            kind: HarnessNativeReferenceKind::Process,
            native_id: "native-task".into(),
            native_turn_id: None,
        },
    };
    let found = sessions
        .session_harness_native_hydration(&owner, std::slice::from_ref(&request))
        .await
        .unwrap();
    assert!(found[0].canonical_revision > ack.canonical_revision);
    let HarnessHydrationResolution::Found { observations } = &found[0].resolution else {
        panic!("retained work");
    };
    assert!(observations.iter().any(|observation| matches!(
        observation.kind,
        HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::BackgroundWork {
                state: HarnessWorkState::Completed,
                ..
            }
        }
    )));
    let mut acceptance = page(&owner);
    acceptance.next_after = Some("hydrate-accepted".into());
    acceptance.observations[0].observation_id = "hydrate-accepted".into();
    acceptance.observations[0].kind = HarnessObservationKind::SubmissionAccepted {
        submission_id: "execution-input".into(),
        attempt_id: "execution-attempt".into(),
        native_turn_id: "hydrated-turn".into(),
    };
    sessions
        .commit_external_harness_page(&owner, Some("hydrate-completed"), &acceptance)
        .await
        .unwrap();
    let turn_request = HarnessHydrationRequest {
        request_id: "hydrate-turn".into(),
        reference: HarnessNativeReference {
            kind: HarnessNativeReferenceKind::Turn,
            native_id: "hydrated-turn".into(),
            native_turn_id: None,
        },
    };
    let turn = sessions
        .session_harness_native_hydration(&owner, &[turn_request])
        .await
        .unwrap();
    assert!(
        matches!(&turn[0].resolution, HarnessHydrationResolution::Found { observations }
        if observations.iter().any(|observation| matches!(observation.kind, HarnessObservationKind::SubmissionAccepted { .. })))
    );
    let mut item_request = request.clone();
    item_request.reference.kind = HarnessNativeReferenceKind::Item;
    item_request.reference.native_turn_id = Some("hydrated-turn".into());
    let item = sessions
        .session_harness_native_hydration(&owner, &[item_request])
        .await
        .unwrap();
    assert!(matches!(
        item[0].resolution,
        HarnessHydrationResolution::Unavailable
    ));
    let mut missing = request.clone();
    missing.reference.native_id = "never-recorded".into();
    let absent = sessions
        .session_harness_native_hydration(&owner, &[missing])
        .await
        .unwrap();
    assert!(matches!(
        absent[0].resolution,
        HarnessHydrationResolution::Absent
    ));
    let mut wrong_kind = request;
    wrong_kind.reference.kind = HarnessNativeReferenceKind::Agent;
    let absent = sessions
        .session_harness_native_hydration(&owner, &[wrong_kind])
        .await
        .unwrap();
    assert!(matches!(
        absent[0].resolution,
        HarnessHydrationResolution::Absent
    ));
}

#[tokio::test]
async fn inventory_item_reads_bind_identity_scope_kind_and_retained_revision() {
    use bcode_session_models::{HarnessInventoryKind, HarnessInventoryScope};
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let scope = HarnessInventoryScope::Attempt(execution_attempt());
    let running = execution_page(
        &owner,
        "item-running",
        execution_work(Some(execution_attempt()), HarnessWorkState::Running),
    );
    sessions
        .commit_external_harness_page(&owner, None, &running)
        .await
        .unwrap();
    let page = sessions
        .session_harness_inventory_page(&owner, &scope, HarnessInventoryKind::Work, None)
        .await
        .unwrap();
    let completed = execution_page(
        &owner,
        "item-completed",
        execution_work(Some(execution_attempt()), HarnessWorkState::Completed),
    );
    sessions
        .commit_external_harness_page(&owner, Some("item-running"), &completed)
        .await
        .unwrap();
    let retained = sessions
        .session_harness_inventory_item(
            &owner,
            &scope,
            HarnessInventoryKind::Work,
            "work-1",
            &page.commitment,
        )
        .await
        .unwrap();
    assert_eq!(retained.commitment, page.commitment);
    assert_eq!(
        retained.entry.unwrap().observation,
        page.entries[0].observation
    );
    let absent = sessions
        .session_harness_inventory_item(
            &owner,
            &scope,
            HarnessInventoryKind::Work,
            "missing-work",
            &page.commitment,
        )
        .await
        .unwrap();
    assert!(absent.entry.is_none());
    assert!(
        sessions
            .session_harness_inventory_item(
                &owner,
                &HarnessInventoryScope::Uncorrelated,
                HarnessInventoryKind::Work,
                "work-1",
                &page.commitment
            )
            .await
            .is_err()
    );
    assert!(
        sessions
            .session_harness_inventory_item(
                &owner,
                &scope,
                HarnessInventoryKind::Continuation,
                "work-1",
                &page.commitment
            )
            .await
            .is_err()
    );
    let mut forged = page.commitment.clone();
    forged.summary.identities += 1;
    assert!(
        sessions
            .session_harness_inventory_item(
                &owner,
                &scope,
                HarnessInventoryKind::Work,
                "work-1",
                &forged
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn inventory_pages_retain_more_than_32_uncorrelated_obligations_without_lifetime_cap() {
    use bcode_session_models::{HarnessInventoryKind, HarnessInventoryScope};
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let mut after: Option<String> = None;
    for index in 0..40 {
        let mut work = execution_work(None, HarnessWorkState::Running);
        if let HarnessExecutionEvent::BackgroundWork {
            work_id,
            native_task_id,
            ..
        } = &mut work
        {
            *work_id = format!("unknown-{index}");
            *native_task_id = None;
        }
        let page = execution_page(&owner, &format!("unknown-page-{index}"), work);
        sessions
            .commit_external_harness_page(&owner, after.as_deref(), &page)
            .await
            .unwrap();
        after = page.next_after;
    }
    let first = sessions
        .session_harness_inventory_page(
            &owner,
            &HarnessInventoryScope::Uncorrelated,
            HarnessInventoryKind::Work,
            None,
        )
        .await
        .unwrap();
    assert_eq!(first.commitment.summary.identities, 40);
    assert_eq!(first.commitment.summary.unfinished, 40);
    assert_eq!(first.commitment.summary.obligations, 40);
    let second = sessions
        .session_harness_inventory_page(
            &owner,
            &HarnessInventoryScope::Uncorrelated,
            HarnessInventoryKind::Work,
            first.next_cursor.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(first.entries.len(), 32);
    assert_eq!(second.entries.len(), 8);
    assert!(second.inventory_exhausted);
    assert!(second.next_cursor.is_none());
}

#[tokio::test]
async fn inventory_observed_notification_requires_verified_terminal_continuation_to_discharge() {
    use bcode_session_models::{HarnessInventoryKind, HarnessInventoryScope};
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let mut work = execution_work(None, HarnessWorkState::Failed);
    if let HarnessExecutionEvent::BackgroundWork { notification, .. } = &mut work {
        *notification = HarnessNotificationState::Observed;
    }
    sessions
        .commit_external_harness_page(
            &owner,
            None,
            &execution_page(&owner, "observed-source", work),
        )
        .await
        .unwrap();
    let before = sessions
        .session_harness_inventory_page(
            &owner,
            &HarnessInventoryScope::Uncorrelated,
            HarnessInventoryKind::Work,
            None,
        )
        .await
        .unwrap();
    assert_eq!(before.commitment.summary.obligations, 1);
    assert_eq!(before.commitment.summary.failed, 1);
    let continuation = HarnessExecutionEvent::Continuation {
        continuation_id: "verified-follow-up".into(),
        origin: None,
        source_work_id: Some("work-1".into()),
        native_turn_id: Some("follow-up-turn".into()),
        state: HarnessWorkState::Completed,
        output: Some("follow-up done".into()),
    };
    let linked = execution_page(&owner, "verified-follow-up", continuation);
    sessions
        .commit_external_harness_page(&owner, Some("observed-source"), &linked)
        .await
        .unwrap();
    let after = sessions
        .session_harness_inventory_page(
            &owner,
            &HarnessInventoryScope::Uncorrelated,
            HarnessInventoryKind::Work,
            None,
        )
        .await
        .unwrap();
    assert_eq!(after.commitment.summary.identities, 1);
    // Equal unknown origins are not exact attribution, even with a source-work link.
    assert_eq!(after.commitment.summary.obligations, 1);
    assert_eq!(after.commitment.summary.failed, 1);
    assert_eq!(after.entries[0].observation, before.entries[0].observation);
}

#[tokio::test]
async fn inventory_notification_discharge_requires_exact_origin_and_complete_coverage() {
    use bcode_harness_models::HarnessTrackingCoverage;
    use bcode_session_models::{HarnessInventoryKind, HarnessInventoryScope};
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let scope = HarnessInventoryScope::Attempt(execution_attempt());
    let mut work = execution_work(Some(execution_attempt()), HarnessWorkState::Failed);
    if let HarnessExecutionEvent::BackgroundWork { notification, .. } = &mut work {
        *notification = HarnessNotificationState::Observed;
    }
    sessions
        .commit_external_harness_page(
            &owner,
            None,
            &execution_page(&owner, "exact-source", work.clone()),
        )
        .await
        .unwrap();
    let continuation = HarnessExecutionEvent::Continuation {
        continuation_id: "exact-followup".into(),
        origin: Some(execution_attempt()),
        source_work_id: Some("work-1".into()),
        native_turn_id: Some("exact-followup-turn".into()),
        state: HarnessWorkState::Completed,
        output: Some("done".into()),
    };
    sessions
        .commit_external_harness_page(
            &owner,
            Some("exact-source"),
            &execution_page(&owner, "no-coverage", continuation.clone()),
        )
        .await
        .unwrap();
    let summary = sessions
        .session_harness_inventory_summary(&owner, &scope, HarnessInventoryKind::Work)
        .await
        .unwrap();
    assert_eq!(summary.commitment.summary.obligations, 1);
    let coverage = HarnessExecutionEvent::Coverage {
        attempt: execution_attempt(),
        background: HarnessTrackingCoverage::Complete,
        continuations: HarnessTrackingCoverage::Complete,
    };
    sessions
        .commit_external_harness_page(
            &owner,
            Some("no-coverage"),
            &execution_page(&owner, "coverage-only", coverage),
        )
        .await
        .unwrap();
    let summary = sessions
        .session_harness_inventory_summary(&owner, &scope, HarnessInventoryKind::Work)
        .await
        .unwrap();
    assert_eq!(summary.commitment.summary.obligations, 0);
    sessions
        .commit_external_harness_page(
            &owner,
            Some("coverage-only"),
            &execution_page(&owner, "verified-chain", continuation),
        )
        .await
        .unwrap();
    let summary = sessions
        .session_harness_inventory_summary(&owner, &scope, HarnessInventoryKind::Work)
        .await
        .unwrap();
    assert_eq!(summary.commitment.summary.obligations, 0);
    assert_eq!(summary.commitment.summary.failed, 1);
    let page = sessions
        .session_harness_inventory_page(&owner, &scope, HarnessInventoryKind::Work, None)
        .await
        .unwrap();
    assert_eq!(page.commitment, summary.commitment);
    assert_eq!(
        page.observed_through_sequence,
        summary.observed_through_sequence
    );
    sessions
        .commit_external_harness_page(
            &owner,
            Some("verified-chain"),
            &execution_page(&owner, "duplicate-source", work),
        )
        .await
        .unwrap();
    let summary = sessions
        .session_harness_inventory_summary(&owner, &scope, HarnessInventoryKind::Work)
        .await
        .unwrap();
    assert_eq!(summary.commitment.summary.obligations, 0);
    assert_eq!(summary.commitment.summary.failed, 1);
}

#[tokio::test]
async fn inventory_notification_discharge_is_independent_of_evidence_order() {
    use bcode_harness_models::HarnessTrackingCoverage;
    use bcode_session_models::{HarnessInventoryKind, HarnessInventoryScope};

    // Each permutation supplies exact, complete evidence once. Neither coverage
    // nor source notification arriving last may require continuation replay.
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let root = tempfile::tempdir().unwrap();
        let (sessions, owner) = execution_fixture(root.path()).await;
        let scope = HarnessInventoryScope::Attempt(execution_attempt());
        let work = execution_work(Some(execution_attempt()), HarnessWorkState::Failed);
        sessions
            .commit_external_harness_page(
                &owner,
                None,
                &execution_page(&owner, "pending-source", work.clone()),
            )
            .await
            .unwrap();
        let pending = sessions
            .session_harness_inventory_page(&owner, &scope, HarnessInventoryKind::Work, None)
            .await
            .unwrap();
        assert_eq!(pending.commitment.summary.obligations, 1);
        let mut observed = work;
        if let HarnessExecutionEvent::BackgroundWork { notification, .. } = &mut observed {
            *notification = HarnessNotificationState::Observed;
        }
        let continuation = HarnessExecutionEvent::Continuation {
            continuation_id: "ordered-followup".into(),
            origin: Some(execution_attempt()),
            source_work_id: Some("work-1".into()),
            native_turn_id: Some("ordered-followup-turn".into()),
            state: HarnessWorkState::Completed,
            output: Some("done".into()),
        };
        let evidence = [
            ("observed-source", observed),
            (
                "complete-coverage",
                HarnessExecutionEvent::Coverage {
                    attempt: execution_attempt(),
                    background: HarnessTrackingCoverage::Complete,
                    continuations: HarnessTrackingCoverage::Complete,
                },
            ),
            ("terminal-continuation", continuation.clone()),
        ];
        let mut after = "pending-source";
        for index in order {
            let (id, event) = &evidence[index];
            sessions
                .commit_external_harness_page(
                    &owner,
                    Some(after),
                    &execution_page(&owner, id, event.clone()),
                )
                .await
                .unwrap();
            after = id;
        }
        let summary = sessions
            .session_harness_inventory_summary(&owner, &scope, HarnessInventoryKind::Work)
            .await
            .unwrap();
        assert_eq!(
            summary.commitment.summary.obligations, 0,
            "arrival order: {order:?}"
        );
        assert_eq!(summary.commitment.summary.identities, 1);
        assert_eq!(summary.commitment.summary.failed, 1);
        assert_eq!(summary.commitment.summary.unfinished, 0);
        let page = sessions
            .session_harness_inventory_page(&owner, &scope, HarnessInventoryKind::Work, None)
            .await
            .unwrap();
        assert_eq!(page.commitment, summary.commitment);
        assert_eq!(
            page.observed_through_sequence,
            summary.observed_through_sequence
        );

        sessions
            .commit_external_harness_page(
                &owner,
                Some(after),
                &execution_page(&owner, "repeated-continuation", continuation),
            )
            .await
            .unwrap();
        let summary = sessions
            .session_harness_inventory_summary(&owner, &scope, HarnessInventoryKind::Work)
            .await
            .unwrap();
        assert_eq!(summary.commitment.summary.obligations, 0);
        assert_eq!(summary.commitment.summary.failed, 1);
        let pinned_cursor = bcode_session_models::HarnessInventoryCursor {
            owner: owner.clone(),
            commitment: pending.commitment.clone(),
            offset: 0,
        };
        let pinned = sessions
            .session_harness_inventory_page(
                &owner,
                &scope,
                HarnessInventoryKind::Work,
                Some(&pinned_cursor),
            )
            .await
            .unwrap();
        assert_eq!(pinned.commitment, pending.commitment);
        assert_eq!(pinned.entries, pending.entries);
        let item = sessions
            .session_harness_inventory_item(
                &owner,
                &scope,
                HarnessInventoryKind::Work,
                "work-1",
                &summary.commitment,
            )
            .await
            .unwrap();
        assert_eq!(
            item.entry.as_ref().unwrap().observation,
            page.entries[0].observation
        );

        // Damage each private dependency in separate permutations. Historical
        // pins remain readable when only a later gate or witness is removed.
        sessions
            .release_idle_session_resources(owner.session_id)
            .await
            .unwrap();
        let db = bcode_session::db::SessionDb::open_existing_turso_in_root(
            owner.session_id,
            root.path(),
        )
        .await
        .unwrap();
        use switchy::database::query::FilterableQuery;
        let root_name = format!(
            "harness_inventory_node_v1:{}",
            summary.commitment.root.as_ref().unwrap()
        );
        let row = db
            .database()
            .select("snapshots")
            .columns(&["payload"])
            .where_eq("snapshot_name", root_name.clone())
            .execute_first(db.database())
            .await
            .unwrap()
            .unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(row.get("payload").unwrap().as_str().unwrap()).unwrap();
        if order[0] == 2 {
            let origin = execution_attempt();
            let attempt_key = format!(
                "{}:{}:{}",
                origin.submission_id.len(),
                origin.submission_id,
                origin.attempt_id
            );
            let (namespace, identity, sequence) = if order[1] == 0 {
                (
                    format!(
                        "execution_v3:{}:inventory:attempt:{attempt_key}:coverage",
                        owner.generation
                    ),
                    attempt_key,
                    payload["gate"]["coverage"].as_u64().unwrap(),
                )
            } else {
                let reverse_name = format!(
                    "harness_inventory_node_v1:{}",
                    payload["gate"]["reverse"]["hash"].as_str().unwrap()
                );
                let reverse_row = db
                    .database()
                    .select("snapshots")
                    .columns(&["payload"])
                    .where_eq("snapshot_name", reverse_name)
                    .execute_first(db.database())
                    .await
                    .unwrap()
                    .unwrap();
                let reverse: serde_json::Value =
                    serde_json::from_str(reverse_row.get("payload").unwrap().as_str().unwrap())
                        .unwrap();
                (
                    format!(
                        "execution_v3:{}:inventory:attempt:{attempt_key}:notification_reverse_v1",
                        owner.generation
                    ),
                    "work-1".into(),
                    reverse["sequence"].as_u64().unwrap(),
                )
            };
            let key =
                bcode_session_models::external_backend::external_backend_key(&namespace, &identity);
            let version = bcode_session_models::external_backend::external_backend_key(
                "inventory_member_v1",
                &format!("{}:{key}:{sequence}", key.len()),
            );
            db.database()
                .delete("turn_evidence")
                .where_eq("turn_id", version)
                .execute(db.database())
                .await
                .unwrap();
        }
        let damaged = match order[0] {
            0 if order[1] == 1 => root_name,
            0 => format!(
                "harness_inventory_node_v1:{}",
                payload["gate"]["eligible"]["hash"].as_str().unwrap()
            ),
            1 => format!(
                "harness_inventory_node_v1:{}",
                payload["gate"]["reverse"]["hash"].as_str().unwrap()
            ),
            _ => "no-such-proof-node".into(),
        };
        db.database()
            .delete("snapshots")
            .where_eq("snapshot_name", damaged)
            .execute(db.database())
            .await
            .unwrap();
        assert!(
            db.harness_inventory_summary(&owner, &scope, HarnessInventoryKind::Work)
                .await
                .is_err()
        );
        assert!(
            db.harness_inventory_page(&owner, &scope, HarnessInventoryKind::Work, None)
                .await
                .is_err()
        );
        assert!(
            db.harness_inventory_item(
                &owner,
                &scope,
                HarnessInventoryKind::Work,
                "work-1",
                &summary.commitment
            )
            .await
            .is_err()
        );
        let historical = db
            .harness_inventory_page(
                &owner,
                &scope,
                HarnessInventoryKind::Work,
                Some(&pinned_cursor),
            )
            .await
            .unwrap();
        assert_eq!(historical.commitment, pending.commitment);
        assert_eq!(historical.entries, pending.entries);
    }
}

#[tokio::test]
async fn inventory_notification_gate_requires_complete_pages_and_observations() {
    use bcode_harness_models::HarnessTrackingCoverage;
    use bcode_session_models::{HarnessInventoryKind, HarnessInventoryScope};
    for damaged_event in 0..3 {
        for partial_page in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (sessions, owner) = execution_fixture(root.path()).await;
            let scope = HarnessInventoryScope::Attempt(execution_attempt());
            let mut work = execution_work(Some(execution_attempt()), HarnessWorkState::Failed);
            if let HarnessExecutionEvent::BackgroundWork { notification, .. } = &mut work {
                *notification = HarnessNotificationState::Observed;
            }
            let events = [
                work,
                HarnessExecutionEvent::Coverage {
                    attempt: execution_attempt(),
                    background: HarnessTrackingCoverage::Complete,
                    continuations: HarnessTrackingCoverage::Complete,
                },
                HarnessExecutionEvent::Continuation {
                    continuation_id: "complete-followup".into(),
                    origin: Some(execution_attempt()),
                    source_work_id: Some("work-1".into()),
                    native_turn_id: None,
                    state: HarnessWorkState::Completed,
                    output: None,
                },
            ];
            let ids = ["partial-source", "partial-coverage", "partial-continuation"];
            let mut after = None;
            for (index, event) in events.iter().enumerate() {
                let mut page = execution_page(&owner, ids[index], event.clone());
                if index == damaged_event {
                    if partial_page {
                        page.completeness = HarnessCompleteness::Partial;
                    } else {
                        page.observations[0].completeness = HarnessCompleteness::Partial;
                    }
                }
                sessions
                    .commit_external_harness_page(&owner, after, &page)
                    .await
                    .unwrap();
                after = Some(ids[index]);
            }
            let summary = sessions
                .session_harness_inventory_summary(&owner, &scope, HarnessInventoryKind::Work)
                .await
                .unwrap();
            assert_eq!(summary.commitment.summary.obligations, 1);
            let page = sessions
                .session_harness_inventory_page(&owner, &scope, HarnessInventoryKind::Work, None)
                .await
                .unwrap();
            assert_eq!(page.commitment, summary.commitment);
            sessions
                .commit_external_harness_page(
                    &owner,
                    after,
                    &execution_page(
                        &owner,
                        "complete-replacement",
                        events[damaged_event].clone(),
                    ),
                )
                .await
                .unwrap();
            let summary = sessions
                .session_harness_inventory_summary(&owner, &scope, HarnessInventoryKind::Work)
                .await
                .unwrap();
            assert_eq!(summary.commitment.summary.obligations, 0);
            assert_eq!(summary.commitment.summary.failed, 1);
        }
    }
}

#[tokio::test]
async fn inventory_notification_gate_deduplicates_sources_and_pins_coverage() {
    use bcode_harness_models::HarnessTrackingCoverage;
    use bcode_session_models::{HarnessInventoryKind, HarnessInventoryScope};
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let scope = HarnessInventoryScope::Attempt(execution_attempt());
    let mut work = execution_work(Some(execution_attempt()), HarnessWorkState::Failed);
    if let HarnessExecutionEvent::BackgroundWork { notification, .. } = &mut work {
        *notification = HarnessNotificationState::Observed;
    }
    let coverage = HarnessExecutionEvent::Coverage {
        attempt: execution_attempt(),
        background: HarnessTrackingCoverage::Complete,
        continuations: HarnessTrackingCoverage::Complete,
    };
    sessions
        .commit_external_harness_page(&owner, None, &execution_page(&owner, "dedup-source", work))
        .await
        .unwrap();
    sessions
        .commit_external_harness_page(
            &owner,
            Some("dedup-source"),
            &execution_page(&owner, "dedup-coverage", coverage.clone()),
        )
        .await
        .unwrap();
    let mut after = "dedup-coverage";
    for id in ["dedup-followup-a", "dedup-followup-b"] {
        let continuation = HarnessExecutionEvent::Continuation {
            continuation_id: id.into(),
            origin: Some(execution_attempt()),
            source_work_id: Some("work-1".into()),
            native_turn_id: None,
            state: HarnessWorkState::Completed,
            output: None,
        };
        sessions
            .commit_external_harness_page(
                &owner,
                Some(after),
                &execution_page(&owner, id, continuation),
            )
            .await
            .unwrap();
        after = id;
        let summary = sessions
            .session_harness_inventory_summary(&owner, &scope, HarnessInventoryKind::Work)
            .await
            .unwrap();
        assert_eq!(summary.commitment.summary.identities, 1);
        assert_eq!(summary.commitment.summary.obligations, 0);
        assert_eq!(summary.commitment.summary.failed, 1);
    }
    let settled = sessions
        .session_harness_inventory_page(&owner, &scope, HarnessInventoryKind::Work, None)
        .await
        .unwrap();
    let mut partial = execution_page(&owner, "coverage-revoked", coverage.clone());
    partial.observations[0].completeness = HarnessCompleteness::Partial;
    sessions
        .commit_external_harness_page(&owner, Some(after), &partial)
        .await
        .unwrap();
    let current = sessions
        .session_harness_inventory_summary(&owner, &scope, HarnessInventoryKind::Work)
        .await
        .unwrap();
    assert_eq!(current.commitment.summary.obligations, 1);
    let cursor = bcode_session_models::HarnessInventoryCursor {
        owner: owner.clone(),
        commitment: settled.commitment.clone(),
        offset: 0,
    };
    let pinned = sessions
        .session_harness_inventory_page(&owner, &scope, HarnessInventoryKind::Work, Some(&cursor))
        .await
        .unwrap();
    assert_eq!(pinned.commitment, settled.commitment);
    assert_eq!(pinned.entries, settled.entries);
    let item = sessions
        .session_harness_inventory_item(
            &owner,
            &scope,
            HarnessInventoryKind::Work,
            "work-1",
            &settled.commitment,
        )
        .await
        .unwrap();
    assert_eq!(
        item.entry.as_ref().unwrap().observation,
        settled.entries[0].observation
    );
    sessions
        .commit_external_harness_page(
            &owner,
            Some("coverage-revoked"),
            &execution_page(&owner, "coverage-restored", coverage),
        )
        .await
        .unwrap();
    let summary = sessions
        .session_harness_inventory_summary(&owner, &scope, HarnessInventoryKind::Work)
        .await
        .unwrap();
    assert_eq!(summary.commitment.summary.obligations, 0);
    assert_eq!(summary.commitment.summary.failed, 1);
}

#[tokio::test]
async fn inventory_pages_pin_more_than_1024_same_attempt_identities_across_restart_and_damage() {
    use bcode_session_models::{HarnessInventoryKind, HarnessInventoryScope};
    use switchy::database::query::FilterableQuery;
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let mut after = None;
    for index in 0..1030 {
        let mut work = execution_work(Some(execution_attempt()), HarnessWorkState::Running);
        if let HarnessExecutionEvent::BackgroundWork {
            work_id,
            native_task_id,
            notification,
            state,
            ..
        } = &mut work
        {
            *work_id = format!("paged-work-{index}");
            *native_task_id = None;
            *notification = HarnessNotificationState::NotExpected;
            if index == 0 {
                *state = HarnessWorkState::Failed;
            }
        }
        let observation_id = format!("paged-observation-{index}");
        let page = execution_page(&owner, &observation_id, work);
        sessions
            .commit_external_harness_page(&owner, after.as_deref(), &page)
            .await
            .unwrap();
        after = page.next_after;
    }
    let scope = HarnessInventoryScope::Attempt(execution_attempt());
    let first = sessions
        .session_harness_inventory_page(&owner, &scope, HarnessInventoryKind::Work, None)
        .await
        .unwrap();
    let summary = sessions
        .session_harness_inventory_summary(&owner, &scope, HarnessInventoryKind::Work)
        .await
        .unwrap();
    assert_eq!(summary.commitment, first.commitment);
    assert_eq!(
        summary.observed_through_sequence,
        first.observed_through_sequence
    );
    assert_eq!(summary.commitment.summary.identities, 1030);
    assert_eq!(first.entries.len(), 32);
    assert_eq!(first.commitment.summary.identities, 1030);
    assert_eq!(first.commitment.summary.failed, 1);
    assert_eq!(first.commitment.summary.unfinished, 1029);
    let mut update = execution_work(Some(execution_attempt()), HarnessWorkState::Completed);
    if let HarnessExecutionEvent::BackgroundWork {
        work_id,
        native_task_id,
        notification,
        ..
    } = &mut update
    {
        *work_id = "paged-work-500".into();
        *native_task_id = None;
        *notification = HarnessNotificationState::NotExpected;
    }
    let page = execution_page(&owner, "paged-concurrent-update", update);
    sessions
        .commit_external_harness_page(&owner, after.as_deref(), &page)
        .await
        .unwrap();
    sessions
        .release_idle_session_resources(owner.session_id)
        .await
        .unwrap();
    let db =
        bcode_session::db::SessionDb::open_existing_turso_in_root(owner.session_id, root.path())
            .await
            .unwrap();
    let mut cursor = first.next_cursor;
    let mut entries = first.entries;
    while let Some(position) = cursor {
        let next = db
            .harness_inventory_page(&owner, &scope, HarnessInventoryKind::Work, Some(&position))
            .await
            .unwrap();
        assert_eq!(next.commitment, first.commitment);
        assert!(next.entries.len() <= 32);
        entries.extend(next.entries);
        cursor = next.next_cursor;
    }
    assert_eq!(entries.len(), 1030);
    assert_eq!(
        entries
            .iter()
            .map(|entry| &entry.key)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1030
    );
    let current = db
        .harness_inventory_page(&owner, &scope, HarnessInventoryKind::Work, None)
        .await
        .unwrap();
    assert_eq!(current.commitment.summary.unfinished, 1028);
    assert_eq!(current.commitment.summary.failed, 1);
    let middle_cursor = bcode_session_models::HarnessInventoryCursor {
        owner: owner.clone(),
        commitment: first.commitment.clone(),
        offset: 512,
    };
    let middle_page = db
        .harness_inventory_page(
            &owner,
            &scope,
            HarnessInventoryKind::Work,
            Some(&middle_cursor),
        )
        .await
        .unwrap();
    let middle = &middle_page.entries[0];
    let version = bcode_session_models::external_backend::external_backend_key(
        "inventory_member_v1",
        &format!(
            "{}:{}:{}",
            middle.key.len(),
            middle.key,
            middle.event_sequence
        ),
    );
    db.database()
        .delete("turn_evidence")
        .where_eq("turn_id", version)
        .execute(db.database())
        .await
        .unwrap();
    assert!(
        db.harness_inventory_page(
            &owner,
            &scope,
            HarnessInventoryKind::Work,
            Some(&middle_cursor)
        )
        .await
        .is_err()
    );
    let mut foreign = middle_cursor.clone();
    foreign.owner.generation += 1;
    assert!(
        db.harness_inventory_page(&owner, &scope, HarnessInventoryKind::Work, Some(&foreign))
            .await
            .is_err()
    );
    let damaged = &current.entries[0];
    db.database()
        .delete("turn_evidence")
        .where_eq("turn_id", damaged.key.clone())
        .where_eq("event_seq", i64::try_from(damaged.event_sequence).unwrap())
        .execute(db.database())
        .await
        .unwrap();
    assert!(
        db.harness_inventory_page(&owner, &scope, HarnessInventoryKind::Work, None)
            .await
            .is_err()
    );
    let name = format!(
        "harness_inventory_node_v1:{}",
        first.commitment.root.as_ref().unwrap()
    );
    db.database()
        .delete("snapshots")
        .where_eq("snapshot_name", name)
        .execute(db.database())
        .await
        .unwrap();
    let forged = bcode_session_models::HarnessInventoryCursor {
        owner: owner.clone(),
        commitment: first.commitment,
        offset: 32,
    };
    assert!(
        db.harness_inventory_page(&owner, &scope, HarnessInventoryKind::Work, Some(&forged))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn execution_evidence_survives_long_prefix_empty_snapshot_and_process_loss() {
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let mut cursor = None;
    for batch in 0..9 {
        let mut prefix = page(&owner);
        prefix.next_after = Some(format!("execution-prefix-{batch}"));
        let template = prefix.observations[0].clone();
        prefix.observations = (0..128)
            .map(|offset| {
                let mut observation = template.clone();
                observation.observation_id = format!("execution-prefix-{batch}-{offset}");
                observation.native_correlations.clear();
                observation
            })
            .collect();
        sessions
            .commit_external_harness_page(&owner, cursor.as_deref(), &prefix)
            .await
            .unwrap();
        cursor = prefix.next_after;
    }
    let terminal = execution_page(&owner, "foreground", execution_terminal());
    sessions
        .commit_external_harness_page(&owner, cursor.as_deref(), &terminal)
        .await
        .unwrap();
    let work = execution_page(
        &owner,
        "work-running",
        execution_work(None, HarnessWorkState::Running),
    );
    sessions
        .commit_external_harness_page(&owner, Some("foreground"), &work)
        .await
        .unwrap();
    let coverage = execution_page(
        &owner,
        "coverage",
        HarnessExecutionEvent::Coverage {
            attempt: execution_attempt(),
            background: HarnessTrackingCoverage::Complete,
            continuations: HarnessTrackingCoverage::Complete,
        },
    );
    sessions
        .commit_external_harness_page(&owner, Some("work-running"), &coverage)
        .await
        .unwrap();
    let mut empty = page(&owner);
    empty.observations.clear();
    empty.next_after = Some("empty".into());
    sessions
        .commit_external_harness_page(&owner, Some("coverage"), &empty)
        .await
        .unwrap();
    let unknown = execution_page(
        &owner,
        "process-lost",
        execution_work(None, HarnessWorkState::Unknown),
    );
    sessions
        .commit_external_harness_page(&owner, Some("empty"), &unknown)
        .await
        .unwrap();
    let foreign = manager(root.path());
    let (actual_owner, evidence) = foreign
        .session_external_harness_submission_evidence(
            owner.session_id,
            "execution-input",
            "execution-attempt",
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual_owner, owner);
    let events: Vec<_> = evidence
        .observations
        .iter()
        .filter_map(|observation| match &observation.kind {
            HarnessObservationKind::Execution { event } => Some(event),
            _ => None,
        })
        .collect();
    assert_eq!(events.len(), 3);
    assert!(events.contains(&&execution_terminal()));
    assert!(events.iter().any(|event| matches!(
        event,
        HarnessExecutionEvent::BackgroundWork {
            origin: None,
            state: HarnessWorkState::Unknown,
            notification: HarnessNotificationState::Pending,
            ..
        }
    )));
    assert!(
        foreign
            .session_external_harness_submission_acceptance(
                owner.session_id,
                "execution-input",
                "execution-attempt"
            )
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn execution_rejects_forged_origins_terminal_overwrite_and_reused_work() {
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let mut forged = execution_attempt();
    forged.attempt_id = "unretained".into();
    let invalid = execution_page(
        &owner,
        "forged",
        execution_work(Some(forged), HarnessWorkState::Running),
    );
    assert!(
        sessions
            .commit_external_harness_page(&owner, None, &invalid)
            .await
            .is_err()
    );
    let terminal = execution_page(&owner, "terminal", execution_terminal());
    sessions
        .commit_external_harness_page(&owner, None, &terminal)
        .await
        .unwrap();
    // Exact retained poll retry remains idempotent; a late conflicting outcome does not.
    sessions
        .commit_external_harness_page(&owner, None, &terminal)
        .await
        .unwrap();
    let duplicate = execution_page(&owner, "duplicate-terminal", execution_terminal());
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("terminal"), &duplicate)
            .await
            .is_err()
    );
    let mut changed = execution_terminal();
    if let HarnessExecutionEvent::SubmissionTerminal { disposition, .. } = &mut changed {
        *disposition = HarnessTerminalDisposition::Failed;
    }
    let invalid = execution_page(&owner, "late-terminal", changed);
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("terminal"), &invalid)
            .await
            .is_err()
    );
    let completed = execution_page(
        &owner,
        "work-completed",
        execution_work(Some(execution_attempt()), HarnessWorkState::Completed),
    );
    sessions
        .commit_external_harness_page(&owner, Some("terminal"), &completed)
        .await
        .unwrap();
    let mut reused = execution_work(Some(execution_attempt()), HarnessWorkState::Running);
    if let HarnessExecutionEvent::BackgroundWork { work_id, .. } = &mut reused {
        *work_id = "alias".into();
    }
    let reused = execution_page(&owner, "native-task-reused", reused);
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("work-completed"), &reused)
            .await
            .is_err()
    );
    let reopened = execution_page(
        &owner,
        "work-reopened",
        execution_work(Some(execution_attempt()), HarnessWorkState::Running),
    );
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("work-completed"), &reopened)
            .await
            .is_err()
    );
    let uncorrelated = execution_page(
        &owner,
        "work-rebound",
        execution_work(None, HarnessWorkState::Completed),
    );
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("work-completed"), &uncorrelated)
            .await
            .is_err()
    );
    let invalid = execution_page(
        &owner,
        "forged-continuation",
        HarnessExecutionEvent::Continuation {
            continuation_id: "followup".into(),
            origin: None,
            source_work_id: Some("work-1".into()),
            native_turn_id: None,
            state: HarnessWorkState::Pending,
            output: None,
        },
    );
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("work-completed"), &invalid)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn execution_monotonic_enrichment_preserves_identity_and_rejects_reassignment() {
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let mut roster = execution_work(None, HarnessWorkState::Running);
    if let HarnessExecutionEvent::BackgroundWork { work_kind, .. } = &mut roster {
        *work_kind = HarnessWorkKind::Other;
    }
    let roster = execution_page(&owner, "roster", roster);
    sessions
        .commit_external_harness_page(&owner, None, &roster)
        .await
        .unwrap();
    let enriched = execution_page(
        &owner,
        "enriched",
        execution_work(Some(execution_attempt()), HarnessWorkState::Running),
    );
    sessions
        .commit_external_harness_page(&owner, Some("roster"), &enriched)
        .await
        .unwrap();
    let evidence = sessions
        .session_external_harness_submission_evidence(
            owner.session_id,
            "execution-input",
            "execution-attempt",
        )
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(evidence.observations.len(), 1);
    assert_eq!(evidence.observations[0], enriched.observations[0]);
    let mut downgraded = execution_work(Some(execution_attempt()), HarnessWorkState::Running);
    if let HarnessExecutionEvent::BackgroundWork { work_kind, .. } = &mut downgraded {
        *work_kind = HarnessWorkKind::Other;
    }
    let downgraded = execution_page(&owner, "downgraded", downgraded);
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("enriched"), &downgraded)
            .await
            .is_err()
    );
    let reassigned = execution_page(
        &owner,
        "reassigned",
        execution_work(None, HarnessWorkState::Running),
    );
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("enriched"), &reassigned)
            .await
            .is_err()
    );
}

fn record_execution_phase(
    phases: &mut std::collections::BTreeMap<&'static str, (u64, std::time::Duration)>,
    phase: &'static str,
    elapsed: std::time::Duration,
) {
    let entry = phases.entry(phase).or_default();
    entry.0 += 1;
    entry.1 += elapsed;
}

#[tokio::test]
async fn execution_many_completed_origins_remain_exactly_readable_without_global_cap() {
    let root = tempfile::tempdir().unwrap();
    let metrics = bcode_metrics::MetricsRegistry::in_memory();
    let sessions = SessionManager::persistent_with_metrics_and_lease_owner(
        root.path(),
        metrics.clone(),
        SessionLeaseOwnerContext {
            build_fingerprint: Some("artifact".into()),
            daemon_instance_id: Some("daemon".into()),
            ..SessionLeaseOwnerContext::default()
        },
    )
    .unwrap();
    let (sessions, owner) = execution_fixture_with_manager(root.path(), sessions).await;
    let mut phases = std::collections::BTreeMap::<&str, (u64, std::time::Duration)>::new();
    let binding = HarnessSessionBinding {
        schema_version: HARNESS_SCHEMA_VERSION,
        session_id: owner.session_id.to_string(),
        instance_id: "fixture".into(),
        native_session_id: "native-confirmed".into(),
        native_version: "fixture-v1".into(),
        generation: owner.generation,
    };
    let mut confirmation = page(&owner);
    confirmation.next_after = Some("confirmed".into());
    confirmation.observations[0].observation_id = "confirmed".into();
    confirmation.observations[0].native_correlations.clear();
    confirmation.observations[0].kind = HarnessObservationKind::BindingConfirmed {
        open_id: "open".into(),
        binding: binding.clone(),
    };
    sessions
        .commit_external_harness_page(&owner, None, &confirmation)
        .await
        .unwrap();
    let mut cursor = "confirmed".to_owned();
    for index in 0..1030 {
        let attempt = HarnessAttemptIdentity {
            submission_id: format!("sequential-{index}"),
            attempt_id: format!("attempt-{index}"),
        };
        let request = HarnessRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: format!("submit-{index}"),
            instance: instance(),
            kind: HarnessRequestKind::Submit {
                target: HarnessSessionTarget::Confirmed(binding.clone()),
                submission: HarnessSubmission {
                    submission_id: attempt.submission_id.clone(),
                    attempt_id: attempt.attempt_id.clone(),
                    session_id: owner.session_id.to_string(),
                    generation: owner.generation,
                    text: "sequential".into(),
                    placement: HarnessPlacement::NextTurn,
                },
            },
        };
        let phase_started = std::time::Instant::now();
        sessions
            .claim_external_harness_request(&owner, &request)
            .await
            .unwrap();
        record_execution_phase(&mut phases, "claim", phase_started.elapsed());
        // First uncorrelated roster, then verified enrichment and explicit retirement.
        let mut running = execution_work(None, HarnessWorkState::Running);
        if let HarnessExecutionEvent::BackgroundWork {
            work_id,
            native_task_id,
            ..
        } = &mut running
        {
            *work_id = format!("work-{index}");
            *native_task_id = Some(format!("task-{index}"));
        }
        let next = format!("roster-{index}");
        let phase_started = std::time::Instant::now();
        sessions
            .commit_external_harness_page(
                &owner,
                Some(&cursor),
                &execution_page(&owner, &next, running.clone()),
            )
            .await
            .unwrap();
        record_execution_phase(&mut phases, "roster", phase_started.elapsed());
        cursor = next;
        if let HarnessExecutionEvent::BackgroundWork {
            origin,
            state,
            notification,
            ..
        } = &mut running
        {
            *origin = Some(attempt.clone());
            *state = HarnessWorkState::Completed;
            *notification = HarnessNotificationState::NotExpected;
        }
        let next = format!("completed-{index}");
        let phase_started = std::time::Instant::now();
        sessions
            .commit_external_harness_page(
                &owner,
                Some(&cursor),
                &execution_page(&owner, &next, running),
            )
            .await
            .unwrap();
        record_execution_phase(&mut phases, "completion", phase_started.elapsed());
        cursor = next;
        let terminal = HarnessExecutionEvent::SubmissionTerminal {
            attempt,
            native_turn_id: None,
            native_frame_id: Some(format!("result-{index}")),
            disposition: HarnessTerminalDisposition::Completed,
            output: Some(format!("output-{index}")),
        };
        let next = format!("terminal-{index}");
        let phase_started = std::time::Instant::now();
        sessions
            .commit_external_harness_page(
                &owner,
                Some(&cursor),
                &execution_page(&owner, &next, terminal),
            )
            .await
            .unwrap();
        record_execution_phase(&mut phases, "terminal", phase_started.elapsed());
        cursor = next;
        if index % 64 == 0 || index == 1029 {
            let snapshot = metrics.snapshot();
            let selected: Vec<_> = [
                "database.operation.duration_ms",
                "session.actor.append_event.db_transaction_ms",
                "session.actor.append_event.manifest_duration_ms",
                "session.actor.append_event.duration_ms",
            ]
            .into_iter()
            .filter_map(|name| {
                snapshot
                    .histograms
                    .get(name)
                    .map(|value| (name, value.count, value.sum, value.max))
            })
            .collect();
            let db_path = bcode_session::db::session_db_path(root.path(), owner.session_id);
            let physical: Vec<_> = [
                db_path.clone(),
                db_path.with_file_name(format!(
                    "{}-wal",
                    db_path.file_name().unwrap().to_string_lossy()
                )),
            ]
            .iter()
            .map(|path| {
                (
                    path.extension().map(|v| v.to_string_lossy().into_owned()),
                    std::fs::metadata(path)
                        .map(|metadata| metadata.len())
                        .unwrap_or(0),
                )
            })
            .collect();
            let physical_operations: Vec<_> = snapshot
                .histograms
                .iter()
                .filter(|(name, _)| name.starts_with("database.physical."))
                .map(|(name, value)| (name, value.count, value.sum, value.max))
                .collect();
            eprintln!(
                "execution origins completed={} phases={phases:?} selected_count_sum_max={selected:?} physical_bytes={physical:?} db_operations={:?} physical_operations_count_sum_max={physical_operations:?}",
                index + 1,
                snapshot.counters.get("database.operation.total")
            );
        }
    }
    for phase in ["claim", "roster", "completion", "terminal"] {
        assert_eq!(phases[phase].0, 1030, "missing production phase {phase}");
    }
    for index in [0, 1029] {
        let evidence = sessions
            .session_external_harness_submission_evidence(
                owner.session_id,
                &format!("sequential-{index}"),
                &format!("attempt-{index}"),
            )
            .await
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(evidence.observations.len(), 2);
        assert!(evidence.observations.iter().any(|v| matches!(&v.kind,
            HarnessObservationKind::Execution { event: HarnessExecutionEvent::SubmissionTerminal { output: Some(output), .. } }
                if output == &format!("output-{index}"))));
    }
}

#[tokio::test]
async fn execution_uncorrelated_retirement_requires_terminal_and_no_notification_debt() {
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let terminal = execution_page(&owner, "terminal", execution_terminal());
    sessions
        .commit_external_harness_page(&owner, None, &terminal)
        .await
        .unwrap();
    let mut cursor = "terminal".to_owned();
    for index in 0..40 {
        let mut event = execution_work(None, HarnessWorkState::Completed);
        if let HarnessExecutionEvent::BackgroundWork {
            work_id,
            native_task_id,
            notification,
            ..
        } = &mut event
        {
            *work_id = format!("retired-{index}");
            *native_task_id = Some(format!("retired-native-{index}"));
            *notification = HarnessNotificationState::NotExpected;
        }
        let next = format!("retired-{index}");
        sessions
            .commit_external_harness_page(
                &owner,
                Some(&cursor),
                &execution_page(&owner, &next, event),
            )
            .await
            .unwrap();
        cursor = next;
    }
    let pending = execution_page(
        &owner,
        "pending-notification",
        execution_work(None, HarnessWorkState::Completed),
    );
    sessions
        .commit_external_harness_page(&owner, Some(&cursor), &pending)
        .await
        .unwrap();
    let mut observed = execution_work(None, HarnessWorkState::Completed);
    if let HarnessExecutionEvent::BackgroundWork { notification, .. } = &mut observed {
        *notification = HarnessNotificationState::Observed;
    }
    let observed = execution_page(&owner, "observed-notification", observed);
    sessions
        .commit_external_harness_page(&owner, Some("pending-notification"), &observed)
        .await
        .unwrap();
    let evidence = sessions
        .session_external_harness_submission_evidence(
            owner.session_id,
            "execution-input",
            "execution-attempt",
        )
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(evidence.observations.len(), 2);
    assert!(evidence.observations.iter().any(|v| matches!(
        &v.kind,
        HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::BackgroundWork {
                notification: HarnessNotificationState::Observed,
                ..
            }
        }
    )));
}

#[tokio::test]
async fn execution_current_inventory_corruption_fails_without_replay() {
    use bcode_session::db::SessionDb;
    use switchy::database::query::FilterableQuery;
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let terminal = execution_page(&owner, "terminal", execution_terminal());
    sessions
        .commit_external_harness_page(&owner, None, &terminal)
        .await
        .unwrap();
    let pending = execution_page(
        &owner,
        "pending",
        execution_work(None, HarnessWorkState::Running),
    );
    sessions
        .commit_external_harness_page(&owner, Some("terminal"), &pending)
        .await
        .unwrap();
    sessions
        .release_idle_session_resources(owner.session_id)
        .await
        .unwrap();
    let db = SessionDb::open_existing_turso_in_root(owner.session_id, root.path())
        .await
        .unwrap();
    let tail = db.last_event_sequence().await.unwrap();
    let namespace = format!("execution_v3:{}:inventory:unknown:work", owner.generation);
    let key = bcode_session_models::external_backend::external_backend_key(&namespace, "work-1");
    db.database()
        .delete("turn_evidence")
        .where_eq("evidence_kind", "harness_operation_v1")
        .where_eq("turn_id", key.clone())
        .execute(db.database())
        .await
        .unwrap();
    assert!(
        db.external_harness_submission_evidence("execution-input", "execution-attempt")
            .await
            .is_err()
    );
    assert_eq!(db.last_event_sequence().await.unwrap(), tail);
    assert!(
        db.database()
            .select("turn_evidence")
            .columns(&["event_seq"])
            .where_eq("turn_id", key)
            .execute(db.database())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn execution_batched_transitions_return_final_state_and_resolved_attention() {
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let mut batch = execution_page(&owner, "batch", execution_terminal());
    let mut running = batch.observations[0].clone();
    running.observation_id = "running".into();
    running.kind = HarnessObservationKind::Execution {
        event: execution_work(Some(execution_attempt()), HarnessWorkState::Running),
    };
    let mut completed = running.clone();
    completed.observation_id = "completed".into();
    completed.kind = HarnessObservationKind::Execution {
        event: execution_work(Some(execution_attempt()), HarnessWorkState::Completed),
    };
    let mut notified = completed.clone();
    notified.observation_id = "notified".into();
    if let HarnessObservationKind::Execution {
        event: HarnessExecutionEvent::BackgroundWork { notification, .. },
    } = &mut notified.kind
    {
        *notification = HarnessNotificationState::Observed;
    }
    batch
        .observations
        .extend([running, completed, notified.clone()]);
    sessions
        .commit_external_harness_page(&owner, None, &batch)
        .await
        .unwrap();
    let evidence = sessions
        .session_external_harness_submission_evidence(
            owner.session_id,
            "execution-input",
            "execution-attempt",
        )
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(evidence.status.attention, HarnessAttention::None);
    assert_eq!(evidence.observations.len(), 2);
    assert!(evidence.observations.contains(&notified));
    let mut updated = execution_work(Some(execution_attempt()), HarnessWorkState::Completed);
    if let HarnessExecutionEvent::BackgroundWork { notification, .. } = &mut updated {
        *notification = HarnessNotificationState::Observed;
    }
    let mut cursor = "batch".to_owned();
    for index in 0..128 {
        let next = format!("update-{index}");
        sessions
            .commit_external_harness_page(
                &owner,
                Some(&cursor),
                &execution_page(&owner, &next, updated.clone()),
            )
            .await
            .unwrap();
        cursor = next;
    }
    // Current membership cardinality remains one despite repeated updates.
    let evidence = sessions
        .session_external_harness_submission_evidence(
            owner.session_id,
            "execution-input",
            "execution-attempt",
        )
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(evidence.observations.len(), 2);
    let reopened = execution_page(
        &owner,
        "reopened",
        execution_work(Some(execution_attempt()), HarnessWorkState::Running),
    );
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some(&cursor), &reopened)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn execution_attention_tracks_pending_and_acknowledged_interaction() {
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let terminal = execution_page(&owner, "terminal", execution_terminal());
    sessions
        .commit_external_harness_page(&owner, None, &terminal)
        .await
        .unwrap();
    let mut question = page(&owner);
    question.next_after = Some("question".into());
    question.observations[0].observation_id = "question".into();
    question.observations[0].native_correlations.clear();
    question.observations[0].kind = HarnessObservationKind::Interaction {
        interaction: HarnessInteraction {
            input_preview: None,
            tool_name: None,
            request_id: "question".into(),
            session_id: owner.session_id.to_string(),
            generation: owner.generation,
            kind: HarnessInteractionKind::Approval,
            prompt: "Proceed?".into(),
            scope: "fixture".into(),
            choices: vec![],
        },
    };
    question.status.attention = HarnessAttention::Approval;
    sessions
        .commit_external_harness_page(&owner, Some("terminal"), &question)
        .await
        .unwrap();
    let evidence = sessions
        .session_external_harness_submission_evidence(
            owner.session_id,
            "execution-input",
            "execution-attempt",
        )
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(evidence.status.attention, HarnessAttention::Approval);
    let request = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "answer".into(),
        instance: instance(),
        kind: HarnessRequestKind::Respond {
            target: HarnessSessionTarget::Prepared(prepared(&owner)),
            response: HarnessResponse {
                response_id: "answer".into(),
                request_id: "question".into(),
                session_id: owner.session_id.to_string(),
                generation: owner.generation,
                value: HarnessResponseValue::Approval { approved: true },
            },
        },
    };
    sessions
        .claim_external_harness_request(&owner, &request)
        .await
        .unwrap();
    // Claiming the response is not native acknowledgment and cannot clear attention.
    let evidence = sessions
        .session_external_harness_submission_evidence(
            owner.session_id,
            "execution-input",
            "execution-attempt",
        )
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_ne!(evidence.status.attention, HarnessAttention::None);
    sessions
        .complete_external_harness_request(
            &owner,
            &request,
            &HarnessServiceResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: "answer".into(),
                operation: HarnessOperation::Respond,
                kind: HarnessServiceResponseKind::Acknowledged {
                    mutation_id: "answer".into(),
                },
            },
        )
        .await
        .unwrap();
    let evidence = sessions
        .session_external_harness_submission_evidence(
            owner.session_id,
            "execution-input",
            "execution-attempt",
        )
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(evidence.status.attention, HarnessAttention::None);
}

#[tokio::test]
async fn execution_snapshot_rejects_physical_and_cardinality_overflow_without_repair() {
    use bcode_session::db::SessionDb;
    use switchy::database::query::FilterableQuery;
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let terminal = execution_page(&owner, "terminal", execution_terminal());
    sessions
        .commit_external_harness_page(&owner, None, &terminal)
        .await
        .unwrap();
    sessions
        .release_idle_session_resources(owner.session_id)
        .await
        .unwrap();
    let db = SessionDb::open_existing_turso_in_root(owner.session_id, root.path())
        .await
        .unwrap();
    let tail = db.last_event_sequence().await.unwrap();
    let namespace = format!("execution_v3:{}:inventory:unknown:work", owner.generation);
    let physical = format!("[{}]", " ".repeat(200_000));
    let cardinal =
        serde_json::to_string(&vec![("locator", 1_u64); MAX_HARNESS_EXECUTION_ITEMS + 1]).unwrap();
    for payload in [physical, cardinal] {
        db.database()
            .update("snapshots")
            .value("payload", payload.clone())
            .where_eq("snapshot_name", namespace.clone())
            .execute(db.database())
            .await
            .unwrap();
        assert!(
            db.external_harness_submission_evidence("execution-input", "execution-attempt")
                .await
                .is_err()
        );
        assert_eq!(db.last_event_sequence().await.unwrap(), tail);
        let retained = db
            .database()
            .select("snapshots")
            .columns(&["payload"])
            .where_eq("snapshot_name", namespace.clone())
            .limit(1)
            .execute(db.database())
            .await
            .unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(
            retained[0].get("payload").unwrap().as_str(),
            Some(payload.as_str())
        );
    }
}

#[tokio::test]
async fn execution_missing_projection_fails_without_hidden_replay() {
    use bcode_session::db::SessionDb;
    use switchy::database::query::FilterableQuery;
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let terminal = execution_page(&owner, "terminal", execution_terminal());
    sessions
        .commit_external_harness_page(&owner, None, &terminal)
        .await
        .unwrap();
    sessions
        .release_idle_session_resources(owner.session_id)
        .await
        .unwrap();
    let db = SessionDb::open_existing_turso_in_root(owner.session_id, root.path())
        .await
        .unwrap();
    let tail = db.last_event_sequence().await.unwrap();
    let key = bcode_session_models::external_backend::external_backend_key(
        "execution_coverage_v3",
        "current",
    );
    db.database()
        .delete("turn_evidence")
        .where_eq("evidence_kind", "harness_operation_v1")
        .where_eq("turn_id", key.clone())
        .execute(db.database())
        .await
        .unwrap();
    assert!(
        db.external_harness_submission_evidence("execution-input", "execution-attempt")
            .await
            .is_err()
    );
    assert_eq!(db.last_event_sequence().await.unwrap(), tail);
    let missing = db
        .database()
        .select("turn_evidence")
        .columns(&["event_seq"])
        .where_eq("evidence_kind", "harness_operation_v1")
        .where_eq("turn_id", key)
        .execute(db.database())
        .await
        .unwrap();
    assert!(missing.is_empty());
}

#[tokio::test]
async fn execution_inventory_is_bounded_and_continuation_is_not_notification() {
    use bcode_session_models::{HarnessInventoryKind, HarnessInventoryScope};
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let mut work = execution_work(Some(execution_attempt()), HarnessWorkState::Completed);
    if let HarnessExecutionEvent::BackgroundWork { notification, .. } = &mut work {
        *notification = HarnessNotificationState::Observed;
    }
    let work = execution_page(&owner, "work", work);
    sessions
        .commit_external_harness_page(&owner, None, &work)
        .await
        .unwrap();
    let continuation = execution_page(
        &owner,
        "followup",
        HarnessExecutionEvent::Continuation {
            continuation_id: "followup".into(),
            origin: Some(execution_attempt()),
            source_work_id: Some("work-1".into()),
            native_turn_id: None,
            state: HarnessWorkState::Pending,
            output: None,
        },
    );
    sessions
        .commit_external_harness_page(&owner, Some("work"), &continuation)
        .await
        .unwrap();
    let evidence = sessions
        .session_external_harness_submission_evidence(
            owner.session_id,
            "execution-input",
            "execution-attempt",
        )
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(evidence.completeness, HarnessCompleteness::Partial);
    assert!(evidence.observations.iter().any(|v| matches!(
        &v.kind,
        HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::Continuation {
                state: HarnessWorkState::Pending,
                ..
            }
        }
    )));
    let mut cursor = "followup".to_owned();
    for index in 0..MAX_HARNESS_EXECUTION_ITEMS - 1 {
        let mut event = execution_work(Some(execution_attempt()), HarnessWorkState::Running);
        if let HarnessExecutionEvent::BackgroundWork {
            work_id,
            native_task_id,
            ..
        } = &mut event
        {
            *work_id = format!("bounded-{index}");
            *native_task_id = Some(format!("native-bounded-{index}"));
        }
        let next = format!("bounded-{index}");
        let next_page = execution_page(&owner, &next, event);
        sessions
            .commit_external_harness_page(&owner, Some(&cursor), &next_page)
            .await
            .unwrap();
        cursor = next;
    }
    let mut event = execution_work(Some(execution_attempt()), HarnessWorkState::Running);
    if let HarnessExecutionEvent::BackgroundWork {
        work_id,
        native_task_id,
        ..
    } = &mut event
    {
        *work_id = "overflow".into();
        *native_task_id = Some("native-overflow".into());
    }
    let overflow = execution_page(&owner, "overflow", event);
    sessions
        .commit_external_harness_page(&owner, Some(&cursor), &overflow)
        .await
        .unwrap();
    let scope = HarnessInventoryScope::Attempt(execution_attempt());
    let page = sessions
        .session_harness_inventory_page(&owner, &scope, HarnessInventoryKind::Work, None)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 32);
    assert_eq!(
        page.commitment.summary.identities,
        MAX_HARNESS_EXECUTION_ITEMS as u64 + 1
    );
    assert!(page.next_cursor.is_some());
    let continuation = sessions
        .session_harness_inventory_summary(&owner, &scope, HarnessInventoryKind::Continuation)
        .await
        .unwrap();
    assert_eq!(continuation.commitment.summary.unfinished, 1);
    let next = sessions
        .session_harness_inventory_page(
            &owner,
            &scope,
            HarnessInventoryKind::Work,
            page.next_cursor.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(next.commitment, page.commitment);
    assert_eq!(next.entries.len(), 1);
    assert!(next.inventory_exhausted);
    assert!(next.next_cursor.is_none());
    assert!(
        !page
            .entries
            .iter()
            .any(|entry| entry.key == next.entries[0].key)
    );
}

#[tokio::test]
async fn withdrawn_interaction_is_no_longer_answerable() {
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    let mut question = page(&owner);
    question.next_after = Some("question".into());
    question.observations[0].observation_id = "question".into();
    question.observations[0].native_correlations.clear();
    question.observations[0].kind = HarnessObservationKind::Interaction {
        interaction: HarnessInteraction {
            input_preview: None,
            tool_name: None,
            request_id: "question".into(),
            session_id: owner.session_id.to_string(),
            generation: owner.generation,
            kind: HarnessInteractionKind::Approval,
            prompt: "Proceed?".into(),
            scope: "fixture".into(),
            choices: vec![],
        },
    };
    sessions
        .commit_external_harness_page(&owner, None, &question)
        .await
        .unwrap();
    assert!(
        sessions
            .external_harness_interaction(&owner, "question")
            .await
            .unwrap()
            .is_some()
    );
    let mut withdrawn = page(&owner);
    withdrawn.next_after = Some("withdrawn".into());
    withdrawn.observations[0].observation_id = "withdrawn".into();
    withdrawn.observations[0].native_correlations.clear();
    withdrawn.observations[0].kind = HarnessObservationKind::InteractionWithdrawn {
        request_id: "question".into(),
    };
    sessions
        .commit_external_harness_page(&owner, Some("question"), &withdrawn)
        .await
        .unwrap();
    assert_eq!(
        sessions
            .external_harness_interaction(&owner, "question")
            .await
            .unwrap(),
        None,
        "a withdrawn prompt is not pending"
    );
    let request = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "late-answer".into(),
        instance: instance(),
        kind: HarnessRequestKind::Respond {
            target: HarnessSessionTarget::Prepared(prepared(&owner)),
            response: HarnessResponse {
                response_id: "late-answer".into(),
                request_id: "question".into(),
                session_id: owner.session_id.to_string(),
                generation: owner.generation,
                value: HarnessResponseValue::Approval { approved: true },
            },
        },
    };
    assert!(
        sessions
            .claim_external_harness_request(&owner, &request)
            .await
            .is_err(),
        "a late answer never reaches the native harness"
    );
}

#[tokio::test]
async fn native_live_text_without_a_durable_segment_retires_when_the_turn_ends() {
    use bcode_session_models::{
        ClientId, SessionLiveEventKind, TextStreamOperation, TextStreamUpdate,
    };
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = execution_fixture(root.path()).await;
    // A partial block the native turn abandons before its assistant frame is emitted.
    let identity = harness_stream_identity(owner.generation, "msg_partial:0");
    let publish =
        |revision, expected_offset, text: &str| SessionLiveEventKind::AssistantTextStreamUpdated {
            output_position: None,
            turn_id: identity.turn_id.clone(),
            segment_id: identity.segment_id.clone(),
            segment_order: identity.segment_order,
            update: TextStreamUpdate {
                generation: 0,
                first_revision: revision,
                revision,
                operation: TextStreamOperation::Append {
                    expected_offset,
                    text: text.to_owned(),
                },
            },
        };
    let _ = sessions
        .publish_live_event(owner.session_id, publish(1, 0, "partial "))
        .await;
    let attached = sessions
        .attach_session(owner.session_id, ClientId::new())
        .await
        .unwrap();
    assert_eq!(attached.live_checkpoints.len(), 1);
    let mut ended = page(&owner);
    ended.observations[0].native_correlations.clear();
    ended.observations[0].kind = HarnessObservationKind::Result {
        text: "interrupted".into(),
    };
    sessions
        .commit_external_harness_page(&owner, None, &ended)
        .await
        .unwrap();
    let attached = sessions
        .attach_session(owner.session_id, ClientId::new())
        .await
        .unwrap();
    assert_eq!(
        attached.live_checkpoints,
        Vec::new(),
        "a reattach never replays abandoned partial text"
    );
    // A late delta for the ended stream cannot revive it.
    let _ = sessions
        .publish_live_event(owner.session_id, publish(2, 8, "late"))
        .await;
    let attached = sessions
        .attach_session(owner.session_id, ClientId::new())
        .await
        .unwrap();
    assert_eq!(attached.live_checkpoints, Vec::new());
}

#[tokio::test]
async fn managed_input_names_the_session_and_survives_reopening() {
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
    let submit = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "submit-1".into(),
        instance: instance(),
        kind: HarnessRequestKind::Submit {
            target: HarnessSessionTarget::Prepared(prepared(&owner)),
            submission: HarnessSubmission {
                submission_id: "input-1".into(),
                attempt_id: "attempt".into(),
                session_id: owner.session_id.to_string(),
                generation: 1,
                text: "Fix the failing parser test".into(),
                placement: HarnessPlacement::NextTurn,
            },
        },
    };
    sessions
        .claim_external_harness_request(&owner, &submit)
        .await
        .unwrap();
    let title = |summary: bcode_session_models::SessionSummary| summary.title().map(str::to_owned);
    assert_eq!(
        title(sessions.session_summary(session.id).await.unwrap()).as_deref(),
        Some("Fix the failing parser test")
    );
    drop(sessions);
    // The stored projection carries the title for a fresh reader.
    let reopened = manager(root.path());
    assert_eq!(
        title(reopened.session_summary(session.id).await.unwrap()).as_deref(),
        Some("Fix the failing parser test")
    );
}

/// One native usage observation as normalized by a managed harness producer.
fn native_usage(
    session_id: SessionId,
    observation_id: &str,
    total_cost_micros: Option<u64>,
) -> HarnessObservation {
    HarnessObservation {
        observation_id: observation_id.into(),
        session_id: session_id.to_string(),
        generation: 1,
        native_correlations: vec!["turn".into()],
        completeness: HarnessCompleteness::Complete,
        kind: HarnessObservationKind::Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_input_tokens: 1_000,
            cache_creation_input_tokens: 10,
            total_cost_micros,
            duration_ms: Some(5),
            model: Some("native-model".into()),
        },
    }
}

async fn prepared_harness(root: &std::path::Path) -> (SessionManager, ExternalHarnessOwner) {
    let sessions = manager(root);
    let session = sessions.create_session(None, root.into()).await.unwrap();
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
    (sessions, owner)
}

fn catalog_estimate(total_micros: u64) -> bcode_session_models::SessionCostEstimate {
    bcode_session_models::SessionCostEstimate::Estimated {
        currency: "USD".into(),
        total_micros,
        components: vec![bcode_session_models::SessionCostComponent {
            bucket: "input".into(),
            modality: None,
            tokens: 10,
            price_micros: total_micros * 100_000,
            cost_micros: total_micros,
        }],
        source: "catalog".into(),
        revision: None,
    }
}

#[tokio::test]
async fn native_usage_is_authoritative_session_usage_across_reopen() {
    use bcode_session_models::{ClientId, SessionCostRange, SessionTokenUsage};
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = prepared_harness(root.path()).await;
    let session_id = owner.session_id;
    let guard = sessions
        .acquire_external_harness_ownership(&owner)
        .await
        .unwrap();
    let mut native = page(&owner);
    native.observations.extend([
        native_usage(session_id, "result-priced", Some(1_234)),
        // A subscription reports tokens without any monetary cost.
        native_usage(session_id, "result-subscription", None),
    ]);
    let mut subscription = sessions.subscribe_session_events(session_id).await.unwrap();
    sessions
        .commit_external_harness_page(&owner, None, &native)
        .await
        .unwrap();
    // Attached clients receive the refreshed authoritative totals with the native page.
    let live_totals = std::iter::from_fn(|| subscription.live_events.try_recv().ok())
        .find_map(|event| match event.kind {
            bcode_session_models::SessionLiveEventKind::UsageSummaryChanged { summary } => {
                Some(summary)
            }
            _ => None,
        })
        .expect("native usage publishes an accounting update");
    assert_eq!(live_totals.totals_micros.get("USD"), Some(&1_234));
    // An identical redelivery of the committed page is not counted again.
    sessions
        .commit_external_harness_page(&owner, None, &native)
        .await
        .unwrap();
    let native_tokens = 2 * (100 + 1_000 + 10 + 20);
    let summary = sessions
        .attach_session_recent(session_id, ClientId::new(), 1)
        .await
        .unwrap()
        .usage_summary;
    assert_eq!(summary.cumulative_metered_tokens, native_tokens);
    assert_eq!(summary.totals_micros.get("USD"), Some(&1_234));
    assert_eq!(summary.observed_usage_count, 2);
    assert_eq!(summary.estimated_usage_count, 1);
    // Unpriced subscription usage is not reported as unavailable pricing.
    assert_eq!(summary.unavailable_usage_count, 0);

    // Bcode-metered usage joins the same projection and remains repriceable.
    let model_usage = SessionTokenUsage {
        request_id: Some("model-request".into()),
        observation_id: Some("model-request:0".into()),
        terminal: true,
        input_tokens: Some(7),
        output_tokens: Some(3),
        ..SessionTokenUsage::default()
    };
    sessions
        .append_priced_model_usage(session_id, "turn".into(), model_usage, catalog_estimate(10))
        .await
        .unwrap();
    let (repriced, summary) = sessions
        .reprice_usage(
            session_id,
            SessionCostRange {
                from_timestamp_ms: 0,
                to_timestamp_ms: i64::MAX.cast_unsigned(),
            },
            std::sync::Arc::new(|_| catalog_estimate(77)),
        )
        .await
        .unwrap();
    // Only the Bcode-metered request is valued; native costs are vendor facts.
    assert_eq!(repriced, 1);
    assert_eq!(summary.totals_micros.get("USD"), Some(&(1_234 + 77)));
    assert_eq!(summary.cumulative_metered_tokens, native_tokens + 10);
    assert_eq!(summary.observed_usage_count, 3);
    drop(guard);
    sessions
        .release_idle_session_resources(session_id)
        .await
        .unwrap();
    drop(sessions);

    // A fresh reader gets the same totals without loading the native page into history.
    let reopened = manager(root.path());
    let attachment = reopened
        .attach_session_recent(session_id, ClientId::new(), 1)
        .await
        .unwrap();
    assert!(attachment.history.iter().all(|event| {
        bcode_session_models::native_harness_usage_in_event(&event.kind).is_empty()
    }));
    let reopened_summary = attachment.usage_summary;
    assert_eq!(reopened_summary.totals_micros, summary.totals_micros);
    assert_eq!(
        reopened_summary.cumulative_metered_tokens,
        summary.cumulative_metered_tokens
    );
    assert_eq!(reopened_summary.observed_usage_count, 3);
    assert_eq!(reopened_summary.cost_revision, summary.cost_revision);
}

#[tokio::test]
async fn conflicting_native_usage_for_one_observation_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = prepared_harness(root.path()).await;
    let session_id = owner.session_id;
    let _guard = sessions
        .acquire_external_harness_ownership(&owner)
        .await
        .unwrap();
    let mut first = page(&owner);
    first.observations = vec![native_usage(session_id, "result", Some(5))];
    sessions
        .commit_external_harness_page(&owner, None, &first)
        .await
        .unwrap();
    let mut conflicting = first.clone();
    conflicting.next_after = Some("native-cursor-2".into());
    conflicting.observations[0].kind = HarnessObservationKind::Usage {
        input_tokens: 1,
        output_tokens: 1,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
        total_cost_micros: Some(6),
        duration_ms: None,
        model: None,
    };
    assert!(
        sessions
            .commit_external_harness_page(&owner, Some("native-cursor"), &conflicting)
            .await
            .is_err()
    );
    let summary = sessions
        .attach_session_recent(session_id, bcode_session_models::ClientId::new(), 1)
        .await
        .unwrap()
        .usage_summary;
    assert_eq!(summary.totals_micros.get("USD"), Some(&5));
    assert_eq!(summary.observed_usage_count, 1);
}

async fn prepared_history_fixture(
    root: &std::path::Path,
) -> (SessionManager, ExternalHarnessOwner) {
    let sessions = manager(root);
    let session = sessions.create_session(None, root.into()).await.unwrap();
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
    (sessions, owner)
}

fn history_page(
    owner: &ExternalHarnessOwner,
    cursor: &str,
    ids: &[&str],
) -> HarnessObservationPage {
    let mut page = page(owner);
    let template = page.observations.remove(0);
    page.observations = ids
        .iter()
        .map(|id| HarnessObservation {
            observation_id: (*id).into(),
            kind: HarnessObservationKind::Text {
                segment_id: None,
                text: format!("text {id}"),
            },
            ..template.clone()
        })
        .collect();
    page.next_after = Some(cursor.into());
    page
}

async fn read_all_history(
    sessions: &SessionManager,
    session_id: SessionId,
    limit: u16,
) -> (Vec<String>, usize) {
    let mut cursor = None;
    let mut ids = Vec::new();
    let mut pages = 0;
    loop {
        let page = sessions
            .external_harness_history(session_id, cursor.clone(), limit)
            .await
            .unwrap();
        assert!(page.observations.len() <= usize::from(limit));
        let next = page
            .next_after
            .clone()
            .expect("canonical history always has a cursor");
        ids.extend(
            page.observations
                .into_iter()
                .map(|item| item.observation_id),
        );
        if cursor.as_ref() == Some(&next) {
            return (ids, pages);
        }
        pages += 1;
        assert!(pages < 64, "history pagination must make progress");
        cursor = Some(next);
    }
}

fn observation_ids(page: &HarnessObservationPage) -> Vec<&str> {
    page.observations
        .iter()
        .map(|item| item.observation_id.as_str())
        .collect()
}

#[tokio::test]
async fn history_pages_aggregate_events_resume_mid_event_and_skip_empty_events() {
    let root = tempfile::tempdir().unwrap();
    let (sessions, owner) = prepared_history_fixture(root.path()).await;
    let session_id = owner.session_id;
    let guard = sessions
        .acquire_external_harness_ownership(&owner)
        .await
        .unwrap();
    let events: [(&str, &[&str]); 6] = [
        ("c1", &["a1", "a2", "a3"]),
        ("c2", &[]),
        ("c3", &["b1", "b2"]),
        ("c4", &[]),
        ("c5", &["d1"]),
        ("c6", &[]),
    ];
    let mut after: Option<&str> = None;
    for (cursor, ids) in events {
        sessions
            .commit_external_harness_page(&owner, after, &history_page(&owner, cursor, ids))
            .await
            .unwrap();
        after = Some(cursor);
    }
    drop(guard);
    let expected = ["a1", "a2", "a3", "b1", "b2", "d1"];

    // One page holds every observation across consecutive events, skipping empty events.
    let all = sessions
        .external_harness_history(session_id, None, 128)
        .await
        .unwrap();
    assert_eq!(observation_ids(&all), expected);
    assert_eq!(all.completeness, HarnessCompleteness::Complete);
    let tail = all.next_after.clone().unwrap();
    let caught_up = sessions
        .external_harness_history(session_id, Some(tail.clone()), 128)
        .await
        .unwrap();
    assert!(caught_up.observations.is_empty());
    assert_eq!(caught_up.next_after.as_deref(), Some(tail.as_str()));

    // Limits split mid-event and resume exactly, never duplicating or skipping.
    let first = sessions
        .external_harness_history(session_id, None, 2)
        .await
        .unwrap();
    assert_eq!(observation_ids(&first), ["a1", "a2"]);
    assert_eq!(first.completeness, HarnessCompleteness::Partial);
    let second = sessions
        .external_harness_history(session_id, first.next_after.clone(), 2)
        .await
        .unwrap();
    assert_eq!(
        observation_ids(&second),
        ["a3", "b1"],
        "mid-event resume crosses an empty event"
    );
    assert_eq!(second.completeness, HarnessCompleteness::Partial);
    let third = sessions
        .external_harness_history(session_id, second.next_after.clone(), 2)
        .await
        .unwrap();
    assert_eq!(observation_ids(&third), ["b2", "d1"]);
    assert_eq!(third.completeness, HarnessCompleteness::Partial);
    // Only the trailing empty event remains: zero observations but an advanced cursor.
    let fourth = sessions
        .external_harness_history(session_id, third.next_after.clone(), 2)
        .await
        .unwrap();
    assert!(fourth.observations.is_empty());
    assert_ne!(fourth.next_after, third.next_after);
    assert_eq!(fourth.next_after.as_deref(), Some(tail.as_str()));

    for limit in [1, 2, 3, 4, 5, 128] {
        let (ids, pages) = read_all_history(&sessions, session_id, limit).await;
        assert_eq!(ids, expected, "limit {limit}");
        assert!(
            pages <= expected.len().div_ceil(usize::from(limit)) + 1,
            "limit {limit}"
        );
    }

    // Single-event cursors keep their format; malformed or out-of-range cursors are rejected.
    let legacy = sessions
        .external_harness_history(session_id, Some("h1:0:0".into()), 128)
        .await
        .unwrap();
    assert_eq!(legacy.observations, all.observations);
    for cursor in ["h2:1:0", "h1:x:0", "h1:1:129", "h1:1:0:0", "h1:1"] {
        assert!(
            sessions
                .external_harness_history(session_id, Some(cursor.into()), 16)
                .await
                .is_err(),
            "{cursor}"
        );
    }
    let mid = first.next_after.clone().unwrap();
    let (prefix, _) = mid.rsplit_once(':').unwrap();
    assert!(
        sessions
            .external_harness_history(session_id, Some(format!("{prefix}:4")), 16)
            .await
            .is_err(),
        "an offset past the event's observations is rejected"
    );

    // History stays readable offline from the reopened store.
    sessions
        .release_idle_session_resources(session_id)
        .await
        .unwrap();
    let reopened = manager(root.path());
    let (ids, _) = read_all_history(&reopened, session_id, 4).await;
    assert_eq!(ids, expected);
}
