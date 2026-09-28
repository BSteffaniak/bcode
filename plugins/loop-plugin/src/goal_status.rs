//! Goal-owned text presentation of the application's bounded semantic workflow view.

use std::fmt::Write as _;

use bcode_workflow_view_models::{
    WorkflowNodeKind, WorkflowOutputValue, WorkflowProjectionHealth, WorkflowRunView,
    WorkflowTerminalView,
};

const DETAIL_LIMIT: usize = 10;

fn preview(text: &str) -> String {
    // A content budget, not terminal geometry. Keep status a bounded, single-line preview.
    text.chars()
        .take(320)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .chain((text.chars().nth(320).is_some()).then_some('…'))
        .collect()
}

pub fn format(view: &WorkflowRunView) -> String {
    if view.validate_version().is_err() {
        return "\nGoal execution detail unavailable: unsupported workflow view version".into();
    }
    let mut text = String::from("\nGoal execution (bounded snapshot; not a complete history):");
    if view.health != WorkflowProjectionHealth::Current {
        let _ = write!(
            text,
            "\nObservation degraded: {}",
            preview(&format!("{:?}", view.health))
        );
    }
    for node in view
        .nodes
        .iter()
        .filter(|node| node.kind == WorkflowNodeKind::Agent)
        .take(DETAIL_LIMIT)
    {
        let _ = write!(
            text,
            "\n  {} [{}] · {}",
            preview(&node.name),
            preview(&node.node_id),
            preview(&format!("{:?}", node.status))
        );
    }
    for permission in view.tool_permissions.iter().take(DETAIL_LIMIT) {
        let _ = write!(
            text,
            "\nApproval needed: {} · {} · execution session {}. Use /goal.worker <session-id> to inspect the exact request before deciding; opening does not approve it.",
            preview(&permission.node_id),
            preview(&permission.tool_name),
            preview(&permission.session_id)
        );
    }
    for approval in view.mutation_approvals.iter().take(DETAIL_LIMIT) {
        let _ = write!(
            text,
            "\nMutation approval needed: {} · {} · approval {} · workspace {}. Use /workflow to inspect the exact request and approve or deny; status does not authorize execution.",
            preview(&approval.node_id),
            preview(&approval.operation),
            preview(&approval.approval_id),
            preview(&approval.workspace_snapshot)
        );
        if let Some(warning) = &approval.reconciliation_warning {
            let _ = write!(text, "\n  Reconciliation warning: {}", preview(warning));
        }
    }
    for wait in view.waits.iter().take(DETAIL_LIMIT) {
        let _ = write!(
            text,
            "\nWaiting for {:?}: {} · {}",
            wait.kind,
            preview(&wait.node_id),
            preview(&wait.prompt)
        );
    }
    for failure in view.failure_diagnostics.iter().rev().take(DETAIL_LIMIT) {
        let _ = write!(
            text,
            "\nFailure: {} · {}",
            preview(failure.node_id.as_deref().unwrap_or("goal")),
            preview(&failure.message)
        );
    }
    for session in view.child_sessions.iter().take(DETAIL_LIMIT) {
        let _ = write!(
            text,
            "\nExecution session: {} · attempt {} · {} · /goal.worker <session-id> opens its canonical transcript",
            preview(&session.node_id),
            session.attempt,
            preview(&session.session_id)
        );
    }
    match &view.terminal {
        Some(WorkflowTerminalView::Completed { output_id }) => {
            let _ = write!(text, "\nWorkflow finished · canonical result {}. Worker success alone does not establish integrated goal completion.", preview(output_id));
            format_result(&mut text, view, output_id);
        }
        Some(WorkflowTerminalView::Failed) => text.push_str("\nWorkflow failed; successful sibling contributions are not an integrated result. Inspect failure details before corrective work."),
        Some(WorkflowTerminalView::Cancelled) => text.push_str("\nWorkflow cancelled; already committed effects are not undone."),
        Some(WorkflowTerminalView::RepairRequired) => text.push_str("\nRepair required: do not retry ambiguous effects. Inspect /workflow before recovery."),
        None => text.push_str("\nNo terminal result in this snapshot. Staged work and completed workers are not goal completion."),
    }
    text.push_str("\n/goal.status refreshes this snapshot; /workflow retains detailed history, results and approval controls.");
    text
}

