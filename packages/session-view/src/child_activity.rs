//! Truthful activity labels from application-normalized evidence, never runtime logs.

use bcode_session_models::{
    SessionActivity, SessionActivityCoverage, SessionActivityPage, SessionActivityState,
};

/// Whether observed evidence indicates active or waiting work (idle is not completion).
#[must_use]
pub fn has_active_evidence(activity: &SessionActivity) -> bool {
    activity.states.iter().any(|state| {
        matches!(
            state,
            SessionActivityState::Running
                | SessionActivityState::Queued
                | SessionActivityState::PermissionWait
                | SessionActivityState::InputWait
        )
    })
}

/// Compact count describes only the bounded observed page, not an inferred subtree total.
#[must_use]
pub fn child_activity_label(page: &SessionActivityPage) -> String {
    let active = page
        .descendants
        .iter()
        .filter(|activity| has_active_evidence(activity))
        .count();
    let coverage = match page.coverage {
        SessionActivityCoverage::Complete if page.next_after.is_none() => {
            "direct children observed"
        }
        SessionActivityCoverage::Complete | SessionActivityCoverage::Partial => "partial",
        SessionActivityCoverage::Degraded => "degraded",
    };
    format!("children: {active} known active · {coverage}")
}

/// Independent runtime evidence; no assignment acceptance or completion is invented.
#[must_use]
pub fn activity_evidence_label(activity: &SessionActivity) -> String {
    if activity.states.is_empty() {
        return "unknown".to_owned();
    }
    activity
        .states
        .iter()
        .map(|state| match state {
            SessionActivityState::Running => "running",
            SessionActivityState::Queued => "queued",
            SessionActivityState::PermissionWait => "permission wait",
            SessionActivityState::InputWait => "input wait",
            SessionActivityState::Idle => "idle (not done)",
            SessionActivityState::Failed => "failed",
            SessionActivityState::Interrupted => "interrupted",
            SessionActivityState::Unknown => "unknown",
        })
        .collect::<Vec<_>>()
        .join(", ")
}
