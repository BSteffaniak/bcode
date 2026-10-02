//! Goal-owned text presentation of the application's bounded semantic workflow view.

use std::fmt::Write as _;

use bcode_workflow_view_models::{
    WorkflowNodeKind, WorkflowOutputValue, WorkflowProjectionHealth, WorkflowRunView,
    WorkflowTerminalView,
};

/// Compact, read-only supervision from the same bounded projection as detailed inspection.
#[must_use]
pub fn overview(view: &WorkflowRunView) -> String {
    if view.version != bcode_workflow_view_models::WORKFLOW_VIEW_VERSION {
        return "Execution observation unavailable: unsupported view · /workflow".into();
    }
    let label = view.status_label();
    let mut text = format!("{} · {label}", preview(&view.run.display_title));
    if let Some(wait) = view.waits.first() {
        let _ = write!(text, " · {}", preview(&wait.prompt));
    }
    if view.health != WorkflowProjectionHealth::Current {
        text.push_str(" · observation degraded");
    }
    for node in view
        .nodes
        .iter()
        .filter(|node| {
            use bcode_workflow_view_models::WorkflowNodeStatus as S;
            matches!(
                node.status,
                S::Running
                    | S::WaitingInput
                    | S::WaitingApproval
                    | S::WaitingMutationApproval
                    | S::RepairRequired
            )
        })
        .take(3)
    {
        let _ = write!(text, " · {}: {:?}", preview(&node.name), node.status);
    }
    if !view.tool_permissions.is_empty()
        || !view.mutation_approvals.is_empty()
        || !view.waits.is_empty()
    {
        text.push_str(" · attention needed (bounded observation)");
    }
    text.push_str(" · Click activity to inspect · /goal.watch · /workflow for decisions");
    text
}

fn format_waits(text: &mut String, view: &WorkflowRunView) {
    for wait in view.waits.iter().take(DETAIL_LIMIT) {
        let _ = write!(
            text,
            "\nWaiting for {:?}: {} · {}",
            wait.kind,
            preview(&wait.node_id),
            wait.prompt
        );
        if wait.node_id == "loop.blocked" {
            if let Some(reason) = wait
                .input
                .as_ref()
                .and_then(|input| input.get("summary"))
                .and_then(serde_json::Value::as_str)
            {
                let _ = write!(
                    text,
                    "\n  Reported reason: {}",
                    reason.chars().take(2048).collect::<String>()
                );
            }
            text.push_str("\n  Existing graph: this checkpoint uses approval for every external blocker. Consent does not resolve missing evidence or dependencies.");
        }
        if wait.kind == bcode_workflow_view_models::WorkflowWaitKind::Approval
            && matches!(wait.node_id.as_str(), "loop.decision" | "loop.blocked")
        {
            let _ = write!(
                text,
                "\n  /goal.unblock {} approve|deny — continuation only, not tool authorization",
                wait.activation_id
            );
        } else if wait.kind == bcode_workflow_view_models::WorkflowWaitKind::Input {
            text.push_str("\n  Use /workflow → Provide input to submit the required update.");
        }
    }
    let reasons = view
        .actions
        .iter()
        .filter_map(|action| action.unavailable_reason.as_deref())
        .collect::<std::collections::BTreeSet<_>>();
    for reason in reasons {
        let _ = write!(text, "\nControl unavailable: {reason}");
    }
}

const DETAIL_LIMIT: usize = 10;