fn format_result(text: &mut String, view: &WorkflowRunView, output_id: &str) {
    let Some(value) = view.outputs.iter().find_map(|output| {
        if output.output_id != output_id {
            return None;
        }
        match &output.value {
            WorkflowOutputValue::Resolved { value } => Some(value),
            WorkflowOutputValue::Unresolved => None,
        }
    }) else {
        text.push_str("\nGoal result detail unavailable in this bounded snapshot; inspect the canonical result in /workflow.");
        return;
    };
    // Interpret only the loop-owned state contract, never arbitrary worker summaries.
    let Ok(result) = serde_json::from_value::<super::LoopWorkflowIteration>(value.clone()) else {
        text.push_str(
            "\nGoal result detail unavailable: unsupported result shape; inspect /workflow.",
        );
        return;
    };
    let verdict = if result.condition_met {
        "criteria reported satisfied"
    } else {
        "criteria not satisfied"
    };
    let _ = write!(
        text,
        "\nGoal evaluation: {verdict} · {}",
        preview(&result.summary)
    );
    for evidence in result.evidence.iter().take(DETAIL_LIMIT) {
        let _ = write!(text, "\n  Evidence: {}", preview(evidence));
    }
    if result.evidence.len() > DETAIL_LIMIT {
        text.push_str(
            "\n  Additional evidence omitted; inspect the canonical result in /workflow.",
        );
    }
    if let Some(delivery) = &result.delivery {
        text.push_str("\nDelivery report (evaluator-reported):");
        for target in delivery.integrated_targets.iter().take(DETAIL_LIMIT) {
            let _ = write!(text, "\n  Integrated target: {}", preview(target));
        }
        let _ = write!(
            text,
            "\n  {} contribution references · {} criteria · {} checks · {} unresolved items. Inspect /workflow for full delivery evidence.",
            delivery.contribution_output_ids.len(),
            delivery.criteria.len(),
            delivery.checks.len(),
            delivery.unresolved_work.len()
        );
    }
    text.push_str("\nEvaluation evidence is reported, not independently verified by this display.");
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn final_result_uses_terminal_identity_and_preserves_incomplete_verdict() {
        let mut snapshot = view();
        snapshot.terminal = Some(WorkflowTerminalView::Completed {
            output_id: "final".into(),
        });
        let output = |id: &str, met: bool| {
            serde_json::from_value(serde_json::json!({
            "output_id":id,"node_id":"evaluation","activation_id":"a",
            "schema_id":"loop","schema_version":1,"checksum_sha256":"checksum",
            "artifact_reference":null,"created_at_ms":0,
            "value":{"availability":"resolved","value":{
                "implementation_prompt":"Implement objective","stop_condition":"Combined checks pass",
                "max_iterations":1,"iteration":1,"condition_met":met,
                "summary":"Combined check failed\nRetain both contributions",
                "evidence":["integrated.sh: expected 27, observed 28"],
                "delivery": {
                    "version":"1", "integrated_targets":["integrated.sh"],
                    "contribution_output_ids":["left", "right"],
                    "criteria":[{"criterion":"total is 27", "status":"failed", "evidence":"observed 28"}],
                    "checks":[], "retained_workspaces":[], "unresolved_work":["fix surcharge"]
                }
            }}
        })).unwrap()
        };
        snapshot.outputs = vec![output("worker", true), output("final", false)];
        let text = format(&snapshot);
        assert!(text.contains("criteria not satisfied"));
        assert!(!text.contains("criteria reported satisfied"));
        assert!(text.contains("Combined check failed Retain both contributions"));
        assert!(text.contains("integrated.sh: expected 27, observed 28"));
        assert!(text.contains("Integrated target: integrated.sh"));
        assert!(
            text.contains("2 contribution references · 1 criteria · 0 checks · 1 unresolved items")
        );
        snapshot.outputs[1].value = WorkflowOutputValue::Unresolved;
        let text = format(&snapshot);
        assert!(text.contains("Goal result detail unavailable"));
        assert!(!text.contains("Evidence:"));
    }

    #[test]
    fn final_result_evidence_is_bounded_and_unknown_shapes_are_not_guessed() {
        let mut snapshot = view();
        snapshot.terminal = Some(WorkflowTerminalView::Completed {
            output_id: "final".into(),
        });
        snapshot.outputs =
            vec![serde_json::from_value(serde_json::json!({
            "output_id":"final","node_id":"evaluation","activation_id":"a",
            "schema_id":"loop","schema_version":1,"checksum_sha256":"checksum",
            "artifact_reference":null,"created_at_ms":0,
            "value":{"availability":"resolved","value":{
                "implementation_prompt":"objective","stop_condition":"criteria",
                "max_iterations":1,"iteration":1,"condition_met":true,
                "summary":"verified integrated artifact", "evidence":vec!["x".repeat(400); 12]
            }}
        })).unwrap()];
        let text = format(&snapshot);
        assert_eq!(text.matches("  Evidence:").count(), DETAIL_LIMIT);
        assert!(text.contains("Additional evidence omitted"));
        assert!(!text.contains(&"x".repeat(321)));
        snapshot.outputs[0].value = WorkflowOutputValue::Resolved {
            value: serde_json::json!({"summary":"success"}),
        };
        assert!(format(&snapshot).contains("unsupported result shape"));
    }

    pub fn view() -> WorkflowRunView {
        serde_json::from_value(serde_json::json!({
            "version": bcode_workflow_view_models::WORKFLOW_VIEW_VERSION,
            "run": {"run_id":"run", "display_title":"Goal", "binding_label":null,
                "definition_id":"goal", "definition_version":1, "authored_source":null,
                "definition_disposition": bcode_workflow_view_models::WorkflowDefinitionDisposition::CompiledOnly,
                "progress": bcode_workflow_view_models::WorkflowRunProgress::default(),
                "attention": bcode_workflow_view_models::WorkflowAttentionSummary::default(),
                "parent_run_id":null, "descendant_count":0, "status":"running",
                "created_at_ms":0, "updated_at_ms":0},
            "nodes": [{"node_id":"worker", "name":"Implement change", "kind":"agent",
                "activation_id":"active", "status":"completed"}],
            "activations":[], "edges":[], "waits":[], "mutation_approvals":[],
            "attempts":[], "retry_schedules":[], "outputs":[], "failure_diagnostics":[],
            "descendant_runs":[], "tool_permissions":[], "child_sessions":[], "actions":[],
            "terminal":null, "health":{"state":"current"}
        }))
        .unwrap()
    }

    #[test]
    fn completed_worker_is_not_goal_completion() {
        let text = format(&view());
        assert!(text.contains("Implement change [worker] · Completed"));
        assert!(text.contains("No terminal result"));
        assert!(text.contains("bounded snapshot"));
    }

    #[test]
    fn approvals_and_failures_remain_actionable() {
        let mut view = view();
        view.tool_permissions
            .push(bcode_workflow_view_models::WorkflowToolPermissionView {
                node_id: "worker".into(),
                activation_id: "active".into(),
                attempt: 1,
                session_id: "session".into(),
                permission_id: "permission".into(),
                tool_name: "shell.run".into(),
            });
        view.failure_diagnostics
            .push(bcode_workflow_view_models::WorkflowFailureDiagnostic {
                kind: "attempt_failed".into(),
                message: "Combined checks failed".into(),
                node_id: Some("integrator".into()),
                activation_id: None,
                dispatch_identity: None,
                event_sequence: 1,
                occurred_at_ms: 1,
            });
        view.terminal = Some(WorkflowTerminalView::Failed);
        let text = format(&view);
        assert!(text.contains("Approval needed: worker · shell.run · execution session session"));
        assert!(text.contains("Failure: integrator · Combined checks failed"));
        assert!(text.contains("successful sibling contributions are not an integrated result"));
    }

    #[test]
    fn mutation_approvals_expose_bounded_request_identity_and_warnings() {
        let mut view = view();
        let approval = bcode_workflow_view_models::WorkflowMutationApprovalView {
            approval_id: "approval-1".into(),
            node_id: "integrator".into(),
            activation_id: "active".into(),
            plugin_id: "coding".into(),
            block_id: "integrate".into(),
            block_version: 1,
            operation: "integrate contributions".into(),
            effect: bcode_workflow_view_models::WorkflowOperationEffect::Mutating,
            input_summary: serde_json::json!({}),
            resource_claims: Vec::new(),
            workspace_snapshot: "dirty workspace\nretained".into(),
            reconciliation_warning: Some("Conflict requires explicit resolution".into()),
            requested_at_ms: 1,
            expires_at_ms: None,
        };
        view.mutation_approvals = vec![approval; DETAIL_LIMIT + 1];
        let text = format(&view);
        assert_eq!(
            text.matches("Mutation approval needed:").count(),
            DETAIL_LIMIT
        );
        assert!(text.contains("integrator · integrate contributions · approval approval-1"));
        assert!(text.contains("workspace dirty workspace retained"));
        assert!(text.contains("Reconciliation warning: Conflict requires explicit resolution"));
        assert!(text.contains("Use /workflow to inspect the exact request and approve or deny"));
        assert!(text.contains("status does not authorize execution"));
        assert_eq!(view.mutation_approvals.len(), DETAIL_LIMIT + 1);
    }

    #[test]
    fn unknown_version_is_not_interpreted() {
        let mut view = view();
        view.version += 1;
        assert!(format(&view).contains("unsupported workflow view version"));
        assert!(!format(&view).contains("Implement change"));
    }

    #[test]
    fn previews_are_bounded_and_single_line_without_splitting_unicode() {
        assert_eq!(preview("hello\nworld\u{1b}"), "hello world ");
        assert_eq!(preview(&"界".repeat(400)), format!("{}…", "界".repeat(320)));
    }
}
