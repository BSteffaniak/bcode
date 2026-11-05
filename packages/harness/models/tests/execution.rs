//! Lifecycle facts remain separate across the portable observation boundary.
use bcode_harness_models::{
    HarnessAttemptIdentity, HarnessCompleteness, HarnessExecutionEvent, HarnessNotificationState,
    HarnessObservation, HarnessObservationKind, HarnessTerminalDisposition,
    HarnessTrackingCoverage, HarnessWorkKind, HarnessWorkState,
};

fn attempt() -> HarnessAttemptIdentity {
    HarnessAttemptIdentity {
        submission_id: "submission".into(),
        attempt_id: "attempt".into(),
    }
}
fn observation(event: HarnessExecutionEvent) -> HarnessObservation {
    HarnessObservation {
        observation_id: "observation".into(),
        session_id: "session".into(),
        generation: 2,
        native_correlations: Vec::new(),
        completeness: HarnessCompleteness::Complete,
        kind: HarnessObservationKind::Execution { event },
    }
}

#[test]
fn foreground_result_frame_does_not_require_fabricated_native_turn() {
    let event = observation(HarnessExecutionEvent::SubmissionTerminal {
        attempt: attempt(),
        native_turn_id: None,
        native_frame_id: Some("native-result-frame".into()),
        disposition: HarnessTerminalDisposition::Completed,
        output: Some("reply".into()),
    });
    assert!(event.validate().is_ok());
    let encoded = serde_json::to_value(&event).unwrap();
    assert!(encoded["kind"]["event"]["native_turn_id"].is_null());
    assert_eq!(
        serde_json::from_value::<HarnessObservation>(encoded).unwrap(),
        event
    );
}

#[test]
fn structural_terminal_evidence_requires_a_genuine_native_reference() {
    let event = observation(HarnessExecutionEvent::SubmissionTerminal {
        attempt: attempt(),
        native_turn_id: None,
        native_frame_id: None,
        disposition: HarnessTerminalDisposition::Completed,
        output: Some("reply".into()),
    });
    assert!(event.validate().is_err());
}

#[test]
fn completed_work_retains_pending_notification_independently() {
    let event = observation(HarnessExecutionEvent::BackgroundWork {
        work_id: "adapter-work".into(),
        native_task_id: Some("native-task".into()),
        origin: Some(attempt()),
        parent_work_id: None,
        work_kind: HarnessWorkKind::Command,
        label: "Background command".into(),
        state: HarnessWorkState::Completed,
        notification: HarnessNotificationState::Pending,
    });
    assert!(event.validate().is_ok());
    let encoded = serde_json::to_value(&event).unwrap();
    assert_eq!(encoded["kind"]["event"]["state"], "completed");
    assert_eq!(encoded["kind"]["event"]["notification"], "pending");
    assert_eq!(
        serde_json::from_value::<HarnessObservation>(encoded).unwrap(),
        event
    );
}

#[test]
fn unknown_origin_and_tracking_coverage_are_not_empty_authoritative_rosters() {
    let work = observation(HarnessExecutionEvent::Continuation {
        continuation_id: "adapter-followup".into(),
        origin: None,
        source_work_id: None,
        native_turn_id: None,
        state: HarnessWorkState::Unknown,
        output: None,
    });
    assert!(work.validate().is_ok());
    let coverage = observation(HarnessExecutionEvent::Coverage {
        attempt: attempt(),
        background: HarnessTrackingCoverage::Unknown,
        continuations: HarnessTrackingCoverage::Unknown,
    });
    assert!(coverage.validate().is_ok());
    assert!(!HarnessWorkState::Unknown.is_terminal());
    assert!(!HarnessWorkState::Pending.is_terminal());
    assert!(HarnessWorkState::Completed.is_terminal());
}

#[test]
fn rejects_self_parent_and_unknown_wire_fields() {
    let mut event = HarnessExecutionEvent::BackgroundWork {
        work_id: "task".into(),
        native_task_id: None,
        origin: Some(attempt()),
        parent_work_id: Some("task".into()),
        work_kind: HarnessWorkKind::Subagent,
        label: "Agent".into(),
        state: HarnessWorkState::Running,
        notification: HarnessNotificationState::Unknown,
    };
    assert!(event.validate().is_err());
    if let HarnessExecutionEvent::BackgroundWork { parent_work_id, .. } = &mut event {
        *parent_work_id = None;
    }
    assert!(event.validate().is_ok());
    let mut wire = serde_json::to_value(event).unwrap();
    wire["unrecognized_authority"] = true.into();
    assert!(serde_json::from_value::<HarnessExecutionEvent>(wire).is_err());
}
