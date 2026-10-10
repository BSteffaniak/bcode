//! Bounded public application observations for agent inactivity accounting.
use super::{EvalError, Instant};
use bcode_ipc::SessionProgressSnapshot;
use bcode_session_models::{
    SessionActivityCoverage, SessionActivityQuery, SessionActivityScope, SessionActivityState,
    SessionChildQuery, SessionId,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, PartialEq, Eq)]
enum ChildPageReadiness {
    Complete,
    Retry,
    Exhausted,
}

fn child_page_readiness(
    page: &bcode_session_models::SessionChildPage,
    elapsed: std::time::Duration,
    allowance: std::time::Duration,
) -> ChildPageReadiness {
    if page.complete && page.issues.is_empty() {
        ChildPageReadiness::Complete
    } else if elapsed < allowance {
        ChildPageReadiness::Retry
    } else {
        ChildPageReadiness::Exhausted
    }
}

fn child_issue_category(issue: &str) -> &'static str {
    match issue {
        "parent child publication is pending" => "parent_publication_pending",
        "parent child publication is pending or historical coverage requires explicit reconciliation" => {
            "parent_pending_or_historical"
        }
        "parent child index is missing, stale, or corrupt; explicit maintenance required" => {
            "parent_index_unavailable"
        }
        _ if issue.starts_with("candidate ")
            && issue.ends_with(" relationship disagrees with canonical projection") =>
        {
            "child_relationship_mismatch"
        }
        _ if issue.starts_with("candidate ")
            && issue.ends_with(" metadata is unavailable or requires maintenance") =>
        {
            "child_metadata_unavailable"
        }
        _ => "unclassified_child_issue",
    }
}

fn child_page_diagnostic(page: &bcode_session_models::SessionChildPage) -> String {
    let categories: BTreeSet<_> = page
        .issues
        .iter()
        .map(|issue| child_issue_category(issue))
        .collect();
    format!(
        "agent child discovery incomplete: complete={} issue_count={} categories={}",
        page.complete,
        page.issues.len(),
        categories.into_iter().collect::<Vec<_>>().join(",")
    )
}

fn temporarily_unavailable(error: &bcode_client::ClientError, code: &str) -> bool {
    matches!(error, bcode_client::ClientError::Server { code: actual, .. } if actual == code)
}

const fn missing_progress_observation(snapshot: &SessionProgressSnapshot) -> bool {
    snapshot.version == bcode_workflow_view_models::SESSION_PROGRESS_VERSION
        && matches!(
        snapshot.observation,
        bcode_workflow_view_models::SessionProgressObservation::Unavailable {
            reason: bcode_workflow_view_models::SessionProgressUnavailableReason::NotObservedOrEvicted
        }
    )
}

const fn progress_unavailable_category(snapshot: &SessionProgressSnapshot) -> &'static str {
    if snapshot.version != bcode_workflow_view_models::SESSION_PROGRESS_VERSION {
        return "unsupported_progress_version";
    }
    match &snapshot.observation {
        bcode_workflow_view_models::SessionProgressObservation::Unavailable { reason } => match reason {
            bcode_workflow_view_models::SessionProgressUnavailableReason::NotObservedOrEvicted => "not_observed_or_evicted",
            bcode_workflow_view_models::SessionProgressUnavailableReason::CoverageUnavailable => "coverage_unavailable",
        },
        bcode_workflow_view_models::SessionProgressObservation::Available { .. } => "invalid_progress_marker",
    }
}

fn may_retry_observation(elapsed: std::time::Duration, allowance: std::time::Duration) -> bool {
    elapsed < allowance
}

fn may_wait_for_initial_coverage(
    initial: bool,
    elapsed: std::time::Duration,
    allowance: std::time::Duration,
) -> bool {
    initial && elapsed < allowance
}

