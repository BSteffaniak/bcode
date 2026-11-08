use super::*;
use bcode_harness_models::{
    HarnessAttemptIdentity, HarnessCompleteness, HarnessObservation, HarnessTerminalDisposition,
};
use bcode_session_models::{CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionId};

fn origin() -> HarnessAttemptIdentity {
    HarnessAttemptIdentity {
        submission_id: "submit".into(),
        attempt_id: "attempt".into(),
    }
}
fn event(sequence: u64, execution: HarnessExecutionEvent) -> SessionEvent {
    let session_id: SessionId = "00000000-0000-4000-8000-000000000001".parse().unwrap();
    SessionEvent {
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        session_id,
        sequence,
        timestamp_ms: sequence,
        provenance: None,
        kind: ExternalBackendEnvelope {
            version: 1,
            record: ExternalBackendRecord::Observation {
                observation: HarnessObservation {
                    observation_id: format!("observation-{sequence}"),
                    session_id: session_id.to_string(),
                    generation: 3,
                    native_correlations: vec![],
                    completeness: HarnessCompleteness::Complete,
                    kind: HarnessObservationKind::Execution { event: execution },
                },
            },
        }
        .into_event_kind()
        .unwrap(),
    }
}
fn terminal() -> HarnessExecutionEvent {
    HarnessExecutionEvent::SubmissionTerminal {
        attempt: origin(),
        native_turn_id: None,
        native_frame_id: Some("native-result".into()),
        disposition: HarnessTerminalDisposition::Completed,
        output: Some("reply".into()),
    }
}
fn work(
    id: &str,
    origin: Option<HarnessAttemptIdentity>,
    state: HarnessWorkState,
) -> HarnessExecutionEvent {
    HarnessExecutionEvent::BackgroundWork {
        work_id: id.into(),
        native_task_id: None,
        origin,
        parent_work_id: None,
        work_kind: bcode_harness_models::HarnessWorkKind::Subagent,
        label: "native agent".into(),
        state,
        notification: HarnessNotificationState::Pending,
    }
}

#[test]
fn empty_page_gap_or_status_loss_invalidates_prior_complete_tracking() {
    use bcode_harness_models::{
        HarnessActivity, HarnessAttention, HarnessAuthBinding, HarnessAuthCustody,
        HarnessCompleteness, HarnessConnectivity, HarnessContinuation, HarnessExecutionStatus,
        HarnessInstance, HarnessObservationPage,
    };
    use bcode_session_models::{
        ExternalHarnessOperation, ExternalHarnessOperationRecord, ExternalHarnessOwner,
    };
    for status_loss in [false, true] {
        let mut view = ExecutionLifecycleView::default();
        assert!(apply(&mut view, &event(1, terminal())));
        assert!(apply(
            &mut view,
            &event(
                2,
                HarnessExecutionEvent::Coverage {
                    attempt: origin(),
                    background: HarnessTrackingCoverage::Complete,
                    continuations: HarnessTrackingCoverage::Complete
                }
            )
        ));
        assert!(!view.summary().contains("coverage unknown"));
        let mut gap = event(3, terminal());
        gap.kind = bcode_session_models::SessionEventKind::ExternalHarnessOperation {
            operation: Box::new(ExternalHarnessOperation {
                inventory_commitments: Vec::new(),
                version: 1,
                owner: ExternalHarnessOwner {
                    session_id: gap.session_id,
                    generation: 3,
                    artifact_id: "artifact".into(),
                    daemon_id: "daemon".into(),
                },
                record: ExternalHarnessOperationRecord::ObservationPage {
                    instance: HarnessInstance {
                        instance_id: "native".into(),
                        plugin_id: "native.plugin".into(),
                        executable: "/native".into(),
                        config_namespace: "isolated".into(),
                        auth: HarnessAuthBinding {
                            provider_id: "native".into(),
                            profile: "work".into(),
                            credential_namespace: "isolated".into(),
                            custody: HarnessAuthCustody::NativeManaged,
                        },
                    },
                    after: None,
                    page: HarnessObservationPage {
                        live: Vec::new(),
                        retention: None,
                        hydration_requests: Vec::new(),
                        inventory_page: None,
                        observations: vec![],
                        next_after: None,
                        completeness: if status_loss {
                            HarnessCompleteness::Complete
                        } else {
                            HarnessCompleteness::Gap
                        },
                        status: HarnessExecutionStatus {
                            connectivity: HarnessConnectivity::Disconnected,
                            activity: HarnessActivity::Unknown,
                            attention: HarnessAttention::Unknown,
                            continuation: HarnessContinuation::Unknown,
                            completeness: if status_loss {
                                HarnessCompleteness::Unknown
                            } else {
                                HarnessCompleteness::Complete
                            },
                        },
                    },
                },
            }),
        };
        assert!(apply(&mut view, &gap));
        assert!(view.summary().contains("Reply complete"));
        assert!(view.summary().contains("coverage unknown"));
        assert!(
            view.coverage
                .iter()
                .all(|item| !item.background_complete && !item.continuations_complete)
        );
        assert!(apply(
            &mut view,
            &event(
                4,
                HarnessExecutionEvent::Coverage {
                    attempt: origin(),
                    background: HarnessTrackingCoverage::Complete,
                    continuations: HarnessTrackingCoverage::Complete
                }
            )
        ));
        assert!(
            view.summary().contains("coverage unknown"),
            "loss remains sticky"
        );
        assert_eq!(view.submissions[0].state, ExecutionStateView::Completed);
    }
}

