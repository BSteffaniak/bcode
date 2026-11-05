//! Behavioral associated-work and immutable-terminal verdict cases.
use super::*;
use bcode_harness_models::HarnessWorkKind;

fn exact() -> HarnessAttemptIdentity {
    HarnessAttemptIdentity {
        submission_id: "submission".into(),
        attempt_id: "attempt".into(),
    }
}
fn coverage() -> E {
    E::Coverage {
        attempt: exact(),
        background: C::Complete,
        continuations: C::Complete,
    }
}
fn task(state: W, notification: N) -> E {
    E::BackgroundWork {
        work_id: "adapter-work".into(),
        native_task_id: Some("native-task".into()),
        origin: Some(exact()),
        parent_work_id: None,
        work_kind: HarnessWorkKind::Command,
        label: "command".into(),
        state,
        notification,
    }
}
fn continuation(state: W) -> E {
    E::Continuation {
        continuation_id: "adapter-followup".into(),
        origin: Some(exact()),
        source_work_id: Some("adapter-work".into()),
        native_turn_id: None,
        state,
        output: None,
    }
}
fn deferred_work(work: &Work) -> bool {
    matches!(work.verdict(), Some(AttemptObservation::Deferred { .. }))
}

#[test]
fn coverage_does_not_erase_pending_work_or_notifications() {
    let mut work = Work::default();
    assert!(work.observe(&task(W::Running, N::Pending), &exact()));
    assert!(work.observe(&coverage(), &exact()));
    assert!(deferred_work(&work));
    assert!(work.observe(&task(W::Completed, N::Pending), &exact()));
    assert!(work.observe(&coverage(), &exact()));
    assert!(deferred_work(&work));
    assert!(work.observe(&task(W::Completed, N::Observed), &exact()));
    assert!(deferred_work(&work));
    assert!(work.observe(&continuation(W::Running), &exact()));
    assert!(deferred_work(&work));
    assert!(work.observe(&continuation(W::Completed), &exact()));
    assert!(work.verdict().is_none());
}

#[test]
fn task_failure_is_assignment_failure_without_rewriting_foreground() {
    let mut work = Work::default();
    assert!(work.observe(&task(W::Failed, N::NotExpected), &exact()));
    assert!(work.observe(&coverage(), &exact()));
    assert!(matches!(
        work.verdict(),
        Some(AttemptObservation::Failed { .. })
    ));
    assert!(!work.observe(&task(W::Running, N::NotExpected), &exact()));
    assert_eq!(work.tasks["adapter-work"].0, W::Failed);
    assert!(work.observe(&task(W::Unknown, N::Unknown), &exact()));
    assert_eq!(work.tasks["adapter-work"].0, W::Failed);
    assert!(deferred_work(&work));
    assert!(work.observe(&task(W::Failed, N::NotExpected), &exact()));
    assert!(matches!(
        work.verdict(),
        Some(AttemptObservation::Failed { .. })
    ));
}

#[test]
fn unknown_origin_and_unknown_coverage_are_not_empty_success() {
    let mut work = Work::default();
    assert!(deferred_work(&work));
    let mut unknown = task(W::Completed, N::NotExpected);
    if let E::BackgroundWork { origin, .. } = &mut unknown {
        *origin = None;
    }
    assert!(work.observe(&unknown, &exact()));
    assert!(work.observe(&coverage(), &exact()));
    assert!(deferred_work(&work));
}

#[test]
fn bounded_inventory_and_continuation_terminals_fail_closed() {
    let mut work = Work::default();
    assert!(work.observe(&continuation(W::Completed), &exact()));
    assert!(!work.observe(&continuation(W::Running), &exact()));
    for index in 0..MAX_HARNESS_EXECUTION_ITEMS {
        let event = E::Continuation {
            continuation_id: format!("followup-{index}"),
            origin: Some(exact()),
            source_work_id: None,
            native_turn_id: None,
            state: W::Completed,
            output: None,
        };
        let accepted = work.observe(&event, &exact());
        assert_eq!(accepted, index + 1 < MAX_HARNESS_EXECUTION_ITEMS);
    }
}
