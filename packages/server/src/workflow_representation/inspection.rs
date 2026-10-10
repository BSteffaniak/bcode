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

mod admission;

fn stage(name: &str) -> ServerError {
    ServerError::WorkflowStorageUnavailable(format!("workflow inspection unavailable at {name}"))
}

fn verify_revision(expected: u64, observed: u64) -> Result<(), ServerError> {
    if expected != observed {
        return Err(stage("revision_changed"));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct CollectionCompleteness {
    admission_observations: Option<bool>,
    child_sessions: Option<bool>,
    activations: Option<bool>,
    child_run_links: Option<bool>,
}

fn unavailable_collections(complete: CollectionCompleteness) -> Vec<String> {
    let unsupported = [
        "events",
        "waits",
        "mutation_approvals",
        "decisions",
        "grants",
        "resource_leases",
        "descendant_runs",
        "repeat_outcomes",
    ];
    unsupported
        .into_iter()
        .chain(
            [
                ("admission_observations", complete.admission_observations),
                ("child_sessions", complete.child_sessions),
                ("activations", complete.activations),
                ("child_run_links", complete.child_run_links),
            ]
            .into_iter()
            .filter_map(|(name, completeness)| (completeness != Some(true)).then_some(name)),
        )
        .map(str::to_owned)
        .collect()
}

pub async fn inspect(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<Option<WorkflowRunInspection>, ServerError> {
    let retained = retained_store(state).await?;
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
    let (activations, activations_complete) = activations(&db, run_id, revision, bound)
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
    let children = super::backpressure::retry(|| {
        representation::hierarchy::children(&db, run_id, None, bound)
    })
    .await
    .map_err(|_| stage("children"))?;
    let children_complete = children.next_child_run_id.is_none();
    let child_run_links = children.children;
    let (admission_observations, admission_observations_complete) =
        admission::inspect(&db, run_id, bound).await;
    // A graph revision fences topology only, not concurrent attempt/output transitions.
    // Reject changed topology rather than returning a mixed-revision aggregate.
    let final_revision =
        super::backpressure::retry(|| representation::graph_pages::revision(&db, run_id))
            .await
            .map_err(|_| stage("final_revision"))?;
    verify_revision(revision, final_revision)?;
    let sessions_complete = attempts.len() < usize::from(bound);
    Ok(Some(WorkflowRunInspection {
        admission_observations,
        admission_observations_complete,
        unavailable_collections: unavailable_collections(CollectionCompleteness {
            admission_observations: admission_observations_complete,
            child_sessions: Some(sessions_complete),
            activations: Some(activations_complete),
            child_run_links: Some(children_complete),
        }),
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
        let summary = state.sessions.session_summary_non_owning(id).await?;
        let child_verified = summary.execution.as_ref().is_some_and(|execution| {
            execution.provenance.run_id == run_id
                && execution.provenance.node_id == link.node_id
                && execution.provenance.activation_id.as_deref()
                    == Some(link.activation_id.as_str())
                && execution.provenance.attempt == link.attempt
        });
        if !child_verified && !shared_parent_link(db, run_id, &link).await? {
            return Err(unavailable());
        }
        summaries.push(summary);
    }
    Ok(summaries)
}

/// Verify the explicit canonical shared-parent target without rewriting session provenance.
pub(super) async fn shared_parent_link(
    db: &dyn switchy_database::Database,
    run_id: &str,
    link: &bcode_workflow_store::WorkflowExecutionSessionLink,
) -> Result<bool, ServerError> {
    let run = representation::runs::summary(db, run_id)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unavailable)?;
    if link.run_id != run_id
        || run.parent_session_id.as_deref() != Some(link.session_id.as_str())
        || run.workspace_snapshot != link.workspace_snapshot
    {
        return Ok(false);
    }
    let canonical = representation::attempts::owner_boundary::execution_session_link(
        db,
        run_id,
        &link.node_id,
        &link.activation_id,
        link.attempt,
    )
    .await
    .map_err(|_| unavailable())?;
    if canonical.as_ref() != Some(link) {
        return Ok(false);
    }
    let node = representation::graph::execution_node(db, run_id, &link.node_id)
        .await
        .map_err(|_| unavailable())?;
    if node.kind != bcode_workflow::NodeKind::Agent {
        return Ok(false);
    }
    let configuration =
        crate::workflow_prompt_configuration(&node.configuration).map_err(|_| unavailable())?;
    Ok(configuration.execution_target
        == bcode_workflow::PromptContextTarget::SharedParentSequential)
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
    expected_revision: u64,
    bound: u16,
) -> Result<(Vec<WorkflowActivationSummary>, bool), ServerError> {
    let scheduling = super::backpressure::retry(|| {
        representation::scheduling::snapshot(db, run_id, Some(expected_revision), None, bound)
    })
    .await
    .map_err(|_| unavailable())?;
    let complete = scheduling.next_activation_id.is_none();
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
    Ok((activations, complete))
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

#[cfg(test)]
mod tests {
    use super::{CollectionCompleteness, unavailable_collections, verify_revision};

    #[test]
    fn unknown_and_incomplete_collections_do_not_claim_availability() {
        for status in [None, Some(false), Some(true)] {
            let names = unavailable_collections(CollectionCompleteness {
                admission_observations: status,
                child_sessions: status,
                activations: status,
                child_run_links: status,
            });
            for name in [
                "admission_observations",
                "child_sessions",
                "activations",
                "child_run_links",
            ] {
                assert_eq!(
                    names.iter().any(|value| value == name),
                    status != Some(true)
                );
            }
            // Supported collection completeness cannot certify legacy event history.
            assert!(names.iter().any(|name| name == "events"));
            assert!(names.iter().any(|name| name == "descendant_runs"));
        }
    }

    #[test]
    fn aggregate_topology_requires_the_pinned_revision() {
        assert!(verify_revision(7, 7).is_ok());
        assert!(verify_revision(7, 8).is_err());
        assert!(verify_revision(8, 7).is_err());
    }
}
