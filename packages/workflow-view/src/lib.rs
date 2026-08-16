#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Normalization of portable workflow application data into semantic projections.

use std::collections::BTreeMap;

use bcode_workflow_view_models::{
    WORKFLOW_VIEW_VERSION, WorkflowActionAffordance, WorkflowActionKind, WorkflowActionTarget,
    WorkflowAttemptView, WorkflowCatalogView, WorkflowChildSessionView, WorkflowDescendantRunView,
    WorkflowEdgeView, WorkflowMutationApprovalView, WorkflowNodeKind, WorkflowNodeStatus,
    WorkflowNodeView, WorkflowOutputValue, WorkflowOutputView, WorkflowProjectionHealth,
    WorkflowRunListItem, WorkflowRunStatus, WorkflowRunView, WorkflowTerminalView,
    WorkflowWaitKind, WorkflowWaitView,
};

/// Portable definition source needed to project a workflow graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowDefinitionProjectionInput {
    pub definition_json: String,
}

/// Portable activation state needed to project one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowActivationProjectionInput {
    pub node_id: String,
    pub activation_id: String,
    pub status: String,
}

/// Canonical validated output supplied by the application boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowOutputProjectionInput {
    pub output_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub schema_id: String,
    pub schema_version: u32,
    pub checksum_sha256: String,
    pub value: Option<serde_json::Value>,
    pub artifact_reference: Option<String>,
    pub created_at_ms: u64,
}

/// Complete bounded portable input needed to project one workflow run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunProjectionInput {
    pub run: WorkflowRunListItem,
    pub terminal_output_id: Option<String>,
    pub definition: WorkflowDefinitionProjectionInput,
    pub activations: Vec<WorkflowActivationProjectionInput>,
    pub waits: Vec<WorkflowWaitView>,
    pub mutation_approvals: Vec<WorkflowMutationApprovalView>,
    pub attempts: Vec<WorkflowAttemptView>,
    pub outputs: Vec<WorkflowOutputProjectionInput>,
    pub descendant_runs: Vec<WorkflowDescendantRunView>,
    pub child_sessions: Vec<WorkflowChildSessionView>,
}

/// Build a bounded portable workflow catalog page.
#[must_use]
pub fn project_catalog(
    runs: Vec<WorkflowRunListItem>,
    request: &bcode_workflow_view_models::WorkflowCatalogRequest,
    has_more: bool,
) -> WorkflowCatalogView {
    let next_cursor = has_more.then(|| runs.last()).flatten().map(|run| {
        bcode_workflow_view_models::WorkflowCatalogCursor {
            sort: request.sort,
            timestamp_ms: match request.sort {
                bcode_workflow_view_models::WorkflowCatalogSort::UpdatedAt
                | bcode_workflow_view_models::WorkflowCatalogSort::Status => run.updated_at_ms,
                bcode_workflow_view_models::WorkflowCatalogSort::CreatedAt => run.created_at_ms,
            },
            status_rank: workflow_status_rank(run.status),
            run_id: run.run_id.clone(),
        }
    });
    WorkflowCatalogView {
        version: WORKFLOW_VIEW_VERSION,
        runs,
        next_cursor,
        has_more,
        filter: request.filter,
        sort: request.sort,
        group: request.group,
        search: request.search.clone(),
    }
}

const fn workflow_status_rank(status: WorkflowRunStatus) -> u8 {
    match status {
        WorkflowRunStatus::Running => 0,
        WorkflowRunStatus::Paused => 1,
        WorkflowRunStatus::RepairRequired => 2,
        WorkflowRunStatus::Failed => 3,
        WorkflowRunStatus::Completed => 4,
        WorkflowRunStatus::Cancelled => 5,
    }
}

