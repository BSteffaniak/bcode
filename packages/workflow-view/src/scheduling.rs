//! Bounded projection of scheduler-owned observations, without re-evaluating admission.

use bcode_workflow_view_models::{
    WorkflowSchedulingActivation, WorkflowSchedulingBlocker, WorkflowSchedulingPolicy,
    WorkflowSchedulingView,
};

/// Application-supplied bounded canonical observations. Unknown eligibility must remain unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowSchedulingObservation {
    pub graph_revision: u64,
    pub observed_at_ms: u64,
    pub policy: Option<WorkflowSchedulingPolicy>,
    pub activations: Vec<WorkflowSchedulingActivation>,
    pub has_more: bool,
}

/// Project at most `limit` (at most 100) activation observations and eight reasons each.
/// Truncated identifiers are display-only and must not be used as action targets.
#[must_use]
pub fn project_scheduling(
    observation: WorkflowSchedulingObservation,
    limit: usize,
) -> WorkflowSchedulingView {
    let limit = limit.min(100);
    let has_more = observation.has_more || observation.activations.len() > limit;
    let mut details_truncated = false;
    let activations = observation
        .activations
        .into_iter()
        .take(limit)
        .map(|mut activation| {
            bound_text(&mut activation.node_id, &mut details_truncated);
            bound_text(&mut activation.activation_id, &mut details_truncated);
            details_truncated |= activation.reasons.len() > 8;
            activation.reasons.truncate(8);
            for reason in &mut activation.reasons {
                if let WorkflowSchedulingBlocker::Resource { resource } = reason {
                    bound_text(resource, &mut details_truncated);
                }
            }
            activation
        })
        .collect();
    WorkflowSchedulingView {
        graph_revision: observation.graph_revision,
        observed_at_ms: observation.observed_at_ms,
        policy: observation.policy,
        activations,
        has_more,
        details_truncated,
    }
}

/// Classify one activation using same-snapshot canonical facts, never a partial attempt history.
///
/// `latest_attempt` must be the latest attempt for this exact activation (or None if not
/// observed), not merely the latest row in a truncated history page. `retry_at_ms` must
/// be the current unconsumed retry schedule. A running result describes the durable owner
/// observation, not proof that a provider process is still live. Pending never implies ready.
#[must_use]
pub fn classify_scheduling_activation(
    activation: &crate::WorkflowActivationProjectionInput,
    latest_attempt: Option<&bcode_workflow_view_models::WorkflowAttemptView>,
    paused: bool,
    retry_at_ms: Option<u64>,
    observed_at_ms: u64,
) -> WorkflowSchedulingActivation {
    use bcode_workflow_view_models::{WorkflowPolicyBlocker, WorkflowSchedulingState};
    let mut result = WorkflowSchedulingActivation {
        node_id: activation.node_id.clone(),
        activation_id: activation.activation_id.clone(),
        state: WorkflowSchedulingState::Unknown,
        reasons: Vec::new(),
    };
    // Terminal/unsupported activation states must not be reopened by stale attempt facts.
    if !matches!(
        activation.status.as_str(),
        "pending"
            | "running"
            | "waiting_input"
            | "waiting_approval"
            | "waiting_mutation_approval"
            | "retry_scheduled"
    ) {
        return result;
    }
    let attempt = latest_attempt.filter(|attempt| {
        attempt.node_id == activation.node_id && attempt.activation_id == activation.activation_id
    });
    if matches!(activation.status.as_str(), "pending" | "running")
        && attempt.is_some_and(|attempt| {
            attempt.terminal_at_ms.is_none()
                && attempt.has_receipt
                && matches!(attempt.status.as_str(), "admitted" | "running")
        })
    {
        result.state = WorkflowSchedulingState::Running;
        return result;
    }
    let reason = match activation.status.as_str() {
        "waiting_input" => Some(WorkflowPolicyBlocker::Input),
        "waiting_approval" | "waiting_mutation_approval" => Some(WorkflowPolicyBlocker::Approval),
        _ if retry_at_ms.is_some_and(|due| due > observed_at_ms) => {
            Some(WorkflowPolicyBlocker::Retry)
        }
        _ if paused => Some(WorkflowPolicyBlocker::Paused),
        _ => None,
    };
    if let Some(reason) = reason {
        result.state = WorkflowSchedulingState::Blocked;
        result
            .reasons
            .push(WorkflowSchedulingBlocker::Policy { reason });
    }
    result
}