#[test]
fn foreground_terminal_late_work_and_continuation_are_independent() {
    let mut reducer = crate::SessionView::new();
    reducer.apply_event(&event(1, terminal()));
    reducer.apply_event(&event(
        2,
        work("one", Some(origin()), HarnessWorkState::Running),
    ));
    reducer.apply_event(&event(
        3,
        work("two", Some(origin()), HarnessWorkState::Running),
    ));
    let snapshot = reducer.snapshot();
    assert!(
        snapshot
            .execution_lifecycle
            .summary()
            .contains("Reply complete + 2 background")
    );
    reducer.apply_event(&event(
        4,
        HarnessExecutionEvent::Continuation {
            continuation_id: "follow-up".into(),
            origin: Some(origin()),
            source_work_id: Some("one".into()),
            native_turn_id: None,
            state: HarnessWorkState::Running,
            output: None,
        },
    ));
    assert_eq!(
        reducer.snapshot().execution_lifecycle.submissions[0].state,
        ExecutionStateView::Completed
    );
    assert!(
        reducer
            .snapshot()
            .execution_lifecycle
            .summary()
            .contains("1 continuation pending")
    );
    assert!(
        reducer.snapshot().execution_lifecycle.submissions[0]
            .native_id
            .is_none()
    );
    // Out-of-order canonical updates never reopen a completed foreground item.
    reducer.apply_event(&event(1, terminal()));
    assert_eq!(
        reducer.snapshot().execution_lifecycle.continuations.len(),
        1
    );
}

#[test]
fn origin_enrichment_is_monotonic_and_conflicts_stay_fenced() {
    let mut view = ExecutionLifecycleView::default();
    assert!(apply(
        &mut view,
        &event(1, work("task", None, HarnessWorkState::Running))
    ));
    assert!(view.summary().contains("session-wide fence"));
    assert!(apply(
        &mut view,
        &event(2, work("task", Some(origin()), HarnessWorkState::Completed))
    ));
    assert!(!view.summary().contains("session-wide fence"));
    let other = HarnessAttemptIdentity {
        submission_id: "other".into(),
        attempt_id: "attempt".into(),
    };
    assert!(apply(
        &mut view,
        &event(3, work("task", Some(other), HarnessWorkState::Running))
    ));
    assert_eq!(view.background[0].origin, Some(attempt(&origin())));
    assert_eq!(view.background[0].state, ExecutionStateView::Completed);
    assert!(view.uncertain);
}

