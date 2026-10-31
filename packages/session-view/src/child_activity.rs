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

/// Combine a watch's loaded windows without retaining observations from an older
/// delivery. Continuations never claim whole-scope completeness on their own.
#[must_use]
pub fn combine_activity_windows(
    mut pages: Vec<SessionActivityPage>,
) -> Option<SessionActivityPage> {
    if pages.is_empty() {
        return None;
    }
    let mut page = pages.remove(0);
    let partial = page.next_after.is_some()
        || pages.iter().any(|window| {
            window.coverage != SessionActivityCoverage::Complete || window.next_after.is_some()
        });
    let mut observations = std::collections::BTreeMap::new();
    for window in std::iter::once(&page).chain(pages.iter()) {
        for activity in &window.descendants {
            let entry = observations
                .entry(activity.session_id)
                .or_insert_with(|| (window.observed_at_ms, activity.clone()));
            if window.observed_at_ms >= entry.0 {
                *entry = (window.observed_at_ms, activity.clone());
            }
        }
    }
    for window in pages {
        if window.observed_at_ms >= page.observed_at_ms {
            page.own = window.own;
            page.observed_at_ms = window.observed_at_ms;
        }
        page.issues.extend(window.issues);
        if window.coverage == SessionActivityCoverage::Degraded {
            page.coverage = SessionActivityCoverage::Degraded;
        }
    }
    page.descendants = observations
        .into_values()
        .map(|(_, activity)| activity)
        .collect();
    if partial && page.coverage == SessionActivityCoverage::Complete {
        page.coverage = SessionActivityCoverage::Partial;
    }
    Some(page)
}

/// Independent runtime evidence; no assignment acceptance or completion is invented.
#[must_use]
pub fn activity_evidence_label(activity: &SessionActivity) -> String {
    if activity.states.is_empty() {
        return "unknown".to_owned();
    }
    let states = activity
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
        .join(", ");
    activity.evidence.as_ref().map_or_else(
        || states.clone(),
        |evidence| {
            let counts = format!(
                "{} running · {} queued · {} permission · {} input{}",
                evidence.running,
                evidence.queued,
                evidence.permission_waits,
                evidence.input_waits,
                if evidence.counts_complete {
                    ""
                } else {
                    " known/partial"
                }
            );
            if evidence.counts_complete
                || evidence.running > 0
                || evidence.queued > 0
                || evidence.permission_waits > 0
                || evidence.input_waits > 0
            {
                format!("{states} · {counts}")
            } else {
                format!("{states} · counts unknown")
            }
        },
    )
}
