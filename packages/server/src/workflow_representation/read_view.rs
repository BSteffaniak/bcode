//! Bounded basic inspection for new-family runs; missing graph detail remains explicit.
use super::{
    RunLookup, ServerError, ServerState, lookup_at, retained_store, unavailable, unsupported,
};
use bcode_workflow_view_models::{
    WORKFLOW_VIEW_VERSION, WorkflowOutputValue, WorkflowOutputView, WorkflowProjectionHealth,
    WorkflowRunView, WorkflowTerminalView,
};

/// Return a basic new-family view; None delegates to the unchanged legacy view path.
/// # Errors
/// Rejects conflicting/unsupported/damaged identity and canonical summary/output failures.
pub async fn run_view(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<Option<WorkflowRunView>, ServerError> {
    let retained = retained_store(state)?;
    match lookup_at(retained.path(), run_id).await? {
        RunLookup::Legacy | RunLookup::Missing => return Ok(None),
        RunLookup::Available(_) => {}
        RunLookup::UnsupportedRepresentation => return Err(unsupported()),
        RunLookup::Degraded => return Err(unavailable()),
    }
    let summary = super::status(state, run_id)
        .await?
        .ok_or_else(unavailable)?;
    let terminal = match summary.status {
        bcode_workflow::RunStatus::Completed => Some(WorkflowTerminalView::Completed {
            output_id: summary.terminal_output_id.clone().ok_or_else(unavailable)?,
        }),
        bcode_workflow::RunStatus::Failed => Some(WorkflowTerminalView::Failed),
        bcode_workflow::RunStatus::Cancelled => Some(WorkflowTerminalView::Cancelled),
        bcode_workflow::RunStatus::RepairRequired => Some(WorkflowTerminalView::RepairRequired),
        _ => None,
    };
    let outputs = super::outputs::run_outputs(state, run_id, limit)
        .await?
        .into_iter()
        .map(|output| WorkflowOutputView {
            output_id: output.output_id,
            node_id: output.node_id,
            activation_id: output.activation_id,
            schema_id: output.schema_id,
            schema_version: output.schema_version,
            checksum_sha256: output.checksum_sha256,
            value: WorkflowOutputValue::Resolved {
                value: output.value,
            },
            artifact_reference: output.artifact_reference,
            created_at_ms: output.created_at_ms,
        })
        .collect();
    let mut view = WorkflowRunView {
        version: WORKFLOW_VIEW_VERSION, run: super::catalog::basic_row(summary),
        nodes: vec![], activations: vec![], edges: vec![], waits: vec![], mutation_approvals: vec![], attempts: vec![], retry_schedules: vec![], outputs,
        failure_diagnostics: vec![], scheduling: None, descendant_runs: vec![], tool_permissions: vec![], child_sessions: vec![], actions: vec![], terminal,
        health: WorkflowProjectionHealth::Degraded { reason: "Basic canonical summary and bounded output prefix only; graph, progress, ancestry, scheduling and controls are not projected. Empty sections mean unavailable, not absent. Output continuation is not exposed by this view.".into() },
    };
    super::views::populate_nodes(state, run_id, limit, &mut view).await?;
    view.scheduling = Some(
        super::scheduling::page(
            state,
            bcode_workflow::WorkflowSchedulingPageRequest {
                version: bcode_workflow::WORKFLOW_SCHEDULING_PAGE_VERSION,
                run_id: run_id.into(),
                limit,
                cursor: None,
            },
        )
        .await?
        .observation,
    );
    super::gate_views::populate(state, run_id, limit, &mut view).await?;
    Ok(Some(view))
}