#[test]
fn complete_tracking_never_clears_owed_notification_or_automatic_followup() {
    let mut view = ExecutionLifecycleView::default();
    assert!(apply(&mut view, &event(1, terminal())));
    assert!(apply(
        &mut view,
        &event(2, work("task", Some(origin()), HarnessWorkState::Completed))
    ));
    assert!(apply(
        &mut view,
        &event(
            3,
            HarnessExecutionEvent::Coverage {
                attempt: origin(),
                background: HarnessTrackingCoverage::Complete,
                continuations: HarnessTrackingCoverage::Complete
            }
        )
    ));
    assert!(view.summary().contains("1 notification pending"));
    let mut completed = work("task", Some(origin()), HarnessWorkState::Completed);
    if let HarnessExecutionEvent::BackgroundWork { notification, .. } = &mut completed {
        *notification = HarnessNotificationState::Observed;
    }
    assert!(apply(&mut view, &event(4, completed)));
    assert!(apply(
        &mut view,
        &event(
            5,
            HarnessExecutionEvent::Continuation {
                continuation_id: "automatic".into(),
                origin: Some(origin()),
                source_work_id: Some("task".into()),
                native_turn_id: None,
                state: HarnessWorkState::Pending,
                output: None
            }
        )
    ));
    assert!(view.summary().contains("1 continuation pending"));
    let mut gap = event(
        6,
        HarnessExecutionEvent::Coverage {
            attempt: origin(),
            background: HarnessTrackingCoverage::Complete,
            continuations: HarnessTrackingCoverage::Complete,
        },
    );
    if let bcode_session_models::SessionEventKind::ExternalObservationRecorded {
        observation, ..
    } = &mut gap.kind
    {
        observation.completeness = HarnessCompleteness::Gap;
    }
    assert!(apply(&mut view, &gap));
    assert!(view.summary().contains("coverage unknown"));
}

#[test]
fn generation_change_and_unknown_cannot_reopen_terminal_work() {
    let mut view = ExecutionLifecycleView::default();
    assert!(apply(
        &mut view,
        &event(1, work("task", Some(origin()), HarnessWorkState::Completed))
    ));
    assert!(apply(
        &mut view,
        &event(2, work("task", None, HarnessWorkState::Unknown))
    ));
    assert_eq!(view.background[0].state, ExecutionStateView::Completed);
    assert_eq!(view.background[0].origin, Some(attempt(&origin())));
    assert!(view.uncertain);
    let mut newer = event(3, terminal());
    if let bcode_session_models::SessionEventKind::ExternalObservationRecorded {
        observation, ..
    } = &mut newer.kind
    {
        observation.generation = 4;
    }
    assert!(apply(&mut view, &newer));
    assert!(!apply(
        &mut view,
        &event(4, work("stale", None, HarnessWorkState::Running))
    ));
    assert_eq!(view.generation, Some(4));
    assert_eq!(view.background.len(), 1);
}

#[test]
fn coverage_and_terminal_output_never_enable_stale_controls() {
    let mut view = ExecutionLifecycleView::default();
    assert!(apply(&mut view, &event(1, terminal())));
    assert!(apply(
        &mut view,
        &event(
            2,
            HarnessExecutionEvent::Coverage {
                attempt: origin(),
                background: HarnessTrackingCoverage::Complete,
                continuations: HarnessTrackingCoverage::Complete
            }
        )
    ));
    let capability = bcode_harness_models::HarnessCapability {
        support: bcode_harness_models::HarnessSupportLevel::Supported,
        constraints: vec![],
    };
    let action = crate::external::external_action(
        bcode_session_view_models::external::ExternalActionKind::Respond,
        &capability,
        crate::external::ExternalInteractionAvailability::Stale,
    );
    assert!(!action.enabled);
    assert!(action.reason.contains("stale"));
    assert!(apply(
        &mut view,
        &event(3, work("orphan", None, HarnessWorkState::Unknown))
    ));
    assert!(view.summary().contains("session-wide fence"));
    assert_eq!(view.submissions[0].state, ExecutionStateView::Completed);
}

