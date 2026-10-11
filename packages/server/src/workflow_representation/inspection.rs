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
use switchy_database::Database as _;

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

fn attempts_settled(attempts: &[bcode_workflow::AttemptSummary], bound: u16) -> bool {
    attempts.len() < usize::from(bound)
        && attempts.iter().all(|attempt| {
            attempt.terminal_at_ms.is_some()
                && matches!(
                    attempt.status.as_str(),
                    "succeeded" | "failed" | "cancelled"
                )
        })
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
    inspect_canonical(state, run_id, limit, bound, &retained)
        .await
        .map(Some)
}

async fn inspect_canonical(
    state: &ServerState,
    run_id: &str,
    limit: usize,
    bound: u16,
    retained: &bcode_workflow_store::WorkflowStore,
) -> Result<WorkflowRunInspection, ServerError> {
    let db = open(retained.path(), false)?;
    // One SQLite read snapshot fences execution transitions as well as topology.
    let tx = db
        .begin_transaction()
        .await
        .map_err(|_| stage("snapshot"))?;
    let run = representation::runs::summary_in(&*tx, run_id)
        .await
        .map_err(|_| stage("run"))?
        .ok_or_else(unavailable)?;
    let definition = definition(retained, &run).map_err(|_| stage("definition"))?;
    let revision =
        super::backpressure::retry(|| representation::graph_pages::revision(&*tx, run_id))
            .await
            .map_err(|_| stage("revision"))?;
    let graph = canonical_graph(&*tx, run_id, revision, limit, bound).await?;
    let terminal_output = terminal(&*tx, &run).await.map_err(|_| stage("terminal"))?;
    let (activations, activations_complete) = activations(&*tx, run_id, revision, bound)
        .await
        .map_err(|_| stage("activations"))?;
    let attempts = super::backpressure::retry(|| {
        representation::inspection_attempts::page_in(&*tx, run_id, None, bound)
    })
    .await
    .map_err(|_| stage("attempts"))?;
    let outputs = output_summaries(&*tx, run_id, bound).await?;
    let child_sessions = sessions(state, &*tx, run_id, bound)
        .await
        .map_err(|_| stage("sessions"))?;
    let children = super::backpressure::retry(|| {
        representation::hierarchy::children_in(&*tx, run_id, None, bound)
    })
    .await
    .map_err(|_| stage("children"))?;
    let children_complete = children.next_child_run_id.is_none();
    let child_run_links = children.children;
    let (admission_observations, admission_observations_complete) =
        admission::inspect(&*tx, run_id, bound).await;
    // A graph revision fences topology only, not concurrent attempt/output transitions.
    // Reject changed topology rather than returning a mixed-revision aggregate.
    let final_revision =
        super::backpressure::retry(|| representation::graph_pages::revision(&*tx, run_id))
            .await
            .map_err(|_| stage("final_revision"))?;
    verify_revision(revision, final_revision)?;
    let sessions_complete =
        attempts.len() < usize::from(bound) && child_sessions.len() < usize::from(bound);
    let exhausted = bcode_workflow::WorkflowCanonicalCollectionCoverage::from_verified;
    let canonical_eval_coverage = Some(bcode_workflow::WorkflowCanonicalEvalCoverage {
        version: 1,
        run_id: run_id.into(),
        graph_revision: revision,
        terminal_status: run.status,
        graph_complete: exhausted(
            graph.nodes_complete
                && graph.edges_complete
                && graph.revision == revision
                && (run.status != bcode_workflow::RunStatus::Completed
                    || terminal_output.is_some()),
        ),
        activations_complete: exhausted(activations_complete),
        attempts_complete: exhausted(attempts_settled(&attempts, bound)),
        outputs_complete: exhausted(outputs.len() < usize::from(bound)),
        admission_complete: exhausted(admission_observations_complete == Some(true)),
        sessions_complete: exhausted(sessions_complete),
        no_child_runs: exhausted(children_complete && child_run_links.is_empty()),
    });
    tx.commit().await.map_err(|_| stage("snapshot_commit"))?;
    Ok(WorkflowRunInspection {
        canonical_eval_coverage,
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
    })
}