pub async fn observe(
    client: &bcode_client::BcodeClient,
    root: SessionId,
    previous: &mut BTreeMap<SessionId, SessionProgressSnapshot>,
    last_progress: &mut Instant,
    scan_allowance: std::time::Duration,
    initial_coverage_allowance: std::time::Duration,
) -> Result<(), EvalError> {
    if scan_allowance.is_zero() || initial_coverage_allowance.is_zero() {
        return Err(EvalError::Validation(
            "agent activity scan allowance must be positive".into(),
        ));
    }
    let initial_acquisition = previous.is_empty();
    let acquisition_start = Instant::now();
    let observation_allowance = if initial_acquisition {
        scan_allowance.max(initial_coverage_allowance)
    } else {
        scan_allowance
    };
    let last_coverage_diagnostic = std::sync::Mutex::new(None::<String>);
    let scan = async {
        let mut pending = VecDeque::from([root]);
        let mut visited = BTreeSet::new();
        while let Some(session) = pending.pop_front() {
            if !visited.insert(session) {
                return Err(EvalError::Validation(
                    "ambiguous session descendant discovery".into(),
                ));
            }
            let initial = !previous.contains_key(&session);
            let activity = loop {
                let activity = match client
                    .session_activity(
                        session,
                        SessionActivityQuery {
                            scope: SessionActivityScope::Own,
                            after: None,
                            limit: 100,
                        },
                    )
                    .await
                {
                    Ok(activity) => activity,
                    Err(error)
                        if temporarily_unavailable(
                            &error,
                            "session_activity_temporarily_unavailable",
                        ) && may_retry_observation(
                            acquisition_start.elapsed(),
                            scan_allowance,
                        ) =>
                    {
                        *last_coverage_diagnostic.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some("session_activity_temporarily_unavailable: normalized transient observation failure".into());
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                    Err(error) => return Err(EvalError::Client(error.to_string())),
                };
                if activity.own.session_id != session
                    || activity.next_after.is_some()
                    || !activity.descendants.is_empty()
                {
                    return Err(EvalError::Validation(
                        "agent own-activity identity or scope mismatch".into(),
                    ));
                }
                let complete = activity.coverage == SessionActivityCoverage::Complete
                    && activity.issues.is_empty()
                    && !activity.own.states.is_empty()
                    && !activity.own.states.contains(&SessionActivityState::Unknown)
                    && activity
                        .own
                        .evidence
                        .as_ref()
                        .is_some_and(|evidence| evidence.counts_complete);
                if complete {
                    break activity;
                }
                let diagnostic = format!(
                    "agent descendant activity coverage unavailable: initial_baseline={initial}, coverage={:?}, states={:?}, issue_count={}, evidence={:?}",
                    activity.coverage,
                    activity.own.states,
                    activity.issues.len(),
                    activity.own.evidence.as_ref().map(|evidence| (
                        evidence.running,
                        evidence.queued,
                        evidence.permission_waits,
                        evidence.input_waits,
                        evidence.counts_complete,
                        evidence.observed_event_sequence,
                    )),
                );
                *last_coverage_diagnostic
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(diagnostic.clone());
                if !may_wait_for_initial_coverage(
                    initial,
                    acquisition_start.elapsed(),
                    initial_coverage_allowance,
                ) {
                    return Err(EvalError::Validation(diagnostic));
                }
                // Observation retries are not substantive progress. In particular,
                // never move last_progress or classify silence while coverage is unknown.
                tokio::time::sleep(
                    std::time::Duration::from_millis(100).min(
                        initial_coverage_allowance.saturating_sub(acquisition_start.elapsed()),
                    ),
                )
                .await;
            };
            let settled = activity.own.states.iter().all(|state| {
                matches!(
                    state,
                    SessionActivityState::Idle
                        | SessionActivityState::Failed
                        | SessionActivityState::Interrupted
                )
            }) && activity.own.evidence.as_ref().is_some_and(|evidence| {
                evidence.running == 0
                    && evidence.queued == 0
                    && evidence.permission_waits == 0
                    && evidence.input_waits == 0
            });
            if settled {
                previous.remove(&session);
            } else {
                let snapshot = loop {
                    let snapshot = client
                        .session_progress(session)
                        .await
                        .map_err(|error| EvalError::Client(error.to_string()))?;
                    if snapshot.session_id != session.to_string() {
                        return Err(EvalError::Validation(
                            "agent progress identity mismatch".into(),
                        ));
                    }
                    if snapshot.advanced_since(&snapshot).is_some() {
                        break snapshot;
                    }
                    let reason = progress_unavailable_category(&snapshot);
                    let diagnostic = format!(
                        "agent progress coverage unavailable: {reason}; session={session}; activity_observed_at_ms={}; progress_version={}",
                        activity.observed_at_ms, snapshot.version
                    );
                    *last_coverage_diagnostic
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(diagnostic.clone());
                    if missing_progress_observation(&snapshot)
                        && may_wait_for_initial_coverage(
                            initial,
                            acquisition_start.elapsed(),
                            initial_coverage_allowance,
                        )
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                    return Err(EvalError::Validation(diagnostic));
                };
                if let Some(before) = previous.get(&session) {
                    match snapshot.advanced_since(before) {
                        Some(true) => *last_progress = Instant::now(),
                        Some(false) => {}
                        None => {
                            return Err(EvalError::Validation(
                                "agent progress continuity unavailable".into(),
                            ));
                        }
                    }
                }
                previous.insert(session, snapshot);
            }
            let mut after = None;
            loop {
                let page = match client
                    .query_child_sessions(SessionChildQuery {
                        parent_id: session,
                        after,
                        limit: 100,
                    })
                    .await
                {
                    Ok(page) => page,
                    Err(error)
                        if temporarily_unavailable(
                            &error,
                            "session_children_temporarily_unavailable",
                        ) && may_retry_observation(
                            acquisition_start.elapsed(),
                            scan_allowance,
                        ) =>
                    {
                        *last_coverage_diagnostic.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some("session_children_temporarily_unavailable: normalized transient observation failure".into());
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                    Err(error) => return Err(EvalError::Client(error.to_string())),
                };
                if page
                    .children
                    .iter()
                    .any(|child| child.parent_id != Some(session))
                {
                    return Err(EvalError::Validation("agent child parent mismatch".into()));
                }
                if page
                    .next_after
                    .is_some_and(|next| after.is_some_and(|before| next <= before))
                {
                    return Err(EvalError::Validation(
                        "agent child cursor did not advance".into(),
                    ));
                }
                match child_page_readiness(&page, acquisition_start.elapsed(), scan_allowance) {
                    ChildPageReadiness::Complete => {}
                    ChildPageReadiness::Retry => {
                        *last_coverage_diagnostic
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(child_page_diagnostic(&page));
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                    ChildPageReadiness::Exhausted => {
                        return Err(EvalError::Validation(format!(
                            "{}: scan allowance exhausted",
                            child_page_diagnostic(&page)
                        )));
                    }
                }
                for child in page.children {
                    if child.parent_id != Some(session) {
                        return Err(EvalError::Validation("agent child parent mismatch".into()));
                    }
                    pending.push_back(child.id);
                }
                match page.next_after {
                    Some(next) if after.is_none_or(|before| next > before) => after = Some(next),
                    None => break,
                    Some(_) => {
                        return Err(EvalError::Validation(
                            "agent child cursor did not advance".into(),
                        ));
                    }
                }
            }
        }
        previous.retain(|id, _| visited.contains(id));
        Ok(())
    };
    tokio::time::timeout(observation_allowance, scan)
        .await
        .map_err(|_| {
            EvalError::Validation(format!("agent activity observation allowance exhausted; coverage unverified, not idle; last_public_observation={}",
                last_coverage_diagnostic.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_deref().unwrap_or("no activity response received")))
        })?
}

#[cfg(test)]
mod tests {
    use super::may_wait_for_initial_coverage;
    use std::time::Duration;

    #[test]
    fn unavailable_progress_categories_preserve_contract_distinctions() {
        let snapshot = |version: u32, observation: serde_json::Value| {
            serde_json::from_value::<bcode_ipc::SessionProgressSnapshot>(serde_json::json!({
                "version": version, "session_id": "session", "observation": observation
            }))
            .unwrap()
        };
        for (reason, expected) in [
            ("coverage_unavailable", "coverage_unavailable"),
            ("not_observed_or_evicted", "not_observed_or_evicted"),
        ] {
            let value = snapshot(
                2,
                serde_json::json!({"status":"unavailable","reason":reason}),
            );
            assert_eq!(super::progress_unavailable_category(&value), expected);
            assert_eq!(value.advanced_since(&value), None);
        }
        let future = snapshot(
            99,
            serde_json::json!({"status":"unavailable","reason":"not_observed_or_evicted"}),
        );
        assert_eq!(
            super::progress_unavailable_category(&future),
            "unsupported_progress_version"
        );
        assert!(!super::missing_progress_observation(&future));
    }

    #[test]
    fn only_exact_normalized_projection_observation_codes_retry() {
        let error = |code: &str| bcode_client::ClientError::Server {
            code: code.into(),
            message: "opaque".into(),
        };
        assert!(super::temporarily_unavailable(
            &error("session_activity_temporarily_unavailable"),
            "session_activity_temporarily_unavailable"
        ));
        for code in [
            "session_activity_unavailable",
            "unsupported_version",
            "session_not_found",
            "corrupt_state",
        ] {
            assert!(!super::temporarily_unavailable(
                &error(code),
                "session_activity_temporarily_unavailable"
            ));
        }
    }

    #[test]
    fn only_missing_first_progress_observation_is_waitable() {
        let snapshot = |reason: &str| {
            serde_json::from_value::<bcode_ipc::SessionProgressSnapshot>(serde_json::json!({
                "version": 2, "session_id": "session",
                "observation": { "status": "unavailable", "reason": reason }
            }))
            .unwrap()
        };
        assert!(super::missing_progress_observation(&snapshot(
            "not_observed_or_evicted"
        )));
        assert!(!super::missing_progress_observation(&snapshot(
            "coverage_unavailable"
        )));
    }

    #[test]
    fn child_page_readiness_recovers_only_complete_pages_and_exhausts_persistent_gaps() {
        use super::{ChildPageReadiness, child_page_readiness};
        let mut page = bcode_session_models::SessionChildPage {
            complete: false,
            children: Vec::new(),
            next_after: None,
            issues: Vec::new(),
        };
        let allowance = Duration::from_secs(30);
        assert_eq!(
            child_page_readiness(&page, Duration::ZERO, allowance),
            ChildPageReadiness::Retry
        );
        assert_eq!(
            child_page_readiness(&page, allowance, allowance),
            ChildPageReadiness::Exhausted
        );
        page.complete = true;
        assert_eq!(
            child_page_readiness(&page, Duration::from_secs(1), allowance),
            ChildPageReadiness::Complete
        );
        page.issues.push("unverified".into());
        assert_eq!(
            child_page_readiness(&page, allowance, allowance),
            ChildPageReadiness::Exhausted
        );
        // Readiness classification cannot advance progress counters or the idle clock.
    }

    #[test]
    fn child_page_diagnostic_retains_coverage_without_untrusted_issue_payloads() {
        let page = bcode_session_models::SessionChildPage {
            complete: false,
            children: Vec::new(),
            next_after: None,
            issues: vec![
                "parent child index is missing, stale, or corrupt; explicit maintenance required"
                    .into(),
                "candidate private-id metadata is unavailable or requires maintenance".into(),
                "secret/path arbitrary backend details".into(),
            ],
        };
        let diagnostic = super::child_page_diagnostic(&page);
        assert!(diagnostic.contains("complete=false issue_count=3"));
        assert!(diagnostic.contains("parent_index_unavailable"));
        assert!(diagnostic.contains("child_metadata_unavailable"));
        assert!(diagnostic.contains("unclassified_child_issue"));
        assert!(!diagnostic.contains("private-id"));
        assert!(!diagnostic.contains("secret/path"));
    }

    #[test]
    fn temporary_observation_retry_uses_each_scan_budget() {
        let allowance = Duration::from_secs(3);
        assert!(super::may_retry_observation(Duration::ZERO, allowance));
        assert!(super::may_retry_observation(
            Duration::from_secs(2),
            allowance
        ));
        assert!(!super::may_retry_observation(allowance, allowance));
    }

    #[test]
    fn initial_unknown_coverage_wait_is_bounded_and_not_recovery_of_known_gaps() {
        let allowance = Duration::from_secs(30);
        assert!(may_wait_for_initial_coverage(
            true,
            Duration::ZERO,
            allowance
        ));
        assert!(may_wait_for_initial_coverage(
            true,
            Duration::from_secs(29),
            allowance
        ));
        assert!(!may_wait_for_initial_coverage(true, allowance, allowance));
        assert!(!may_wait_for_initial_coverage(
            false,
            Duration::ZERO,
            allowance
        ));
    }
}