/// Build a portable semantic projection from bounded application data.
///
/// Malformed definitions produce a degraded projection rather than silently guessing graph
/// semantics. Canonical run identity and bounded runtime state remain visible.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn project_run(input: WorkflowRunProjectionInput) -> WorkflowRunView {
    let latest_activations = input.activations.iter().fold(
        BTreeMap::<&str, &WorkflowActivationProjectionInput>::new(),
        |mut latest, activation| {
            latest.entry(&activation.node_id).or_insert(activation);
            latest
        },
    );

    let (nodes, edges, health) = match serde_json::from_str::<bcode_workflow::WorkflowDefinition>(
        &input.definition.definition_json,
    ) {
        Ok(definition) => {
            let nodes = definition
                .nodes
                .values()
                .map(|node| {
                    let activation = latest_activations.get(node.id.as_str()).copied();
                    WorkflowNodeView {
                        node_id: node.id.clone(),
                        name: node.name.clone(),
                        kind: project_node_kind(node.kind),
                        activation_id: activation.map(|value| value.activation_id.clone()),
                        status: activation.map_or(WorkflowNodeStatus::NotStarted, |value| {
                            project_node_status(&value.status)
                        }),
                    }
                })
                .collect();
            let edges = definition
                .edges
                .iter()
                .map(|edge| WorkflowEdgeView {
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                    kind: format!("{:?}", edge.kind).to_ascii_lowercase(),
                })
                .collect();
            (nodes, edges, WorkflowProjectionHealth::Current)
        }
        Err(error) => (
            Vec::new(),
            Vec::new(),
            WorkflowProjectionHealth::Degraded {
                reason: format!("stored workflow definition is malformed: {error}"),
            },
        ),
    };

    let terminal = project_terminal(input.run.status, input.terminal_output_id);
    let actions = project_actions(
        &input.run,
        &input.waits,
        &input.mutation_approvals,
        &input.attempts,
        &input.child_sessions,
    );

    WorkflowRunView {
        version: WORKFLOW_VIEW_VERSION,
        run: input.run,
        nodes,
        edges,
        waits: input.waits,
        mutation_approvals: input.mutation_approvals,
        attempts: input.attempts,
        outputs: input
            .outputs
            .into_iter()
            .map(|output| WorkflowOutputView {
                output_id: output.output_id,
                node_id: output.node_id,
                activation_id: output.activation_id,
                schema_id: output.schema_id,
                schema_version: output.schema_version,
                checksum_sha256: output.checksum_sha256,
                value: output
                    .value
                    .map_or(WorkflowOutputValue::Unresolved, |value| {
                        WorkflowOutputValue::Resolved { value }
                    }),
                artifact_reference: output.artifact_reference,
                created_at_ms: output.created_at_ms,
            })
            .collect(),
        descendant_runs: input.descendant_runs,
        child_sessions: input.child_sessions,
        actions,
        terminal,
        health,
    }
}