fn bound_text(text: &mut String, truncated: &mut bool) {
    if let Some((boundary, _)) = text.char_indices().nth(256) {
        text.truncate(boundary);
        *truncated = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_workflow_view_models::{WorkflowCapacityScope, WorkflowSchedulingState};

    fn observation(count: usize) -> WorkflowSchedulingObservation {
        WorkflowSchedulingObservation {
            graph_revision: 7,
            observed_at_ms: 42,
            policy: None,
            activations: (0..count)
                .map(|index| WorkflowSchedulingActivation {
                    node_id: format!("node-{index}"),
                    activation_id: format!("activation-{index}"),
                    state: WorkflowSchedulingState::Unknown,
                    reasons: vec![],
                })
                .collect(),
            has_more: false,
        }
    }

    #[test]
    fn bounded_pages_never_invent_readiness_or_totals() {
        for count in [32, 100, 1000] {
            let view = project_scheduling(observation(count), 17);
            assert_eq!(view.activations.len(), 17);
            assert!(view.has_more);
            assert!(view.policy.is_none());
            assert!(
                view.activations
                    .iter()
                    .all(|item| item.state == WorkflowSchedulingState::Unknown)
            );
        }
        assert!(project_scheduling(observation(1), 0).has_more);
        assert_eq!(
            project_scheduling(observation(1000), usize::MAX)
                .activations
                .len(),
            100
        );
    }

    #[test]
    fn typed_reasons_round_trip_and_unknown_variants_fail_closed() {
        use bcode_workflow_view_models::{WorkflowBackpressureKind, WorkflowPolicyBlocker};
        let mut input = observation(3);
        input.activations[0].state = WorkflowSchedulingState::Ready;
        input.activations[1].state = WorkflowSchedulingState::Running;
        input.activations[2].state = WorkflowSchedulingState::Blocked;
        input.activations[2].reasons = vec![
            WorkflowSchedulingBlocker::Dependency { remaining: 2 },
            WorkflowSchedulingBlocker::Policy {
                reason: WorkflowPolicyBlocker::Approval,
            },
            WorkflowSchedulingBlocker::Resource {
                resource: "workspace".into(),
            },
            WorkflowSchedulingBlocker::Capacity {
                scope: WorkflowCapacityScope::Run,
                running: 8,
                limit: 8,
            },
            WorkflowSchedulingBlocker::Backpressure {
                reason: WorkflowBackpressureKind::Dispatch,
            },
        ];
        let view = project_scheduling(input, 10);
        let encoded = serde_json::to_value(&view).unwrap();
        assert_eq!(
            serde_json::from_value::<WorkflowSchedulingView>(encoded.clone()).unwrap(),
            view
        );
        let lines = view.summary_lines().join("\n");
        for label in [
            "Ready",
            "Running",
            "Blocked",
            "Dependency",
            "Policy",
            "Resource conflict",
            "Capacity",
            "Backpressure",
        ] {
            assert!(lines.contains(label), "missing {label}");
        }
        let mut future = encoded;
        future["activations"][0]["state"] = serde_json::json!("future_state");
        assert!(serde_json::from_value::<WorkflowSchedulingView>(future).is_err());
    }

    #[test]
    fn classification_requires_exact_current_attempt_and_preserves_terminal_state() {
        use bcode_workflow_view_models::{
            WorkflowAttemptView, WorkflowInputSummary, WorkflowPolicyBlocker,
        };
        let mut activation = crate::WorkflowActivationProjectionInput {
            node_id: "n".into(),
            activation_id: "a".into(),
            dependency_generation: 1,
            status: "pending".into(),
            has_output: false,
            input_summary: WorkflowInputSummary::Absent,
            created_at_ms: 0,
        };
        let mut attempt = WorkflowAttemptView {
            node_id: "n".into(),
            activation_id: "a".into(),
            attempt: 1,
            dispatch_identity: "dispatch".into(),
            status: "running".into(),
            has_receipt: true,
            prepared_at_ms: 0,
            admitted_at_ms: Some(1),
            terminal_at_ms: None,
        };
        assert_eq!(
            classify_scheduling_activation(&activation, None, false, None, 10).state,
            WorkflowSchedulingState::Unknown
        );
        assert_eq!(
            classify_scheduling_activation(&activation, Some(&attempt), true, None, 10).state,
            WorkflowSchedulingState::Running
        );
        attempt.activation_id = "other".into();
        assert_eq!(
            classify_scheduling_activation(&activation, Some(&attempt), false, None, 10).state,
            WorkflowSchedulingState::Unknown
        );
        attempt.activation_id = "a".into();
        attempt.terminal_at_ms = Some(9);
        assert_eq!(
            classify_scheduling_activation(&activation, Some(&attempt), false, None, 10).state,
            WorkflowSchedulingState::Unknown
        );
        attempt.terminal_at_ms = None;
        activation.status = "completed".into();
        assert_eq!(
            classify_scheduling_activation(&activation, Some(&attempt), true, None, 10).state,
            WorkflowSchedulingState::Unknown
        );
        activation.status = "pending".into();
        let retry = classify_scheduling_activation(&activation, None, false, Some(11), 10);
        assert_eq!(retry.state, WorkflowSchedulingState::Blocked);
        assert_eq!(
            retry.reasons,
            vec![WorkflowSchedulingBlocker::Policy {
                reason: WorkflowPolicyBlocker::Retry
            }]
        );
        assert_eq!(
            classify_scheduling_activation(&activation, None, false, Some(10), 10).state,
            WorkflowSchedulingState::Unknown
        );
        for status in [
            "waiting_approval",
            "waiting_mutation_approval",
            "waiting_input",
        ] {
            activation.status = status.into();
            assert_eq!(
                classify_scheduling_activation(&activation, None, false, None, 10).state,
                WorkflowSchedulingState::Blocked
            );
        }
        activation.status = "future".into();
        assert_eq!(
            classify_scheduling_activation(&activation, Some(&attempt), true, None, 10).state,
            WorkflowSchedulingState::Unknown
        );
    }

    #[test]
    fn uncapped_and_unknown_remain_distinct() {
        let mut input = observation(1);
        input.policy = Some(WorkflowSchedulingPolicy {
            max_running: None,
            running: 100,
            scope: WorkflowCapacityScope::Run,
        });
        let view = project_scheduling(input, 10);
        assert_eq!(view.policy.unwrap().max_running, None);
        assert_eq!(view.activations[0].state, WorkflowSchedulingState::Unknown);
    }

    #[test]
    fn reasons_and_unicode_are_bounded_without_losing_state() {
        let mut input = observation(1);
        input.activations[0].state = WorkflowSchedulingState::Blocked;
        input.activations[0].reasons = vec![
            WorkflowSchedulingBlocker::Resource {
                resource: "界".repeat(1000)
            };
            20
        ];
        input.has_more = true;
        let view = project_scheduling(input, 10);
        assert!(view.details_truncated);
        assert!(view.has_more);
        assert_eq!(view.activations[0].state, WorkflowSchedulingState::Blocked);
        assert_eq!(view.activations[0].reasons.len(), 8);
        assert_eq!(
            view.activations[0].reasons[0],
            WorkflowSchedulingBlocker::Resource {
                resource: "界".repeat(256)
            }
        );
    }
}
