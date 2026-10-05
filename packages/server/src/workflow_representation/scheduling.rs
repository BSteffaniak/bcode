//! Representation-aware scheduling cursor validation. Observation facts remain store-owned.
use super::{ServerError, unavailable};
use bcode_workflow::WorkflowSchedulingPageRequest;

/// Cursor encoding is private to the application, not a display identity or execution authority.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u32,
    run_id: String,
    revision: u64,
    representation: u32,
    after: String,
}

fn decode(
    request: &WorkflowSchedulingPageRequest,
    representation: u32,
) -> Result<Option<Cursor>, ServerError> {
    request.validate().map_err(|_| unavailable())?;
    request
        .cursor
        .as_ref()
        .map(|token| {
            let bytes = hex::decode(token).map_err(|_| unavailable())?;
            let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
            if cursor.version != 1
                || cursor.run_id != request.run_id
                || cursor.representation != representation
                || cursor.revision == 0
                || cursor.after.is_empty()
            {
                return Err(unavailable());
            }
            Ok(cursor)
        })
        .transpose()
}

fn encode(
    run_id: &str,
    revision: u64,
    representation: u32,
    after: String,
) -> Result<String, ServerError> {
    let token = hex::encode(
        serde_json::to_vec(&Cursor {
            version: 1,
            run_id: run_id.into(),
            revision,
            representation,
            after,
        })
        .map_err(|_| unavailable())?,
    );
    if token.len() > 16384 {
        return Err(unavailable());
    }
    Ok(token)
}

/// Route canonical scheduling observations without using legacy tables for new identities.
/// # Errors
/// Rejects damaged/unsupported families, malformed/stale cursors and snapshot failures.
pub async fn page(
    state: &super::ServerState,
    request: WorkflowSchedulingPageRequest,
) -> Result<
    bcode_workflow::WorkflowSchedulingPage<bcode_workflow_view_models::WorkflowSchedulingView>,
    ServerError,
> {
    use bcode_workflow_view_models::{WorkflowCapacityScope, WorkflowSchedulingPolicy};
    request.validate().map_err(|_| unavailable())?;
    let retained = super::retained_store(state)?;
    match super::lookup_at(retained.path(), &request.run_id).await? {
        super::RunLookup::Legacy | super::RunLookup::Missing => {
            return crate::workflow_operations::scheduling_page(state, request);
        }
        super::RunLookup::Available(_) => {}
        super::RunLookup::UnsupportedRepresentation => return Err(super::unsupported()),
        super::RunLookup::Degraded => return Err(unavailable()),
    }
    let cursor = decode(&request, 2)?;
    let db = super::open(retained.path(), false)?;
    let limit = u16::try_from(request.limit).map_err(|_| unavailable())?;
    let diagnostics = super::backpressure::retry_observed(Some(&state.metrics), || {
        super::representation::diagnostics::snapshot(
            &db,
            &request.run_id,
            cursor.as_ref().map(|cursor| cursor.revision),
            cursor.as_ref().map(|cursor| cursor.after.as_str()),
            limit,
        )
    })
    .await
    .map_err(|_| unavailable())?;
    let snapshot = diagnostics.scheduling;
    let mut explanations = diagnostics
        .explanations
        .into_iter()
        .map(|explanation| (explanation.activation_id.clone(), explanation))
        .collect::<std::collections::BTreeMap<_, _>>();
    let next_cursor = snapshot
        .next_activation_id
        .map(|after| encode(&request.run_id, snapshot.revision, 2, after))
        .transpose()?;
    let activations = snapshot
        .activations
        .into_iter()
        .map(|activation| {
            let explanation = explanations.remove(&activation.activation_id);
            project_activation(activation, explanation)
        })
        .collect();
    let mut observation = project_observation(
        bcode_workflow_view::WorkflowSchedulingObservation {
            graph_revision: snapshot.revision,
            observed_at_ms: crate::current_unix_millis(),
            policy: Some(WorkflowSchedulingPolicy {
                max_running: snapshot.concurrency_cap.map(u64::from),
                running: snapshot.occupied,
                scope: WorkflowCapacityScope::Run,
            }),
            activations,
            has_more: next_cursor.is_some(),
        },
        request.limit,
    );
    observation.next_cursor.clone_from(&next_cursor);
    Ok(bcode_workflow::WorkflowSchedulingPage {
        version: bcode_workflow::WORKFLOW_SCHEDULING_PAGE_VERSION,
        run_id: request.run_id,
        graph_revision: snapshot.revision,
        observation,
        next_cursor,
    })
}

