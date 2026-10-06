//! Renderer-neutral attention and freshness policy for bounded child observations.
#[cfg(test)]
#[path = "child_attention_tests.rs"]
mod tests;

use bcode_session_models::{SessionActivity, SessionActivityPage, SessionActivityState, SessionId};
use std::collections::BTreeMap;

/// Attention order: blocked/error work precedes ordinary active work, then unknown and idle.
/// Coexisting facts are retained; this ordering is not a scalar execution outcome.
#[must_use]
pub fn attention_rank(activity: &SessionActivity) -> u8 {
    activity
        .states
        .iter()
        .map(|state| match state {
            SessionActivityState::PermissionWait | SessionActivityState::InputWait => 0,
            SessionActivityState::Failed | SessionActivityState::Interrupted => 1,
            SessionActivityState::Running | SessionActivityState::Queued => 2,
            SessionActivityState::Unknown => 3,
            SessionActivityState::Idle => 4,
        })
        .min()
        .unwrap_or(3)
}

/// Merge newest evidence by observation time, without deriving completion from idle.
#[must_use]
pub fn newest_observations<'a>(
    pages: impl IntoIterator<Item = &'a SessionActivityPage>,
) -> BTreeMap<SessionId, (&'a SessionActivity, u64)> {
    let mut observations = BTreeMap::new();
    for page in pages {
        for activity in std::iter::once(&page.own).chain(page.descendants.iter()) {
            let observed = observations
                .entry(activity.session_id)
                .or_insert((activity, page.observed_at_ms));
            if page.observed_at_ms >= observed.1 {
                *observed = (activity, page.observed_at_ms);
            }
        }
    }
    observations
}

/// Bounded parent bird's-eye summary. Counts describe observed sessions, not all descendants.
#[must_use]
pub fn overview_label<'a>(
    root: &SessionActivityPage,
    pages: impl IntoIterator<Item = &'a SessionActivityPage>,
) -> String {
    let observations = newest_observations(pages);
    let mut active = 0;
    let mut idle = 0;
    let mut attention = 0;
    let mut unknown = 0;
    for (id, (activity, _)) in observations {
        if id == root.own.session_id {
            continue;
        }
        if attention_rank(activity) <= 1 {
            attention += 1;
        }
        if crate::child_activity::has_active_evidence(activity) {
            active += 1;
        } else if activity.states.contains(&SessionActivityState::Idle) {
            idle += 1;
        } else {
            unknown += 1;
        }
    }
    format!(
        "Attention {attention} · {active} known active · {idle} observed idle (not done) · {unknown} unknown · descendants partial/unscanned"
    )
}

/// Freshness is presentation evidence, not durable execution authority.
#[must_use]
pub fn observation_age_label(observed_at_ms: u64, now_ms: u64) -> String {
    if observed_at_ms == 0 {
        return "observation time unknown".to_owned();
    }
    let seconds = now_ms.saturating_sub(observed_at_ms) / 1000;
    if seconds > 10 {
        format!("stale · {seconds}s ago")
    } else {
        format!("observed {seconds}s ago")
    }
}