async fn canonical_graph(
    tx: &dyn switchy_database::DatabaseTransaction,
    run_id: &str,
    revision: u64,
    limit: usize,
    bound: u16,
) -> Result<bcode_workflow::WorkflowRunGraphInspection, ServerError> {
    super::inspection_graph::read_in(
        tx,
        &bcode_workflow::WorkflowRunGraphPageRequest {
            run_id: run_id.into(),
            expected_revision: revision,
            after_node_id: None,
            after_edge_id: None,
            limit,
        },
        bound,
    )
    .await
    .map_err(|_| stage("graph"))
}

async fn sessions(
    state: &ServerState,
    db: &dyn switchy_database::DatabaseTransaction,
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
        if !child_verified
            && !super::workflow_activity::verify_shared_parent_link_in(db, run_id, &link)
                .await
                .map_err(super::workflow_activity::SharedVerificationFailure::into_server_error)?
        {
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
    db: &dyn switchy_database::DatabaseTransaction,
    run: &bcode_workflow::WorkflowRunSummary,
) -> Result<Option<WorkflowTerminalOutputInspection>, ServerError> {
    if run.status != bcode_workflow::RunStatus::Completed {
        return Ok(None);
    }
    let run_id = &run.run_id;
    let terminal_output = representation::graph::completion::terminal_in(db, run_id)
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
    db: &dyn switchy_database::DatabaseTransaction,
    run_id: &str,
    expected_revision: u64,
    bound: u16,
) -> Result<(Vec<WorkflowActivationSummary>, bool), ServerError> {
    let scheduling = super::backpressure::retry(|| {
        representation::scheduling::snapshot_in(db, run_id, Some(expected_revision), None, bound)
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
    db: &dyn switchy_database::DatabaseTransaction,
    run_id: &str,
    bound: u16,
) -> Result<Vec<WorkflowOutputSummary>, ServerError> {
    let outputs = representation::graph::completion::outputs_in(db, run_id, None, bound)
        .await
        .map_err(|_| stage("outputs"))?
        .into_iter()
        .map(|output| {
            let checksum_sha256 = bcode_workflow::workflow_canonical_value_sha256(&output.value)
                .map_err(|_| stage("output_checksum"))?;
            Ok(WorkflowOutputSummary {
                output_id: output.output_id,
                run_id: output.run_id,
                node_id: output.node_id,
                activation_id: output.activation_id,
                schema_id: output.schema_id,
                schema_version: output.schema_version,
                checksum_sha256,
                artifact_reference: output.artifact_reference,
                created_at_ms: output.created_at_ms,
            })
        })
        .collect::<Result<Vec<_>, ServerError>>()?;
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
    fn attempt_settlement_requires_terminal_facts_not_just_topology() {
        let mut attempt: bcode_workflow::AttemptSummary =
            serde_json::from_value(serde_json::json!({
                "run_id":"run", "node_id":"worker", "activation_id":"activation", "attempt":1,
                "dispatch_identity":"dispatch", "side_effect":"read_only", "status":"admitted",
                "has_receipt":true, "prepared_at_ms":1, "admitted_at_ms":2, "terminal_at_ms":null
            }))
            .unwrap();
        assert!(!super::attempts_settled(&[attempt.clone()], 2));
        attempt.terminal_at_ms = Some(3);
        assert!(!super::attempts_settled(&[attempt.clone()], 2));
        for status in ["succeeded", "failed", "cancelled"] {
            attempt.status = status.into();
            assert!(super::attempts_settled(&[attempt.clone()], 2));
            assert!(!super::attempts_settled(&[attempt.clone()], 1));
        }
        attempt.status = "future_terminal".into();
        assert!(!super::attempts_settled(&[attempt], 2));
    }

    #[test]
    fn aggregate_topology_requires_the_pinned_revision() {
        assert!(verify_revision(7, 7).is_ok());
        assert!(verify_revision(7, 8).is_err());
        assert!(verify_revision(8, 7).is_err());
    }
}
