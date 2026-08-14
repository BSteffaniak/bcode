#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Normalization from server-owned workflow data into portable semantic projections.

use std::collections::BTreeMap;

use bcode_workflow_store::{
    AttemptSummary, RunStatus, StoredWorkflowDefinition, WaitingActivation,
    WorkflowActivationSummary, WorkflowExecutionSessionLink, WorkflowMutationApproval,
    WorkflowOutputSummary, WorkflowRunSummary,
};
use bcode_workflow_view_models::{
    WORKFLOW_VIEW_VERSION, WorkflowAttemptView, WorkflowCatalogView, WorkflowChildSessionView,
    WorkflowDescendantRunView, WorkflowEdgeView, WorkflowMutationApprovalView, WorkflowNodeKind,
    WorkflowNodeStatus, WorkflowNodeView, WorkflowOutputValue, WorkflowOutputView,
    WorkflowProjectionHealth, WorkflowRunListItem, WorkflowRunStatus, WorkflowRunView,
    WorkflowTerminalView, WorkflowWaitKind, WorkflowWaitView,
};

/// Canonical validated value supplied by the application boundary for projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWorkflowOutput {
    pub output_id: String,
    pub checksum_sha256: String,
    pub value: serde_json::Value,
}

/// Complete bounded server-owned input needed to project one workflow run.
pub struct WorkflowRunProjectionInput<'a> {
    pub run: &'a WorkflowRunSummary,
    pub definition: &'a StoredWorkflowDefinition,
    pub activations: &'a [WorkflowActivationSummary],
    pub waits: &'a [WaitingActivation],
    pub mutation_approvals: &'a [WorkflowMutationApproval],
    pub attempts: &'a [AttemptSummary],
    pub outputs: &'a [WorkflowOutputSummary],
    pub resolved_outputs: &'a [ResolvedWorkflowOutput],
    pub descendant_runs: &'a [bcode_workflow_store::WorkflowDescendantRunSummary],
    pub child_sessions: &'a [WorkflowExecutionSessionLink],
}

/// Build a bounded portable workflow catalog.
#[must_use]
pub fn project_catalog(runs: &[WorkflowRunSummary]) -> WorkflowCatalogView {
    WorkflowCatalogView {
        version: WORKFLOW_VIEW_VERSION,
        runs: runs.iter().map(project_run_item).collect(),
    }
}

/// Build a portable semantic projection from bounded server-owned state.
///
/// Malformed stored definitions produce a degraded projection rather than silently guessing graph
/// semantics. Canonical run identity and bounded runtime state remain visible.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn project_run(input: &WorkflowRunProjectionInput<'_>) -> WorkflowRunView {
    let resolved = input
        .resolved_outputs
        .iter()
        .map(|output| (output.output_id.as_str(), output))
        .collect::<BTreeMap<_, _>>();
    let latest_activations = input.activations.iter().fold(
        BTreeMap::<&str, &WorkflowActivationSummary>::new(),
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

    WorkflowRunView {
        version: WORKFLOW_VIEW_VERSION,
        run: project_run_item(input.run),
        nodes,
        edges,
        waits: input.waits.iter().map(project_wait).collect(),
        mutation_approvals: input
            .mutation_approvals
            .iter()
            .map(|approval| WorkflowMutationApprovalView {
                approval_id: approval.approval_id.clone(),
                node_id: approval.node_id.clone(),
                activation_id: approval.activation_id.clone(),
                requested_at_ms: approval.requested_at_ms,
                expires_at_ms: approval.expires_at_ms,
            })
            .collect(),
        attempts: input.attempts.iter().map(project_attempt).collect(),
        outputs: input
            .outputs
            .iter()
            .map(|output| {
                let value = resolved.get(output.output_id.as_str()).map_or(
                    WorkflowOutputValue::Unresolved,
                    |resolved| WorkflowOutputValue::Resolved {
                        value: resolved.value.clone(),
                    },
                );
                WorkflowOutputView {
                    output_id: output.output_id.clone(),
                    node_id: output.node_id.clone(),
                    activation_id: output.activation_id.clone(),
                    schema_id: output.schema_id.clone(),
                    schema_version: output.schema_version,
                    checksum_sha256: output.checksum_sha256.clone(),
                    value,
                    artifact_reference: output.artifact_reference.clone(),
                    created_at_ms: output.created_at_ms,
                }
            })
            .collect(),
        descendant_runs: input
            .descendant_runs
            .iter()
            .map(|descendant| WorkflowDescendantRunView {
                run: project_run_item(&descendant.run),
                parent_run_id: descendant.link.parent_run_id.clone(),
                parent_node_id: descendant.link.parent_node_id.clone(),
                depth: descendant.link.depth,
            })
            .collect(),
        child_sessions: input
            .child_sessions
            .iter()
            .map(|link| WorkflowChildSessionView {
                node_id: link.node_id.clone(),
                activation_id: link.activation_id.clone(),
                attempt: link.attempt,
                session_id: link.session_id.clone(),
            })
            .collect(),
        terminal: project_terminal(input.run),
        health,
    }
}

fn project_run_item(run: &WorkflowRunSummary) -> WorkflowRunListItem {
    WorkflowRunListItem {
        run_id: run.run_id.clone(),
        definition_id: run.definition_id.clone(),
        definition_version: run.definition_version,
        status: match run.status {
            RunStatus::Running => WorkflowRunStatus::Running,
            RunStatus::Paused => WorkflowRunStatus::Paused,
            RunStatus::Completed => WorkflowRunStatus::Completed,
            RunStatus::Failed => WorkflowRunStatus::Failed,
            RunStatus::Cancelled => WorkflowRunStatus::Cancelled,
            RunStatus::RepairRequired => WorkflowRunStatus::RepairRequired,
        },
        created_at_ms: run.created_at_ms,
        updated_at_ms: run.updated_at_ms,
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

fn project_wait(wait: &WaitingActivation) -> WorkflowWaitView {
    WorkflowWaitView {
        node_id: wait.node_id.clone(),
        activation_id: wait.activation_id.clone(),
        kind: match wait.kind {
            bcode_workflow_store::WorkflowWaitKind::Input => WorkflowWaitKind::Input,
            bcode_workflow_store::WorkflowWaitKind::Approval => WorkflowWaitKind::Approval,
        },
        input: wait.input.clone(),
        requested_at_ms: wait.requested_at_ms,
    }
}

fn project_attempt(attempt: &AttemptSummary) -> WorkflowAttemptView {
    WorkflowAttemptView {
        node_id: attempt.node_id.clone(),
        activation_id: attempt.activation_id.clone(),
        attempt: attempt.attempt,
        dispatch_identity: attempt.dispatch_identity.clone(),
        status: attempt.status.clone(),
        has_receipt: attempt.has_receipt,
        prepared_at_ms: attempt.prepared_at_ms,
        terminal_at_ms: attempt.terminal_at_ms,
    }
}

fn project_terminal(run: &WorkflowRunSummary) -> Option<WorkflowTerminalView> {
    match run.status {
        RunStatus::Running | RunStatus::Paused => None,
        RunStatus::Completed => run
            .terminal_output_id
            .clone()
            .map(|output_id| WorkflowTerminalView::Completed { output_id }),
        RunStatus::Failed => Some(WorkflowTerminalView::Failed),
        RunStatus::Cancelled => Some(WorkflowTerminalView::Cancelled),
        RunStatus::RepairRequired => Some(WorkflowTerminalView::RepairRequired),
    }
}
