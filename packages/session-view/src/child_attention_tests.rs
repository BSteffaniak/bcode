use super::*;
use bcode_session_models::{SessionActivityCoverage, SessionActivityEvidence};

#[test]
fn loaded_window_replacement_keeps_newest_attention_and_truthful_partial_coverage() {
    let root = SessionId::new();
    let child = SessionId::new();
    let activity = |session_id, state| SessionActivity {
        session_id,
        parent_id: None,
        states: vec![state],
        evidence: None,
    };
    let first = SessionActivityPage {
        own: activity(root, SessionActivityState::Idle),
        descendants: vec![activity(child, SessionActivityState::PermissionWait)],
        next_after: Some(child),
        coverage: SessionActivityCoverage::Complete,
        observed_at_ms: 1,
        issues: vec![],
    };
    let mut second = first.clone();
    second.descendants[0].states = vec![SessionActivityState::Failed];
    second.observed_at_ms = 2;
    second.next_after = None;
    let combined = crate::child_activity::combine_activity_windows(vec![first, second]).unwrap();
    assert_eq!(combined.descendants.len(), 1);
    assert_eq!(
        combined.descendants[0].states,
        [SessionActivityState::Failed]
    );
    assert_eq!(combined.coverage, SessionActivityCoverage::Partial);
    assert_eq!(combined.observed_at_ms, 2);
    assert!(crate::child_activity::combine_activity_windows(Vec::new()).is_none());
}

#[test]
fn concurrent_attention_evidence_and_newest_observation_are_truthful() {
    let id = SessionId::new();
    let idle = SessionActivity {
        session_id: id,
        parent_id: None,
        states: vec![SessionActivityState::Idle],
        evidence: None,
    };
    let waiting = SessionActivity {
        session_id: id,
        parent_id: None,
        states: vec![
            SessionActivityState::Idle,
            SessionActivityState::PermissionWait,
        ],
        evidence: Some(SessionActivityEvidence {
            running: 0,
            queued: 0,
            permission_waits: 2,
            input_waits: 0,
            counts_complete: false,
            observed_event_sequence: Some(7),
            current_operation: Some("review tools".to_owned()),
        }),
    };
    assert_eq!(attention_rank(&waiting), 0);
    assert_eq!(attention_rank(&idle), 4);
    let older = SessionActivityPage {
        own: idle,
        descendants: vec![],
        next_after: None,
        coverage: SessionActivityCoverage::Complete,
        observed_at_ms: 1000,
        issues: vec![],
    };
    let newer = SessionActivityPage {
        own: waiting,
        descendants: vec![],
        next_after: None,
        coverage: SessionActivityCoverage::Partial,
        observed_at_ms: 2000,
        issues: vec![],
    };
    let merged = newest_observations([&newer, &older]);
    assert_eq!(merged[&id].1, 2000);
    let label = crate::child_activity::activity_evidence_label(merged[&id].0);
    assert!(label.contains("idle (not done)"));
    assert!(label.contains("2 permission"));
    assert!(label.contains("known/partial"));
    assert_eq!(observation_age_label(1000, 15000), "stale · 14s ago");
}