/// Describe existing controls from the canonical run summary, without granting authority.
pub const fn controls(status: bcode_workflow::RunStatus) -> &'static str {
    use bcode_workflow::RunStatus;
    match status {
        RunStatus::Running => {
            "\nControls: /goal.pause stops new attempt admission; already-admitted work may still finish. /goal.stop requests cancellation; it does not undo effects. Refresh /goal.status to observe the outcome."
        }
        RunStatus::Paused => {
            "\nControls: /goal.resume requests continuation, subject to ownership, compatibility and remaining allowances; it does not approve pending requests. Already-admitted work may still finish while paused. /goal.stop requests cancellation without undoing effects."
        }
        RunStatus::RepairRequired => {
            "\nRecovery required: inspect /workflow before taking further action; unresolved operations may have had effects. /goal.detach releases only this session association, without cancelling, repairing or resolving the run. Start a new /goal separately only after reviewing possible effects."
        }
        RunStatus::Failed => {
            "\nGoal failed: inspect the failure and any retained contributions in /workflow. Continuation is available only when explicitly offered below; do not replay unresolved operations."
        }
        RunStatus::Cancelled => {
            "\nGoal cancelled: completed effects and retained workspaces are not undone. Inspect /workflow and the workspace before starting another /goal."
        }
        RunStatus::Completed => {
            "\nExecution finished: inspect the goal evaluation evidence below; a completed run alone does not establish that the original criteria were satisfied."
        }
    }
}

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
    format_waits(&mut text, view);
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
    format_omissions(&mut text, view);
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

fn format_omissions(text: &mut String, view: &WorkflowRunView) {
    for (label, count) in [
        (
            "workers",
            view.nodes
                .iter()
                .filter(|node| node.kind == WorkflowNodeKind::Agent)
                .count(),
        ),
        ("tool approvals", view.tool_permissions.len()),
        ("mutation approvals", view.mutation_approvals.len()),
        ("waits", view.waits.len()),
        ("failures", view.failure_diagnostics.len()),
        ("execution sessions", view.child_sessions.len()),
    ] {
        if count > DETAIL_LIMIT {
            let _ = write!(
                text,
                "\nAdditional {label} omitted from this preview: {} in this snapshot. Inspect /workflow; this count is not a full-run total.",
                count - DETAIL_LIMIT
            );
        }
    }
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
    text.push_str("\nEvaluation evidence is reported, not independently verified by this display.");
}

#[cfg(test)]
pub mod tests {
    #[test]
    fn overview_reports_attention_without_claiming_completion() {
        let mut snapshot = view();
        snapshot.run.display_title = "Inspect 界👩‍💻".into();
        snapshot
            .nodes
            .push(bcode_workflow_view_models::WorkflowNodeView {
                node_id: "implementation".into(),
                name: "Implementation".into(),
                kind: WorkflowNodeKind::Agent,
                activation_id: Some("attempt".into()),
                status: bcode_workflow_view_models::WorkflowNodeStatus::WaitingApproval,
            });
        let text = overview(&snapshot);
        assert!(text.contains("Inspect 界👩‍💻"));
        assert!(text.contains("Implementation: WaitingApproval"));
        assert!(text.contains("/workflow"));
        assert!(!text.contains("100%"));
        snapshot.version = u32::MAX;
        assert!(overview(&snapshot).contains("unsupported view"));
        assert!(!overview(&snapshot).contains("Implementation"));
    }