#[test]
fn lifecycle_patch_uses_full_reset_and_defaults_remain_compatible() {
    let before = bcode_session_view_models::SessionViewSnapshot::empty();
    let mut after = before.clone();
    assert!(apply(&mut after.execution_lifecycle, &event(1, terminal())));
    let patch = bcode_session_view_models::SessionViewPatch::between_snapshots(&before, &after);
    assert_eq!(
        patch.reset.as_ref().unwrap().execution_lifecycle,
        after.execution_lifecycle
    );
    let mut legacy = serde_json::to_value(&before).unwrap();
    legacy
        .as_object_mut()
        .unwrap()
        .remove("execution_lifecycle");
    let decoded: bcode_session_view_models::SessionViewSnapshot =
        serde_json::from_value(legacy).unwrap();
    assert!(
        decoded
            .execution_lifecycle
            .summary()
            .contains("coverage unknown")
    );
}

#[test]
fn bounded_reducer_matches_ordinary_projection_and_rejects_atomically() {
    use bcode_session_view_models::retention::{ObservationRetentionLimits, inspect_snapshot};
    let limits = ObservationRetentionLimits {
        scalar_capacity_bytes: 16 * 1024 * 1024,
        collection_entries: 100_000,
        value_nodes: 100_000,
        depth: 64,
        copies: 64,
    };
    let mut bounded = crate::SessionView::new();
    let mut ordinary = crate::SessionView::new();
    let mut ledger = inspect_snapshot(bounded.snapshot(), limits).unwrap();
    for event in [
        event(1, terminal()),
        event(2, work("one", Some(origin()), HarnessWorkState::Running)),
    ] {
        ordinary.apply_event(&event);
        bounded
            .apply_event_bounded(&event, &mut ledger, limits)
            .unwrap();
        assert_eq!(bounded.snapshot(), ordinary.snapshot());
    }
    let before = bounded.snapshot().clone();
    let prior_ledger = ledger;
    let too_small = ObservationRetentionLimits {
        scalar_capacity_bytes: 1,
        ..limits
    };
    assert!(
        bounded
            .apply_event_bounded(
                &event(3, work("two", Some(origin()), HarnessWorkState::Running)),
                &mut ledger,
                too_small
            )
            .is_err()
    );
    assert_eq!(bounded.snapshot(), &before);
    assert_eq!(ledger, prior_ledger);
}

#[test]
fn renderer_bounds_and_stale_controls_are_independent_of_terminal_reply() {
    let mut snapshot = bcode_session_view_models::SessionViewSnapshot::empty();
    assert!(apply(
        &mut snapshot.execution_lifecycle,
        &event(1, terminal())
    ));
    let limits = bcode_session_view_models::retention::ObservationRetentionLimits {
        scalar_capacity_bytes: 16 * 1024 * 1024,
        collection_entries: 100_000,
        value_nodes: 100_000,
        depth: 64,
        copies: 64,
    };
    let retained =
        bcode_session_view_models::retention::inspect_snapshot(&snapshot, limits).unwrap();
    assert!(retained.collection_entries > 0);
    snapshot.execution_lifecycle.submissions[0].label = "x".repeat(1025);
    assert!(!snapshot.execution_lifecycle.is_bounded());
    assert!(bcode_session_view_models::retention::inspect_snapshot(&snapshot, limits).is_err());
}

#[test]
fn lifecycle_collections_are_bounded_and_overflow_visible() {
    let mut view = ExecutionLifecycleView::default();
    for index in 0..=MAX_HARNESS_EXECUTION_ITEMS {
        assert!(apply(
            &mut view,
            &event(
                u64::try_from(index).unwrap() + 1,
                work(&format!("task-{index}"), None, HarnessWorkState::Running)
            )
        ));
    }
    assert_eq!(view.background.len(), MAX_HARNESS_EXECUTION_ITEMS);
    assert!(view.truncated && view.uncertain);
}
