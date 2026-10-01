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
    let mut text = format!(
        "{} · {:?}",
        preview(&view.run.display_title),
        view.run.status
    );
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
            "\nExecution finished: inspect the goal evaluation and delivery evidence below; a completed run alone does not establish that the original criteria were satisfied."
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
    if let Some(delivery) = &result.delivery {
        delivery_location(text, delivery);
        for target in delivery.integrated_targets.iter().take(DETAIL_LIMIT) {
            let _ = write!(text, "\n  Integrated target: {}", preview(target));
        }
        for criterion in delivery.criteria.iter().take(DETAIL_LIMIT) {
            let _ = write!(
                text,
                "\n  Criterion [{}]: {} · {} · {}",
                observation(&criterion.status),
                preview(&criterion.description),
                preview(&criterion.evidence),
                match criterion.basis {
                    Some(crate::delivery::CriterionBasis::ObservedCheck) =>
                        "claimed observed check",
                    Some(crate::delivery::CriterionBasis::Review) => "review judgment",
                    Some(crate::delivery::CriterionBasis::Unknown) | None => "unknown basis",
                }
            );
        }
        for check in delivery.checks.iter().take(DETAIL_LIMIT) {
            let _ = write!(
                text,
                "\n  Check [{}]: {} · workspace {} · {}",
                observation(&check.outcome),
                preview(&check.command),
                preview(&check.workspace),
                preview(&check.evidence)
            );
        }
        for unresolved in delivery.unresolved_work.iter().take(DETAIL_LIMIT) {
            let _ = write!(text, "\n  Unresolved: {}", preview(unresolved));
        }
        for workspace in delivery.retained_workspaces.iter().take(DETAIL_LIMIT) {
            let _ = write!(text, "\n  Retained workspace: {}", preview(workspace));
        }
        contribution_reviews(text, delivery);
        if [
            delivery.resolutions.len(),
            delivery.integrated_targets.len(),
            delivery.criteria.len(),
            delivery.checks.len(),
            delivery.unresolved_work.len(),
            delivery.retained_workspaces.len(),
            delivery.contribution_output_ids.len(),
        ]
        .into_iter()
        .any(|count| count > DETAIL_LIMIT)
        {
            text.push_str(
                "\n  Additional delivery details omitted; inspect /workflow for the full report.",
            );
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

fn contribution_reviews(text: &mut String, delivery: &crate::delivery::DeliveryReport) {
    for output in delivery.contribution_output_ids.iter().take(DETAIL_LIMIT) {
        let _ = write!(text, "\n  Contribution reference: {}", preview(output));
    }
    for resolution in delivery.resolutions.iter().take(DETAIL_LIMIT) {
        let _ = write!(
            text,
            "\n  Resolution review: {} · checksum {} · {} historical items · {} referenced checks · {}",
            preview(&resolution.output_id),
            preview(&resolution.checksum_sha256),
            resolution.item_paths.len(),
            resolution.check_indices.len(),
            preview(&resolution.evidence)
        );
    }
}

fn delivery_location(text: &mut String, delivery: &crate::delivery::DeliveryReport) {
    text.push_str("\nDelivery report (evaluator-reported):");
    if delivery.version == crate::delivery::ReportVersion::V2 {
        text.push_str("\n  Delivered location: canonical result delivery.delivered_snapshot (complete retained UTF-8 bytes). Not live checkout freshness or hermetic environment verification; reviews remain judgments.");
    } else if delivery.version == crate::delivery::ReportVersion::V3 {
        if let Some(repository) = &delivery.repository_delivery {
            let _ = write!(
                text,
                "\n  Repository export: {} · commit {} · SHA-256 {}",
                preview(&repository.artifact),
                preview(&repository.target.commit),
                preview(&repository.sha256)
            );
            text.push_str("\n  Delivered location: canonical result delivery.repository_delivery references the retained complete commit export, not uncommitted checkout files. Checks do not establish live checkout freshness or a hermetic environment. Resolution reviews retain historical evidence and remain judgments.");
        } else {
            text.push_str("\n  Repository delivery missing; target identity is unverified.");
        }
    }
}

const fn observation(value: &crate::delivery::Observation) -> &'static str {
    match value {
        crate::delivery::Observation::Passed => "passed",
        crate::delivery::Observation::Failed => "failed",
        crate::delivery::Observation::Unverified => "unverified",
    }
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
                    "checks":[{"command":"sh verify.sh", "workspace":"integration", "outcome":"failed", "evidence":"exit 1"}], "retained_workspaces":["worker-right"], "unresolved_work":["fix surcharge"]
                }
            }}
        })).unwrap()
        };
        snapshot.outputs = vec![output("worker", true), output("final", false)];
        let text = format(&snapshot);
        assert!(text.contains("criteria not satisfied"));
        assert!(!text.contains("criteria reported satisfied"));
        assert!(text.contains("Combined check failed Retain both contributions"));
        assert!(text.contains("Criterion [failed]: total is 27 · observed 28"));
        assert!(text.contains("Check [failed]: sh verify.sh · workspace integration · exit 1"));
        assert!(text.contains("Unresolved: fix surcharge"));
        assert!(text.contains("Retained workspace: worker-right"));
        assert!(text.contains("Contribution reference: right"));
        assert!(text.contains("integrated.sh: expected 27, observed 28"));
        assert!(text.contains("Integrated target: integrated.sh"));
        assert!(
            text.contains("2 contribution references · 1 criteria · 1 checks · 1 unresolved items")
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

    #[test]
    fn repository_delivery_and_resolution_reviews_remain_bounded_claims() {
        let mut snapshot = view();
        snapshot.terminal = Some(WorkflowTerminalView::Completed {
            output_id: "final".into(),
        });
        snapshot.outputs = vec![serde_json::from_value(serde_json::json!({
            "output_id":"final","node_id":"evaluation","activation_id":"a",
            "schema_id":"loop","schema_version":1,"checksum_sha256":"checksum",
            "artifact_reference":null,"created_at_ms":0,
            "value":{"availability":"resolved","value":{
                "implementation_prompt":"objective","stop_condition":"criteria",
                "max_iterations":1,"iteration":1,"condition_met":false,
                "summary":"Live acceptance unknown","evidence":["Retained target"],
                "delivery":{
                    "version":"3","integrated_targets":["artifact"],
                    "repository_delivery":{"target":{"version":1,"commit":"a".repeat(40)},"artifact":"artifact","sha256":"b".repeat(64)},
                    "contribution_output_ids":["worker"],"criteria":[],"checks":[],
                    "retained_workspaces":["integration"],"unresolved_work":["Live acceptance"],
                    "resolutions":vec![serde_json::json!({"output_id":"worker","checksum_sha256":"c".repeat(64),"item_paths":["/blockers/0"],"check_indices":[0],"evidence":format!("Reviewed\n{}", "界".repeat(400))}); 11]
                }
            }}
        })).unwrap()];
        let text = format(&snapshot);
        assert!(text.contains(&format!(
            "Repository export: artifact · commit {} · SHA-256 {}",
            "a".repeat(40),
            "b".repeat(64)
        )));
        assert!(text.contains("not uncommitted checkout files"));
        assert!(text.contains("hermetic environment"));
        assert!(text.contains("remain judgments"));
        assert_eq!(text.matches("Resolution review:").count(), DETAIL_LIMIT);
        assert!(text.contains("Additional delivery details omitted"));
        assert!(text.contains("Reviewed 界"));
        assert!(!text.contains(&"界".repeat(321)));
        assert!(text.contains("Unresolved: Live acceptance"));
        assert!(text.contains("not independently verified"));
    }

    #[test]
    fn delivery_details_are_bounded_and_do_not_upgrade_unknown_checks() {
        let mut snapshot = view();
        snapshot.terminal = Some(WorkflowTerminalView::Completed {
            output_id: "final".into(),
        });
        snapshot.outputs = vec![serde_json::from_value(serde_json::json!({
            "output_id":"final","node_id":"evaluation","activation_id":"a",
            "schema_id":"loop","schema_version":1,"checksum_sha256":"checksum",
            "artifact_reference":null,"created_at_ms":0,
            "value":{"availability":"resolved","value":{
                "implementation_prompt":"objective","stop_condition":"criteria",
                "max_iterations":1,"iteration":1,"condition_met":false,
                "summary":"verification pending", "evidence":["check not run"],
                "delivery": {
                    "version":"1", "integrated_targets":vec!["target"; 11],
                    "contribution_output_ids":vec!["output"; 11],
                    "criteria":vec![serde_json::json!({"criterion":"required", "status":"unverified", "evidence":"not checked"}); 11],
                    "checks":vec![serde_json::json!({"command":"verify", "workspace":"workspace", "outcome":"unverified", "evidence":"not run"}); 11],
                    "retained_workspaces":vec!["worker"; 11],
                    "unresolved_work":vec![format!("repair\n{}", "界".repeat(400)); 11]
                }
            }}
        })).unwrap()];
        let text = format(&snapshot);
        for label in [
            "Integrated target:",
            "Contribution reference:",
            "Criterion [unverified]:",
            "Check [unverified]:",
            "Retained workspace:",
            "Unresolved:",
        ] {
            assert_eq!(text.matches(label).count(), DETAIL_LIMIT, "{label}");
        }
        assert!(text.contains("Additional delivery details omitted"));
        assert!(text.contains("Unresolved: repair 界"));
        assert!(!text.contains(&"界".repeat(321)));
        assert!(!text.contains("Check [passed]"));
        assert!(text.contains("not independently verified"));
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
