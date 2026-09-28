//! Goal-owned text presentation of the application's bounded semantic workflow view.

use std::fmt::Write as _;

use bcode_workflow_view_models::{
    WorkflowNodeKind, WorkflowProjectionHealth, WorkflowRunView, WorkflowTerminalView,
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
        }
        Some(WorkflowTerminalView::Failed) => text.push_str("\nWorkflow failed; successful sibling contributions are not an integrated result. Inspect failure details before corrective work."),
        Some(WorkflowTerminalView::Cancelled) => text.push_str("\nWorkflow cancelled; already committed effects are not undone."),
        Some(WorkflowTerminalView::RepairRequired) => text.push_str("\nRepair required: do not retry ambiguous effects. Inspect /workflow before recovery."),
        None => text.push_str("\nNo terminal result in this snapshot. Staged work and completed workers are not goal completion."),
    }
    text.push_str("\n/goal.status refreshes this snapshot; /workflow retains detailed history, results and approval controls.");
    text
}

#[cfg(test)]
pub mod tests {
    use super::*;

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
