use super::{
    Database, DatabaseValue, DirectContributorObservation as Observation,
    DirectContributorReference, Error, WorkflowExecutionSessionLink, current_activation, decode,
    text,
};
use switchy_database::query::{FilterableQuery, SortDirection};

pub(super) async fn prove(
    db: &dyn Database,
    caller: &WorkflowExecutionSessionLink,
    revision: u64,
) -> Result<Observation, Error> {
    let edges = super::super::graph_edges::edges(
        db,
        &caller.run_id,
        revision,
        &caller.node_id,
        super::super::graph_edges::EdgeDirection::Incoming,
        None,
        2,
    )
    .await?;
    let [record] = edges.as_slice() else {
        return Ok(Observation::ProvenanceUnavailable);
    };
    let edge = &record.edge;
    if edge.to != caller.node_id || edge.from == caller.node_id {
        return Err(Error::MaintenanceRequired);
    }
    if !supported_edge(db, caller, revision, edge).await? {
        return Ok(Observation::Unsupported);
    }
    let source_node_id = edge.from.as_str();
    let Some(output) = admitted_output(db, caller, revision, source_node_id).await? else {
        return Ok(Observation::ProvenanceUnavailable);
    };
    let Some((dispatch_identity, execution)) =
        successful_execution(db, caller, source_node_id, &output.activation_id).await?
    else {
        return Ok(Observation::ProvenanceUnavailable);
    };
    Ok(Observation::Available(Box::new(
        DirectContributorReference {
            run_id: caller.run_id.clone(),
            consumer_activation_id: caller.activation_id.clone(),
            graph_revision: revision,
            edge_ordinal: record.ordinal,
            output_id: output.output_id,
            output_checksum_sha256: bcode_workflow::workflow_canonical_value_sha256(&output.value)
                .map_err(|_| Error::MaintenanceRequired)?,
            dispatch_identity,
            execution,
        },
    )))
}

async fn supported_edge(
    db: &dyn Database,
    caller: &WorkflowExecutionSessionLink,
    revision: u64,
    edge: &bcode_workflow::EdgeDefinition,
) -> Result<bool, Error> {
    if !matches!(edge.kind, bcode_workflow::EdgeKind::Direct) || edge.transform.is_some() {
        return Ok(false);
    }
    for node in [&edge.from, &edge.to] {
        let definition = super::super::graph_pages::node(db, &caller.run_id, revision, node)
            .await?
            .ok_or(Error::MaintenanceRequired)?;
        if !matches!(
            definition.kind,
            bcode_workflow::NodeKind::Agent | bcode_workflow::NodeKind::Task
        ) {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn admitted_output(
    db: &dyn Database,
    caller: &WorkflowExecutionSessionLink,
    revision: u64,
    source_node_id: &str,
) -> Result<Option<crate::ValidatedOutput>, Error> {
    let Some(target) = current_activation(db, &caller.run_id, &caller.node_id).await? else {
        return Ok(None);
    };
    let Some(source) = current_activation(db, &caller.run_id, source_node_id).await? else {
        return Ok(None);
    };
    if text(&target, "identity")? != caller.activation_id
        || !admitted(&target, revision)
        || !admitted(&source, revision)
    {
        return Ok(None);
    }
    let activation = text(&source, "identity")?;
    let outputs = db
        .select("workflow_outputs_v2")
        .where_eq("run_id", caller.run_id.as_str())
        .where_eq("activation_id", activation.as_str())
        .limit(2)
        .execute(db)
        .await?;
    let [row] = outputs.as_slice() else {
        return Ok(None);
    };
    let output: crate::ValidatedOutput = decode(row, "output_json")?;
    crate::validate_output(&output).map_err(|_| Error::MaintenanceRequired)?;
    if output.run_id != caller.run_id
        || output.node_id != source_node_id
        || output.activation_id != activation
        || text(row, "output_id")? != output.output_id
    {
        return Err(Error::MaintenanceRequired);
    }
    // Identity is proved by admitted topology and exact current source, not value search.
    // Equality is only a consistency check on that already-selected direct edge.
    let source_value: serde_json::Value = decode(&source, "output_json")?;
    let target_input: serde_json::Value = decode(&target, "input_json")?;
    if source_value != output.value || target_input != output.value {
        return Err(Error::MaintenanceRequired);
    }
    Ok(Some(output))
}

pub(super) async fn successful_execution(
    db: &dyn Database,
    caller: &WorkflowExecutionSessionLink,
    source_node_id: &str,
    activation: &str,
) -> Result<Option<(String, WorkflowExecutionSessionLink)>, Error> {
    let attempts = db
        .select("workflow_attempts_v2")
        .where_eq("run_id", caller.run_id.as_str())
        .where_eq("node_id", source_node_id)
        .where_eq("activation_id", activation)
        .sort("attempt_number", SortDirection::Desc)
        .limit(1)
        .execute(db)
        .await?;
    let Some(attempt) = attempts.first() else {
        return Ok(None);
    };
    if attempt.get("outcome_json") == Some(DatabaseValue::Null)
        || attempt.get("receipt_json") == Some(DatabaseValue::Null)
    {
        return Ok(None);
    }
    let outcome: super::super::attempts::TerminalOutcome = decode(attempt, "outcome_json")?;
    if outcome != super::super::attempts::TerminalOutcome::Succeeded {
        return Ok(None);
    }
    let prepared: crate::PreparedAttempt = decode(attempt, "prepared_json")?;
    let receipt: crate::DispatchReceipt = decode(attempt, "receipt_json")?;
    let dispatch = prepared.dispatch_identity();
    if prepared.run_id != caller.run_id
        || prepared.node_id != source_node_id
        || prepared.activation_id != activation
        || prepared.attempt == 0
        || attempt.get("attempt_number") != Some(DatabaseValue::Int64(i64::from(prepared.attempt)))
        || text(attempt, "dispatch_identity")? != dispatch
        || receipt.run_id != prepared.run_id
        || receipt.node_id != prepared.node_id
        || receipt.activation_id != activation
        || receipt.attempt != prepared.attempt
        || receipt.dispatch_identity != dispatch
        || !matches!(attempt.get("terminal_at_ms"), Some(DatabaseValue::Int64(n)) if n >= 0)
    {
        return Err(Error::MaintenanceRequired);
    }
    let links = db
        .select("workflow_execution_sessions_v2")
        .where_eq("dispatch_identity", dispatch.as_str())
        .limit(1)
        .execute(db)
        .await?;
    let Some(link) = links.first() else {
        return Ok(None);
    };
    let execution: WorkflowExecutionSessionLink = decode(link, "link_json")?;
    crate::validate_execution_session_link(&execution).map_err(|_| Error::MaintenanceRequired)?;
    if execution.run_id != caller.run_id
        || execution.node_id != source_node_id
        || execution.activation_id != activation
        || execution.attempt != prepared.attempt
        || execution.workspace_snapshot != caller.workspace_snapshot
    {
        return Err(Error::MaintenanceRequired);
    }
    Ok(Some((dispatch, execution)))
}

fn admitted(row: &switchy_database::Row, revision: u64) -> bool {
    i64::try_from(revision).is_ok_and(|n| row.get("revision") == Some(DatabaseValue::Int64(n)))
        && row.get("skipped") == Some(DatabaseValue::Int64(0))
        && matches!(row.get("generation"), Some(DatabaseValue::Int64(n)) if n > 0)
}
