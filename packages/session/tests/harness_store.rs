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
        .external_harness_history(session.id, None, 128)
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