#[allow(clippy::too_many_lines)]
fn project_actions(
    run: &WorkflowRunListItem,
    waits: &[WorkflowWaitView],
    mutation_approvals: &[WorkflowMutationApprovalView],
    attempts: &[WorkflowAttemptView],
    child_sessions: &[WorkflowChildSessionView],
) -> Vec<WorkflowActionAffordance> {
    let mut actions = Vec::new();
    let run_target = WorkflowActionTarget::Run {
        run_id: run.run_id.clone(),
    };
    match run.status {
        WorkflowRunStatus::Running => {
            actions.push(enabled_action(
                WorkflowActionKind::Pause,
                run_target.clone(),
            ));
            actions.push(enabled_action(WorkflowActionKind::Cancel, run_target));
        }
        WorkflowRunStatus::Paused => {
            actions.push(enabled_action(
                WorkflowActionKind::Resume,
                run_target.clone(),
            ));
            actions.push(enabled_action(WorkflowActionKind::Cancel, run_target));
        }
        WorkflowRunStatus::Completed
        | WorkflowRunStatus::Failed
        | WorkflowRunStatus::Cancelled
        | WorkflowRunStatus::RepairRequired => {}
    }
    for wait in waits {
        let target = WorkflowActionTarget::Activation {
            run_id: run.run_id.clone(),
            node_id: wait.node_id.clone(),
            activation_id: wait.activation_id.clone(),
        };
        match wait.kind {
            WorkflowWaitKind::Input => {
                actions.push(enabled_action(WorkflowActionKind::ProvideInput, target));
            }
            WorkflowWaitKind::Approval => {
                actions.push(enabled_action(WorkflowActionKind::Approve, target.clone()));
                actions.push(enabled_action(WorkflowActionKind::Deny, target));
            }
        }
    }
    for approval in mutation_approvals {
        let target = WorkflowActionTarget::MutationApproval {
            approval_id: approval.approval_id.clone(),
        };
        actions.push(enabled_action(
            WorkflowActionKind::ApproveMutation,
            target.clone(),
        ));
        actions.push(enabled_action(WorkflowActionKind::DenyMutation, target));
    }
    let latest_failed_attempts = attempts.iter().fold(
        BTreeMap::<(&str, &str), &WorkflowAttemptView>::new(),
        |mut latest, attempt| {
            if attempt.status == "failed" {
                latest
                    .entry((&attempt.node_id, &attempt.activation_id))
                    .and_modify(|current| {
                        if attempt.attempt > current.attempt {
                            *current = attempt;
                        }
                    })
                    .or_insert(attempt);
            }
            latest
        },
    );
    for attempt in latest_failed_attempts.into_values() {
        actions.push(enabled_action(
            WorkflowActionKind::RetryNode,
            WorkflowActionTarget::Attempt {
                run_id: run.run_id.clone(),
                node_id: attempt.node_id.clone(),
                activation_id: attempt.activation_id.clone(),
                attempt: attempt.attempt,
            },
        ));
    }
    for session in child_sessions {
        actions.push(enabled_action(
            WorkflowActionKind::OpenSession,
            WorkflowActionTarget::Session {
                session_id: session.session_id.clone(),
            },
        ));
    }
    match &run.definition_disposition {
        bcode_workflow_view_models::WorkflowDefinitionDisposition::Published {
            workflow_id,
            revision,
            editable_draft_id,
        } => {
            actions.push(enabled_action(
                WorkflowActionKind::ViewDefinition,
                WorkflowActionTarget::PublishedDefinition {
                    workflow_id: workflow_id.clone(),
                    revision: *revision,
                },
            ));
            if let Some(draft_id) = editable_draft_id {
                actions.push(enabled_action(
                    WorkflowActionKind::EditDraft,
                    WorkflowActionTarget::Draft {
                        workflow_id: workflow_id.clone(),
                        draft_id: draft_id.clone(),
                    },
                ));
            } else {
                actions.push(enabled_action(
                    WorkflowActionKind::ForkDefinition,
                    WorkflowActionTarget::PublishedDefinition {
                        workflow_id: workflow_id.clone(),
                        revision: *revision,
                    },
                ));
            }
        }
        bcode_workflow_view_models::WorkflowDefinitionDisposition::CompiledOnly => {}
    }
    actions
}

const fn enabled_action(
    kind: WorkflowActionKind,
    target: WorkflowActionTarget,
) -> WorkflowActionAffordance {
    WorkflowActionAffordance {
        kind,
        target,
        enabled: true,
        unavailable_reason: None,
    }
}

const fn project_node_kind(kind: bcode_workflow::NodeKind) -> WorkflowNodeKind {
    match kind {
        bcode_workflow::NodeKind::Task => WorkflowNodeKind::Task,
        bcode_workflow::NodeKind::Agent => WorkflowNodeKind::Agent,
        bcode_workflow::NodeKind::Branch => WorkflowNodeKind::Branch,
        bcode_workflow::NodeKind::Repeat => WorkflowNodeKind::Repeat,
        bcode_workflow::NodeKind::Retry => WorkflowNodeKind::Retry,
        bcode_workflow::NodeKind::Parallel => WorkflowNodeKind::Parallel,
        bcode_workflow::NodeKind::FanOut => WorkflowNodeKind::FanOut,
        bcode_workflow::NodeKind::PluginBlock => WorkflowNodeKind::PluginBlock,
        bcode_workflow::NodeKind::Input => WorkflowNodeKind::Input,
        bcode_workflow::NodeKind::Approval => WorkflowNodeKind::Approval,
        bcode_workflow::NodeKind::WorkflowCall => WorkflowNodeKind::WorkflowCall,
    }
}

