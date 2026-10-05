//! Exact canonical gate waits adapted into existing portable interaction contracts.
use bcode_workflow_view_models::{
    WorkflowActionAffordance, WorkflowActionKind, WorkflowActionTarget, WorkflowNodeStatus,
    WorkflowRunView, WorkflowWaitKind, WorkflowWaitView,
};

/// Load one canonical gate scan page and expose exact application-revalidated actions.
/// # Errors
/// Rejects invalid limits, damaged gate state and unsupported canonical representations.
pub async fn populate(
    state: &super::ServerState,
    run_id: &str,
    limit: usize,
    view: &mut WorkflowRunView,
) -> Result<(), super::ServerError> {
    let retained = super::retained_store(state)?;
    let db = super::open(retained.path(), false)?;
    let limit = u16::try_from(limit)
        .ok()
        .filter(|limit| (1..=100).contains(limit))
        .ok_or_else(super::unavailable)?;
    let page = super::representation::graph::gate_waits::page(&db, run_id, limit)
        .await
        .map_err(|_| super::unavailable())?;
    let waits = page
        .gates
        .into_iter()
        .map(|gate| WorkflowWaitView {
            node_id: gate.node_id,
            activation_id: gate.activation_id,
            kind: match gate.kind {
                super::representation::graph::gate_waits::GateKind::Input => {
                    WorkflowWaitKind::Input
                }
                super::representation::graph::gate_waits::GateKind::Approval => {
                    WorkflowWaitKind::Approval
                }
            },
            prompt: gate
                .prompt
                .unwrap_or_else(|| "Workflow decision requested".into()),
            expected_schema: gate.response_schema.map(|schema| schema.schema),
            input: Some(gate.input),
            requested_at_ms: gate.requested_at_ms,
        })
        .collect();
    attach(view, waits);
    if let bcode_workflow_view_models::WorkflowProjectionHealth::Degraded { reason } =
        &mut view.health
    {
        reason.push_str(" Canonical gate actions shown are revalidated at invocation; gate input and request time are canonical.");
        if page.next_node_id.is_some() {
            reason.push_str(
                " Additional gate scan rows omitted; this view does not expose their continuation.",
            );
        }
    }
    Ok(())
}

pub(super) fn attach(view: &mut WorkflowRunView, waits: Vec<WorkflowWaitView>) {
    view.waits.clear();
    view.actions.retain(|action| {
        !matches!(
            action.kind,
            WorkflowActionKind::ProvideInput
                | WorkflowActionKind::Approve
                | WorkflowActionKind::Deny
        )
    });
    if view.terminal.is_some() {
        return;
    }
    for wait in waits.into_iter().take(100) {
        for node in &mut view.nodes {
            if node.node_id == wait.node_id
                && node.activation_id.as_ref() == Some(&wait.activation_id)
            {
                node.status = match wait.kind {
                    WorkflowWaitKind::Input => WorkflowNodeStatus::WaitingInput,
                    WorkflowWaitKind::Approval => WorkflowNodeStatus::WaitingApproval,
                };
            }
        }
        let kinds: &[WorkflowActionKind] = match wait.kind {
            WorkflowWaitKind::Input => &[WorkflowActionKind::ProvideInput],
            WorkflowWaitKind::Approval => &[WorkflowActionKind::Approve, WorkflowActionKind::Deny],
        };
        for kind in kinds {
            view.actions.push(WorkflowActionAffordance {
                kind: *kind,
                target: WorkflowActionTarget::Activation {
                    run_id: view.run.run_id.clone(),
                    node_id: wait.node_id.clone(),
                    activation_id: wait.activation_id.clone(),
                },
                enabled: true,
                unavailable_reason: None,
            });
        }
        view.waits.push(wait);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_gate_targets_and_schema_survive_degraded_view() {
        let mut view: WorkflowRunView = serde_json::from_value(serde_json::json!({
            "version": bcode_workflow_view_models::WORKFLOW_VIEW_VERSION,
            "run": {"run_id":"run","display_title":"run","binding_label":null,"definition_id":"d","definition_version":1,"authored_source":null,"definition_disposition":{"disposition":"compiled_only"},"parent_run_id":null,"descendant_count":0,"progress":bcode_workflow_view_models::WorkflowRunProgress::default(),"attention":bcode_workflow_view_models::WorkflowAttentionSummary::default(),"status":"paused","created_at_ms":1,"updated_at_ms":1},
            "nodes":[],"activations":[],"edges":[],"waits":[],"mutation_approvals":[],"attempts":[],"retry_schedules":[],"outputs":[],"failure_diagnostics":[],"scheduling":null,"descendant_runs":[],"tool_permissions":[],"child_sessions":[],"actions":[],"terminal":null,"health":{"state":"degraded","reason":"partial"}
        })).unwrap();
        let wait = WorkflowWaitView {
            node_id: "input".into(),
            activation_id: "exact".into(),
            kind: WorkflowWaitKind::Input,
            prompt: "Supply value".into(),
            expected_schema: Some(serde_json::json!({"type":"integer"})),
            input: Some(serde_json::json!(2)),
            requested_at_ms: 42,
        };
        attach(&mut view, vec![wait.clone()]);
        assert_eq!(view.waits, vec![wait]);
        assert_eq!(view.actions[0].kind, WorkflowActionKind::ProvideInput);
        assert_eq!(
            view.actions[0].target,
            WorkflowActionTarget::Activation {
                run_id: "run".into(),
                node_id: "input".into(),
                activation_id: "exact".into()
            }
        );
        let approval = WorkflowWaitView {
            node_id: "approval".into(),
            activation_id: "a".into(),
            kind: WorkflowWaitKind::Approval,
            prompt: "Review".into(),
            expected_schema: None,
            input: None,
            requested_at_ms: 43,
        };
        attach(&mut view, vec![approval.clone()]);
        assert_eq!(view.actions[0].kind, WorkflowActionKind::Approve);
        assert_eq!(view.actions[1].kind, WorkflowActionKind::Deny);
        assert_eq!(view.waits.len(), 1);
        attach(&mut view, vec![approval.clone()]);
        assert_eq!(view.actions.len(), 2);
        attach(&mut view, vec![]);
        assert!(view.actions.is_empty());
        assert!(view.waits.is_empty());
        view.terminal = Some(bcode_workflow_view_models::WorkflowTerminalView::Cancelled);
        attach(&mut view, vec![approval]);
        assert!(view.actions.is_empty());
    }
}