    #[test]
    fn controls_distinguish_admission_cancellation_and_recovery() {
        use bcode_workflow::RunStatus;

        let running = super::controls(RunStatus::Running);
        assert!(running.contains("/goal.pause"));
        assert!(running.contains("already-admitted work may still finish"));
        assert!(running.contains("/goal.stop requests cancellation"));
        assert!(running.contains("does not undo effects"));

        let paused = super::controls(RunStatus::Paused);
        assert!(paused.contains("/goal.resume"));
        assert!(paused.contains("does not approve pending requests"));
        assert!(paused.contains("remaining allowances"));

        let repair = super::controls(RunStatus::RepairRequired);
        assert!(repair.contains("/goal.detach releases only this session association"));
        assert!(repair.contains("may have had effects"));
        assert!(!repair.contains("/goal.resume"));
        for status in [
            RunStatus::Completed,
            RunStatus::Failed,
            RunStatus::Cancelled,
        ] {
            let text = super::controls(status);
            assert!(!text.contains("/goal.resume"));
            assert!(!text.contains("/goal.stop"));
        }
        assert!(super::controls(RunStatus::Completed).contains("does not establish"));
        assert!(super::controls(RunStatus::Failed).contains("only when explicitly offered"));
        assert!(super::controls(RunStatus::Cancelled).contains("not undone"));
    }
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
                "evidence":["integrated.sh: expected 27, observed 28"]

            }}
        })).unwrap()
        };
        snapshot.outputs = vec![output("worker", true), output("final", false)];
        let text = format(&snapshot);
        assert!(text.contains("criteria not satisfied"));
        assert!(!text.contains("criteria reported satisfied"));
        assert!(text.contains("Combined check failed Retain both contributions"));
        assert!(text.contains("integrated.sh: expected 27, observed 28"));
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

    #[test]
    fn bounded_worker_preview_reports_omissions_without_claiming_global_totals() {
        let mut snapshot = view();
        let worker = snapshot.nodes[0].clone();
        snapshot.nodes = vec![worker; DETAIL_LIMIT];
        assert!(!format(&snapshot).contains("Additional workers omitted"));
        snapshot.nodes.push(snapshot.nodes[0].clone());
        let text = format(&snapshot);
        assert_eq!(
            text.matches("Implement change [worker]").count(),
            DETAIL_LIMIT
        );
        assert!(text.contains("Additional workers omitted from this preview: 1 in this snapshot"));
        assert!(text.contains("this count is not a full-run total"));
        assert_eq!(snapshot.nodes.len(), DETAIL_LIMIT + 1);
    }

    #[test]
    fn completed_worker_is_not_goal_completion() {
        let text = format(&view());
        assert!(text.contains("Implement change [worker] · Completed"));
        assert!(text.contains("No terminal result"));
        assert!(text.contains("bounded snapshot"));
    }

    #[test]
    fn approval_and_input_waits_offer_distinct_controls() {
        use bcode_workflow_view_models::{WorkflowWaitKind, WorkflowWaitView};

        let mut snapshot = view();
        snapshot.waits.push(WorkflowWaitView {
            node_id: "loop.decision".into(),
            activation_id: "decision-activation".into(),
            kind: WorkflowWaitKind::Approval,
            prompt: "Approve continuation?".into(),
            expected_schema: None,
            input: None,
            requested_at_ms: 1,
        });
        let approval = format(&snapshot);
        assert!(approval.contains("Approve continuation?"));
        assert!(approval.contains("/goal.unblock decision-activation approve|deny"));
        assert!(approval.contains("continuation only, not tool authorization"));
        assert!(!approval.contains("Provide input"));

        snapshot.waits[0].node_id = "loop.requirement".into();
        snapshot.waits[0].kind = WorkflowWaitKind::Input;
        snapshot.waits[0].prompt = "Supply the missing dependency".into();
        let input = format(&snapshot);
        assert!(input.contains("Supply the missing dependency"));
        assert!(input.contains("/workflow → Provide input"));
        assert!(!input.contains("/goal.unblock"));

        snapshot.waits[0].node_id = "loop.blocked".into();
        snapshot.waits[0].kind = WorkflowWaitKind::Approval;
        snapshot.waits[0].input = Some(serde_json::json!({"summary":"Dependency unavailable"}));
        let legacy = format(&snapshot);
        assert!(legacy.contains("Dependency unavailable"));
        assert!(legacy.contains("Consent does not resolve missing evidence or dependencies"));
        assert!(legacy.contains("/goal.unblock decision-activation approve|deny"));
    }

    #[test]
    fn repair_required_does_not_present_retained_positive_output_as_success() {
        let mut snapshot = view();
        snapshot.terminal = Some(WorkflowTerminalView::RepairRequired);
        snapshot.outputs = vec![
            serde_json::from_value(serde_json::json!({
                "output_id":"prior","node_id":"evaluation","activation_id":"a",
                "schema_id":"loop","schema_version":1,"checksum_sha256":"checksum",
                "artifact_reference":null,"created_at_ms":0,
                "value":{"availability":"resolved","value":{
                    "implementation_prompt":"objective","stop_condition":"criteria",
                    "max_iterations":1,"iteration":1,"condition_met":true,
                    "summary":"Earlier successful result", "evidence":[]
                }}
            }))
            .unwrap(),
        ];
        let text = format(&snapshot);
        assert!(text.contains("Repair required: do not retry ambiguous effects"));
        assert!(!text.contains("criteria reported satisfied"));
        assert!(!text.contains("Earlier successful result"));
        assert!(!text.contains("Workflow finished"));
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
        assert!(text.contains("Additional mutation approvals omitted from this preview: 1"));
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