fn project_node_status(status: &str) -> WorkflowNodeStatus {
    match status {
        "pending" => WorkflowNodeStatus::Pending,
        "running" => WorkflowNodeStatus::Running,
        "waiting_input" => WorkflowNodeStatus::WaitingInput,
        "waiting_approval" => WorkflowNodeStatus::WaitingApproval,
        "waiting_mutation_approval" => WorkflowNodeStatus::WaitingMutationApproval,
        "completed" => WorkflowNodeStatus::Completed,
        "failed" => WorkflowNodeStatus::Failed,
        "cancelled" => WorkflowNodeStatus::Cancelled,
        "skipped" => WorkflowNodeStatus::Skipped,
        "repair_required" => WorkflowNodeStatus::RepairRequired,
        value => WorkflowNodeStatus::Unknown(value.to_string()),
    }
}

fn project_terminal(
    status: WorkflowRunStatus,
    terminal_output_id: Option<String>,
) -> Option<WorkflowTerminalView> {
    match status {
        WorkflowRunStatus::Running | WorkflowRunStatus::Paused => None,
        WorkflowRunStatus::Completed => {
            terminal_output_id.map(|output_id| WorkflowTerminalView::Completed { output_id })
        }
        WorkflowRunStatus::Failed => Some(WorkflowTerminalView::Failed),
        WorkflowRunStatus::Cancelled => Some(WorkflowTerminalView::Cancelled),
        WorkflowRunStatus::RepairRequired => Some(WorkflowTerminalView::RepairRequired),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_item(
        run_id: &str,
        status: WorkflowRunStatus,
        created: u64,
        updated: u64,
    ) -> WorkflowRunListItem {
        WorkflowRunListItem {
            run_id: run_id.to_string(),
            display_title: format!("Run {run_id}"),
            binding_label: None,
            definition_id: "definition-1".to_string(),
            definition_version: 1,
            authored_source: None,
            definition_disposition:
                bcode_workflow_view_models::WorkflowDefinitionDisposition::CompiledOnly,
            parent_run_id: None,
            descendant_count: 0,
            progress: bcode_workflow_view_models::WorkflowRunProgress::default(),
            attention: bcode_workflow_view_models::WorkflowAttentionSummary::default(),
            status,
            created_at_ms: created,
            updated_at_ms: updated,
        }
    }

    #[test]
    fn catalog_cursors_are_deterministic_for_every_sort_and_query_identity() {
        let runs = vec![
            catalog_item("run-a", WorkflowRunStatus::Running, 10, 30),
            catalog_item("run-b", WorkflowRunStatus::Failed, 20, 40),
        ];
        for (sort, expected_timestamp, expected_rank) in [
            (
                bcode_workflow_view_models::WorkflowCatalogSort::UpdatedAt,
                40,
                3,
            ),
            (
                bcode_workflow_view_models::WorkflowCatalogSort::CreatedAt,
                20,
                3,
            ),
            (
                bcode_workflow_view_models::WorkflowCatalogSort::Status,
                40,
                3,
            ),
        ] {
            let request = bcode_workflow_view_models::WorkflowCatalogRequest {
                limit: 2,
                cursor: None,
                filter: bcode_workflow_view_models::WorkflowCatalogFilter::NeedsAttention,
                sort,
                group: bcode_workflow_view_models::WorkflowCatalogGroup::Definition,
                search: Some("review".to_string()),
            };
            let first = project_catalog(runs.clone(), &request, true);
            let second = project_catalog(runs.clone(), &request, true);
            assert_eq!(first, second);
            assert_eq!(first.filter, request.filter);
            assert_eq!(first.sort, sort);
            assert_eq!(first.group, request.group);
            assert_eq!(first.search, request.search);
            assert_eq!(
                first.next_cursor,
                Some(bcode_workflow_view_models::WorkflowCatalogCursor {
                    sort,
                    timestamp_ms: expected_timestamp,
                    status_rank: expected_rank,
                    run_id: "run-b".to_string(),
                })
            );
        }
    }

    #[test]
    fn node_status_projection_is_exhaustive_for_current_states() {
        let cases = [
            ("pending", WorkflowNodeStatus::Pending),
            ("running", WorkflowNodeStatus::Running),
            ("waiting_input", WorkflowNodeStatus::WaitingInput),
            ("waiting_approval", WorkflowNodeStatus::WaitingApproval),
            (
                "waiting_mutation_approval",
                WorkflowNodeStatus::WaitingMutationApproval,
            ),
            ("completed", WorkflowNodeStatus::Completed),
            ("failed", WorkflowNodeStatus::Failed),
            ("cancelled", WorkflowNodeStatus::Cancelled),
            ("skipped", WorkflowNodeStatus::Skipped),
            ("repair_required", WorkflowNodeStatus::RepairRequired),
        ];
        for (source, expected) in cases {
            assert_eq!(project_node_status(source), expected);
        }
        assert_eq!(
            project_node_status("future_state"),
            WorkflowNodeStatus::Unknown("future_state".to_string())
        );
    }

    #[test]
    fn terminal_projection_is_stable_for_every_run_state() {
        assert_eq!(project_terminal(WorkflowRunStatus::Running, None), None);
        assert_eq!(project_terminal(WorkflowRunStatus::Paused, None), None);
        assert_eq!(
            project_terminal(WorkflowRunStatus::Completed, Some("output-1".to_string())),
            Some(WorkflowTerminalView::Completed {
                output_id: "output-1".to_string()
            })
        );
        assert_eq!(project_terminal(WorkflowRunStatus::Completed, None), None);
        assert_eq!(
            project_terminal(WorkflowRunStatus::Failed, None),
            Some(WorkflowTerminalView::Failed)
        );
        assert_eq!(
            project_terminal(WorkflowRunStatus::Cancelled, None),
            Some(WorkflowTerminalView::Cancelled)
        );
        assert_eq!(
            project_terminal(WorkflowRunStatus::RepairRequired, None),
            Some(WorkflowTerminalView::RepairRequired)
        );
    }

    #[test]
    fn malformed_definition_is_degraded_and_output_absence_is_explicit() {
        let view = project_run(WorkflowRunProjectionInput {
            run: WorkflowRunListItem {
                run_id: "run-1".to_string(),
                display_title: "Definition 1".to_string(),
                binding_label: None,
                definition_id: "definition-1".to_string(),
                definition_version: 1,
                authored_source: None,
                definition_disposition:
                    bcode_workflow_view_models::WorkflowDefinitionDisposition::CompiledOnly,
                parent_run_id: None,
                descendant_count: 0,
                progress: bcode_workflow_view_models::WorkflowRunProgress::default(),
                attention: bcode_workflow_view_models::WorkflowAttentionSummary::default(),
                status: WorkflowRunStatus::Running,
                created_at_ms: 1,
                updated_at_ms: 2,
            },
            terminal_output_id: None,
            definition: WorkflowDefinitionProjectionInput {
                definition_json: "not-json".to_string(),
            },
            activations: Vec::new(),
            waits: vec![WorkflowWaitView {
                node_id: "input".to_string(),
                activation_id: "activation-1".to_string(),
                kind: bcode_workflow_view_models::WorkflowWaitKind::Input,
                prompt: "Input".to_string(),
                expected_schema: Some(serde_json::json!({"type": "object"})),
                input: None,
                requested_at_ms: 3,
            }],
            mutation_approvals: Vec::new(),
            attempts: Vec::new(),
            outputs: vec![WorkflowOutputProjectionInput {
                output_id: "output-1".to_string(),
                node_id: "review".to_string(),
                activation_id: "activation-2".to_string(),
                schema_id: "review/v1".to_string(),
                schema_version: 1,
                checksum_sha256: "checksum".to_string(),
                value: None,
                artifact_reference: None,
                created_at_ms: 4,
            }],
            descendant_runs: Vec::new(),
            child_sessions: Vec::new(),
        });
        assert!(matches!(
            view.health,
            WorkflowProjectionHealth::Degraded { .. }
        ));
        assert_eq!(
            view.waits[0].kind,
            bcode_workflow_view_models::WorkflowWaitKind::Input
        );
        assert_eq!(view.outputs[0].value, WorkflowOutputValue::Unresolved);
    }
}
