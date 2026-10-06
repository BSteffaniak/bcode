//! Canonical new-family aggregate observation. Unsupported collections remain explicit.
use super::{
    RunLookup, ServerError, ServerState, lookup_at, open, representation, retained_store,
    unavailable, unsupported,
};
use bcode_workflow::{
    WorkflowActivationInputSummary, WorkflowActivationSummary, WorkflowDefinitionRepresentation,
    WorkflowDefinitionSnapshot, WorkflowOutputSummary, WorkflowRunInspection,
    WorkflowTerminalOutputInspection,
};
use sha2::Digest;

fn stage(name: &str) -> ServerError {
    ServerError::WorkflowStorageUnavailable(format!("workflow inspection unavailable at {name}"))
}

pub async fn inspect(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<Option<WorkflowRunInspection>, ServerError> {
    let retained = retained_store(state)?;
    match lookup_at(retained.path(), run_id).await? {
        RunLookup::Legacy => return Ok(None),
        RunLookup::Available(_) => {}
        RunLookup::UnsupportedRepresentation => return Err(unsupported()),
        RunLookup::Missing | RunLookup::Degraded => return Err(unavailable()),
    }
    let bound = u16::try_from(limit)
        .ok()
        .filter(|limit| (1..=100).contains(limit))
        .ok_or_else(unavailable)?;
    let db = open(retained.path(), false)?;
    let run = super::status(state, run_id)
        .await?
        .ok_or_else(unavailable)?;
    let definition = definition(&retained, &run).map_err(|_| stage("definition"))?;
    let revision =
        super::backpressure::retry(|| representation::graph_pages::revision(&db, run_id))
            .await
            .map_err(|_| stage("revision"))?;
    let graph = super::inspection_graph::page(
        state,
        &bcode_workflow::WorkflowRunGraphPageRequest {
            run_id: run_id.into(),
            expected_revision: revision,
            after_node_id: None,
            after_edge_id: None,
            limit,
        },
    )
    .await
    .map_err(|_| stage("graph"))?;
    let terminal_output = terminal(&db, &run).await.map_err(|_| stage("terminal"))?;
    let activations = activations(&db, run_id, bound)
        .await
        .map_err(|_| stage("activations"))?;
    let attempts = super::backpressure::retry(|| {
        representation::inspection_attempts::page(&db, run_id, None, bound)
    })
    .await
    .map_err(|_| stage("attempts"))?;
    let outputs = output_summaries(state, run_id, limit).await?;
    let child_sessions = sessions(state, &db, run_id, bound)
        .await
        .map_err(|_| stage("sessions"))?;
    let child_run_links = super::backpressure::retry(|| {
        representation::hierarchy::children(&db, run_id, None, bound)
    })
    .await
    .map_err(|_| stage("children"))?
    .children;
    let sessions_complete = attempts.len() < usize::from(bound);
    Ok(Some(WorkflowRunInspection {
        unavailable_collections: vec![
            "events",
            "waits",
            "mutation_approvals",
            "decisions",
            "grants",
            "resource_leases",
            "descendant_runs",
            "repeat_outcomes",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain((!sessions_complete).then(|| "child_sessions".to_owned()))
        .collect(),
        execution_allowance: None,
        continuation: None,
        replacement_readiness: None,
        run,
        graph: Some(graph),
        definition,
        terminal_output,
        activations,
        attempts,
        outputs,
        events: vec![],
        waits: vec![],
        mutation_approvals: vec![],
        decisions: vec![],
        grants: vec![],
        resource_leases: vec![],
        child_run_links,
        descendant_runs: vec![],
        repeat_outcomes: vec![],
        child_sessions,
        coordinator: None,
    }))
}

async fn sessions(
    state: &ServerState,
    db: &dyn switchy_database::Database,
    run_id: &str,
    bound: u16,
) -> Result<Vec<bcode_session_models::SessionSummary>, ServerError> {
    let links = super::backpressure::retry(|| {
        representation::inspection_sessions::links(db, run_id, None, bound)
    })
    .await
    .map_err(|_| unavailable())?;
    let mut summaries = Vec::new();
    for link in links {
        let id = link.session_id.parse().map_err(|_| unavailable())?;
        let summary = state.sessions.session_summary(id).await?;
        if summary.execution.as_ref().is_none_or(|execution| {
            execution.provenance.run_id != run_id
                || execution.provenance.node_id != link.node_id
                || execution.provenance.activation_id.as_deref()
                    != Some(link.activation_id.as_str())
                || execution.provenance.attempt != link.attempt
        }) {
            return Err(unavailable());
        }
        summaries.push(summary);
    }
    Ok(summaries)
}

fn definition(
    store: &bcode_workflow_store::WorkflowStore,
    run: &bcode_workflow::WorkflowRunSummary,
) -> Result<WorkflowDefinitionSnapshot, ServerError> {
    let stored = store
        .definition(&run.definition_id, run.definition_version)?
        .ok_or_else(unavailable)?;
    if hex::encode(sha2::Sha256::digest(stored.definition_json.as_bytes()))
        != stored.checksum_sha256
    {
        return Err(unavailable());
    }
    let parsed = WorkflowDefinitionRepresentation::parse(stored.definition_json)
        .map_err(|_| unavailable())?;
    Ok(WorkflowDefinitionSnapshot {
        definition_id: stored.definition_id,
        version: stored.version,
        checksum_sha256: stored.checksum_sha256,
        definition: parsed,
    })
}

async fn terminal(
    db: &dyn switchy_database::Database,
    run: &bcode_workflow::WorkflowRunSummary,
) -> Result<Option<WorkflowTerminalOutputInspection>, ServerError> {
    if run.status != bcode_workflow::RunStatus::Completed {
        return Ok(None);
    }
    let run_id = &run.run_id;
    let terminal_output = representation::graph::completion::terminal(db, run_id)
        .await
        .map_err(|_| unavailable())?
        .map(|output| {
            let checksum = bcode_workflow::workflow_canonical_value_sha256(&output.value)
                .map_err(|_| unavailable())?;
            if run.terminal_output_id.as_deref() != Some(output.output_id.as_str())
                || run.terminal_output_checksum_sha256.as_deref() != Some(checksum.as_str())
            {
                return Err(unavailable());
            }
            Ok(WorkflowTerminalOutputInspection {
                version: bcode_workflow::WORKFLOW_TERMINAL_OUTPUT_INSPECTION_VERSION,
                checksum_sha256: checksum,
                output_id: output.output_id,
                node_id: output.node_id,
                activation_id: output.activation_id,
                schema_id: output.schema_id,
                schema_version: output.schema_version,
                value: output.value,
                artifact_reference: output.artifact_reference,
                created_at_ms: output.created_at_ms,
            })
        })
        .transpose()?;
    Ok(terminal_output)
}

async fn activations(
    db: &dyn switchy_database::Database,
    run_id: &str,
    bound: u16,
) -> Result<Vec<WorkflowActivationSummary>, ServerError> {
    let scheduling = super::backpressure::retry(|| {
        representation::scheduling::snapshot(db, run_id, None, None, bound)
    })
    .await
    .map_err(|_| unavailable())?;
    let mut activations = Vec::new();
    for activation in scheduling.activations {
        let (generation, created_at_ms, byte_count) = super::backpressure::retry(|| {
            representation::inspection_activations::metadata(db, run_id, &activation.activation_id)
        })
        .await
        .map_err(|_| unavailable())?;
        activations.push(WorkflowActivationSummary {
            run_id: run_id.into(),
            node_id: activation.node_id,
            activation_id: activation.activation_id,
            dependency_generation: generation,
            status: match activation.status {
                representation::scheduling::ActivationState::Skipped => "skipped",
                representation::scheduling::ActivationState::OutputCommitted => "completed",
                representation::scheduling::ActivationState::Unresolved => "unresolved",
            }
            .into(),
            has_output: activation.status
                == representation::scheduling::ActivationState::OutputCommitted,
            input_summary: WorkflowActivationInputSummary::Omitted { byte_count },
            created_at_ms,
        });
    }
    Ok(activations)
}

async fn output_summaries(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<Vec<WorkflowOutputSummary>, ServerError> {
    let outputs = super::outputs::run_outputs(state, run_id, limit)
        .await
        .map_err(|_| stage("outputs"))?
        .into_iter()
        .map(|output| WorkflowOutputSummary {
            output_id: output.output_id,
            run_id: output.run_id,
            node_id: output.node_id,
            activation_id: output.activation_id,
            schema_id: output.schema_id,
            schema_version: output.schema_version,
            checksum_sha256: output.checksum_sha256,
            artifact_reference: output.artifact_reference,
            created_at_ms: output.created_at_ms,
        })
        .collect();
    Ok(outputs)
}