fn project_activation(
    activation: super::representation::scheduling::ActivationObservation,
    explanation: Option<super::representation::diagnostics::ActivationExplanation>,
) -> bcode_workflow_view_models::WorkflowSchedulingActivation {
    use bcode_workflow_view_models::WorkflowSchedulingActivation;

    let unresolved =
        activation.status == super::representation::scheduling::ActivationState::Unresolved;
    let admitted = activation
        .latest_attempt
        .as_ref()
        .is_some_and(|attempt| attempt.receipt_present && attempt.terminal.is_none());
    let explanation = explanation.filter(|explanation| explanation.node_id == activation.node_id);
    let eligible = explanation
        .as_ref()
        .is_some_and(|explanation| !explanation.incomplete && explanation.blockers.is_empty());
    let reasons = explanation
        .map(|explanation| {
            explanation
                .blockers
                .into_iter()
                .map(project_blocker)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let blocked = unresolved && !admitted && !reasons.is_empty();
    WorkflowSchedulingActivation {
        node_id: activation.node_id,
        activation_id: activation.activation_id,
        state: observed_state((
            unresolved,
            admitted,
            activation.latest_attempt.is_none(),
            eligible,
            blocked,
        )),
        reasons,
    }
}

fn project_observation(
    input: bcode_workflow_view::WorkflowSchedulingObservation,
    limit: usize,
) -> bcode_workflow_view_models::WorkflowSchedulingView {
    bcode_workflow_view::project_scheduling(input, limit)
}

const fn observed_state(
    facts: (bool, bool, bool, bool, bool),
) -> bcode_workflow_view_models::WorkflowSchedulingState {
    use bcode_workflow_view_models::WorkflowSchedulingState as State;
    let (unresolved, admitted, fresh, complete, blocked) = facts;
    if !unresolved {
        State::Unknown
    } else if admitted {
        State::Running
    } else if blocked {
        State::Blocked
    } else if fresh && complete {
        State::Ready
    } else {
        State::Unknown
    }
}

fn project_blocker(
    reason: super::representation::diagnostics::Blocker,
) -> bcode_workflow_view_models::WorkflowSchedulingBlocker {
    use super::representation::diagnostics::Blocker;
    use bcode_workflow_view_models::{
        WorkflowCapacityScope, WorkflowPolicyBlocker as Policy, WorkflowSchedulingBlocker as View,
    };
    match reason {
        Blocker::Admission(reason) => {
            use super::representation::attempts::admission::FreshLeafBlock as B;
            View::Policy {
                reason: match reason {
                    B::MissingAuthority => Policy::ExecutionAuthority,
                    B::Terminal => Policy::Terminal,
                    B::Activation => Policy::StaleActivation,
                    B::Paused => Policy::Paused,
                    B::Cancellation => Policy::Cancellation,
                    B::Recovery => Policy::Recovery,
                    B::ControlFailure => Policy::ControlFailure,
                    B::Concurrency => Policy::Concurrency,
                    B::Resource => Policy::ResourceConflict,
                    B::Deadline | B::Allowance | B::RootAllowance => Policy::AdmissionAllowance,
                },
            }
        }
        Blocker::Capacity { occupied, limit } => View::Capacity {
            scope: WorkflowCapacityScope::Run,
            running: occupied,
            limit: u64::from(limit),
        },
        Blocker::Resource { resource } => View::Resource { resource },
        Blocker::UserPaused => View::Policy {
            reason: Policy::Paused,
        },
        Blocker::Cancellation => View::Policy {
            reason: Policy::Cancellation,
        },
        Blocker::ControlFailure => View::Policy {
            reason: Policy::ControlFailure,
        },
        Blocker::Recovery => View::Policy {
            reason: Policy::Recovery,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_fresh_leaf_projection_reports_ready_then_resource_or_capacity() {
        use super::super::representation::{diagnostics, scheduling};
        use bcode_workflow_view_models::WorkflowSchedulingState as State;
        let activation = scheduling::ActivationObservation {
            node_id: "leaf".into(),
            activation_id: "a".into(),
            status: scheduling::ActivationState::Unresolved,
            latest_attempt: None,
        };
        let mut explanation = diagnostics::ActivationExplanation {
            node_id: "leaf".into(),
            activation_id: "a".into(),
            blockers: vec![],
            dependency_inputs_materialized: true,
            incomplete: false,
        };
        assert_eq!(
            project_activation(activation.clone(), Some(explanation.clone())).state,
            State::Ready
        );
        explanation.blockers = vec![diagnostics::Blocker::Resource {
            resource: "workspace".into(),
        }];
        assert_eq!(
            project_activation(activation.clone(), Some(explanation.clone())).state,
            State::Blocked
        );
        explanation.blockers = vec![diagnostics::Blocker::Capacity {
            occupied: 2,
            limit: 2,
        }];
        assert_eq!(
            project_activation(activation.clone(), Some(explanation.clone())).state,
            State::Blocked
        );
        explanation.blockers.clear();
        explanation.incomplete = true;
        assert_eq!(
            project_activation(activation, Some(explanation)).state,
            State::Unknown
        );
    }

    #[test]
    fn ready_requires_complete_fresh_observation_and_never_overrides_blockers() {
        use bcode_workflow_view_models::WorkflowSchedulingState as State;
        assert_eq!(
            observed_state((true, false, true, true, false)),
            State::Ready
        );
        assert_eq!(
            observed_state((true, false, true, false, false)),
            State::Unknown
        );
        assert_eq!(
            observed_state((true, false, true, true, true)),
            State::Blocked
        );
        assert_eq!(
            observed_state((true, false, false, true, false)),
            State::Unknown
        );
        assert_eq!(
            observed_state((true, true, false, true, false)),
            State::Running
        );
        assert_eq!(
            observed_state((false, true, false, true, true)),
            State::Unknown
        );
    }

    #[test]
    fn canonical_blockers_survive_public_json_and_shared_ui_fallback() {
        use super::super::representation::diagnostics::Blocker;
        use bcode_workflow_view_models::{WorkflowSchedulingActivation, WorkflowSchedulingState};
        let activation = WorkflowSchedulingActivation {
            node_id: "worker".into(),
            activation_id: "a".into(),
            state: WorkflowSchedulingState::Blocked,
            reasons: vec![
                Blocker::Resource {
                    resource: "exact-workspace".into(),
                },
                Blocker::Recovery,
                Blocker::Cancellation,
                Blocker::UserPaused,
                Blocker::ControlFailure,
                Blocker::Capacity {
                    occupied: 7,
                    limit: 7,
                },
            ]
            .into_iter()
            .map(project_blocker)
            .collect(),
        };
        let json = serde_json::to_value(&activation).unwrap();
        assert_eq!(json["reasons"][0]["resource"], "exact-workspace");
        assert_eq!(json["reasons"][1]["reason"], "recovery");
        assert_eq!(
            serde_json::from_value::<WorkflowSchedulingActivation>(json).unwrap(),
            activation
        );
        let text = activation.detail_lines().join("\n");
        for expected in [
            "exact-workspace",
            "Recovery",
            "Cancellation",
            "Paused",
            "ControlFailure",
            "7/7",
        ] {
            assert!(text.contains(expected), "missing {expected}");
        }
        assert!(!text.contains("Ready"));
    }

    #[test]
    fn cursor_is_lossless_and_pinned_to_canonical_family() {
        let key = "activation-界".repeat(40);
        let request = WorkflowSchedulingPageRequest {
            version: 1,
            run_id: "run".into(),
            limit: 31,
            cursor: Some(encode("run", 7, 2, key.clone()).unwrap()),
        };
        let cursor = decode(&request, 2).unwrap().unwrap();
        assert_eq!(cursor.after, key);
        assert_eq!(cursor.revision, 7);
        assert!(decode(&request, 1).is_err());
        let mut wrong = request;
        wrong.run_id = "other".into();
        assert!(decode(&wrong, 2).is_err());
        wrong.limit = 0;
        assert!(decode(&wrong, 2).is_err());
    }
}
