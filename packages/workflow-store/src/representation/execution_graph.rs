//! Receipt-backed execution graph staging; staging never publishes or dispatches.
use super::{Database, DatabaseValue, RepresentationError as Error, TransactionMode};
use crate::{WorkflowExecutionAuthority, WorkflowExecutionSessionLink};
use bcode_workflow::{WorkflowRunGraphEditBatch, WorkflowRunGraphReconciliation};
use switchy_database::{
    DatabaseTransaction,
    query::FilterableQuery,
    schema::{DataType, create_table},
};
pub(crate) mod cancellation;
pub use cancellation::accept_publication_from_execution;
const TABLE: &str = "workflow_execution_graph_candidates_v2";
#[cfg(test)]
mod tests;
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    cancellation::install(db).await?;
    create_table(TABLE)
        .column(super::column("identity", DataType::Text, false))
        .primary_key("identity")
        .column(super::column("request_json", DataType::Text, false))
        .column(super::column("link_json", DataType::Text, false))
        .column(super::column("created_at_ms", DataType::Int, false))
        .execute(db)
        .await?;
    Ok(())
}
fn encode<T: serde::Serialize>(value: &T) -> Result<String, Error> {
    let json = serde_json::to_string(value).map_err(|_| Error::InvalidAdmission)?;
    if json.len() > super::MAX_ADMISSION_BYTES {
        return Err(Error::InvalidAdmission);
    }
    Ok(json)
}
/// Verify current ownership and the exact active receipt-backed execution link in one snapshot.
/// # Errors
/// Rejects foreign/stale owners, replaced session links, terminal attempts or malformed state.
pub async fn verify_execution(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    link: &WorkflowExecutionSessionLink,
) -> Result<(), Error> {
    crate::validate_execution_session_link(link).map_err(|_| Error::InvalidAdmission)?;
    super::verified(db).await?;
    let rows = db
        .select(super::RUNS)
        .columns(&["authority_json", "status"])
        .where_eq("run_id", link.run_id.as_str())
        .limit(1)
        .execute(db)
        .await?;
    let root = rows.first().ok_or(Error::InvalidAdmission)?;
    if root.get("authority_json") != Some(DatabaseValue::String(encode(owner)?)) {
        return Err(Error::AuthorityConflict);
    }
    if root.get("status") != Some(DatabaseValue::String("paused".into())) {
        return Err(Error::InvalidAdmission);
    }
    let id = crate::dispatch_identity(
        &link.run_id,
        &link.node_id,
        &link.activation_id,
        link.attempt,
    );
    let links = db
        .select("workflow_execution_sessions_v2")
        .columns(&["link_json"])
        .where_eq("dispatch_identity", id.as_str())
        .limit(1)
        .execute(db)
        .await?;
    if links.first().and_then(|r| r.get("link_json")) != Some(DatabaseValue::String(encode(link)?))
    {
        return Err(Error::AuthorityConflict);
    }
    let attempts = db
        .select("workflow_attempts_v2")
        .columns(&["receipt_json", "outcome_json"])
        .where_eq("dispatch_identity", id)
        .limit(1)
        .execute(db)
        .await?;
    let attempt = attempts.first().ok_or(Error::InvalidAdmission)?;
    if !matches!(attempt.get("receipt_json"), Some(DatabaseValue::String(_)))
        || attempt.get("outcome_json") != Some(DatabaseValue::Null)
    {
        return Err(Error::InvalidAdmission);
    }
    if !super::graph::is_ready_activation(db, &link.run_id, &link.node_id, &link.activation_id)
        .await?
    {
        return Err(Error::InvalidAdmission);
    }
    Ok(())
}
fn validate(
    batch: &WorkflowRunGraphEditBatch,
    link: &WorkflowExecutionSessionLink,
) -> Result<(), Error> {
    batch.validate().map_err(|_| Error::InvalidAdmission)?;
    if batch.run_id != link.run_id {
        return Err(Error::InvalidAdmission);
    }
    Ok(())
}
/// Stage an immutable exact candidate for the authenticated active execution; never publish it.
/// # Errors
/// Rejects stale revision/owner/link, unsupported reconciliation or conflicting mutation reuse.
pub async fn stage_run_graph_edit_from_execution(
    db: &dyn Database,
    batch: &WorkflowRunGraphEditBatch,
    owner: &WorkflowExecutionAuthority,
    link: &WorkflowExecutionSessionLink,
    now: u64,
) -> Result<bool, Error> {
    bcode_workflow::reject_execution_parent_claims(batch).map_err(|_| Error::InvalidAdmission)?;
    validate(batch, link)?;
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    verify_execution(&*tx, owner, link).await?;
    if super::graph_pages::revision(&*tx, &batch.run_id).await? != batch.expected_revision {
        return Err(Error::InvalidAdmission);
    }
    let id = encode(&(&batch.run_id, &batch.mutation_id))?;
    let request = encode(batch)?;
    let caller = encode(link)?;
    let rows = tx
        .select(TABLE)
        .where_eq("identity", id.as_str())
        .limit(1)
        .execute(&*tx)
        .await?;
    let inserted = if let Some(row) = rows.first() {
        if row.get("request_json") != Some(DatabaseValue::String(request))
            || row.get("link_json") != Some(DatabaseValue::String(caller))
        {
            return Err(Error::InvalidAdmission);
        }
        false
    } else {
        tx.insert(TABLE)
            .value("identity", id)
            .value("request_json", request)
            .value("link_json", caller)
            .value(
                "created_at_ms",
                i64::try_from(now).map_err(|_| Error::InvalidAdmission)?,
            )
            .execute(&*tx)
            .await?;
        true
    };
    tx.commit().await?;
    Ok(inserted)
}
/// Publish only the exact staged candidate under fresh execution fencing in the same transaction.
///
/// Application publication authorization must precede this call. Cancellation reconciliation
/// remains unsupported and cannot be silently rewritten into retain.
/// # Errors
/// Rejects unstaged/changed candidates, stale execution facts, revision conflicts or graph failure.
pub async fn publication_from_execution(
    db: &dyn Database,
    batch: &WorkflowRunGraphEditBatch,
    owner: &WorkflowExecutionAuthority,
    link: &WorkflowExecutionSessionLink,
) -> Result<u64, Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    let revision = publication_in(&*tx, batch, owner, link).await?;
    tx.commit().await?;
    Ok(revision)
}
async fn publication_in(
    tx: &dyn DatabaseTransaction,
    batch: &WorkflowRunGraphEditBatch,
    owner: &WorkflowExecutionAuthority,
    link: &WorkflowExecutionSessionLink,
) -> Result<u64, Error> {
    validate(batch, link)?;
    verify_execution(tx, owner, link).await?;
    let rows = tx
        .select(TABLE)
        .where_eq("identity", encode(&(&batch.run_id, &batch.mutation_id))?)
        .limit(1)
        .execute(tx)
        .await?;
    let row = rows.first().ok_or(Error::InvalidAdmission)?;
    if row.get("request_json") != Some(DatabaseValue::String(encode(batch)?))
        || row.get("link_json") != Some(DatabaseValue::String(encode(link)?))
    {
        return Err(Error::InvalidAdmission);
    }
    super::graph_revision::publish_with_parent_in(
        tx,
        owner,
        batch,
        super::GraphAdmissionPolicy::default(),
        Some(&link.session_id),
    )
    .await
}

/// Inspect an authenticated active execution's independent bounded node/edge/output pages.
/// # Errors
/// Rejects stale execution, unqualified cursors, bad bounds, revision mismatch or invalid outputs.
pub async fn execution_context_graph_page(
    db: &dyn Database,
    link: &WorkflowExecutionSessionLink,
    owner: &WorkflowExecutionAuthority,
    request: &bcode_workflow::WorkflowExecutionContextRequest,
) -> Result<
    (
        crate::RunGraphPage,
        Option<bcode_workflow::WorkflowOutputInspection>,
        Vec<crate::WorkflowOutputSummary>,
    ),
    Error,
> {
    if !(1..=100).contains(&request.limit)
        || ((request.after_node_id.is_some() || request.after_edge_id.is_some())
            && request.expected_revision.is_none())
        || (request.source_local && request.after_node_id.is_some())
    {
        return Err(Error::InvalidAdmission);
    }
    let tx = db.begin_transaction().await?;
    verify_execution(&*tx, owner, link).await?;
    let revision = super::graph_pages::revision(&*tx, &link.run_id).await?;
    if request
        .expected_revision
        .is_some_and(|expected| expected != revision)
    {
        return Err(Error::InvalidAdmission);
    }
    let boundary = super::graph_edges::boundary(&*tx, &link.run_id, revision).await?;
    let limit = u16::try_from(request.limit).map_err(|_| Error::InvalidAdmission)?;
    let nodes = if request.source_local {
        vec![
            super::graph_pages::node(&*tx, &link.run_id, revision, &link.node_id)
                .await?
                .ok_or(Error::MaintenanceRequired)?,
        ]
    } else {
        super::graph_pages::nodes(
            &*tx,
            &link.run_id,
            revision,
            request.after_node_id.as_deref(),
            limit,
        )
        .await?
    };
    let nodes_complete = if request.source_local {
        true
    } else if let Some(last) = nodes.last() {
        super::graph_pages::nodes(&*tx, &link.run_id, revision, Some(&last.id), 1)
            .await?
            .is_empty()
    } else {
        true
    };
    let source = request.source_local.then_some(link.node_id.as_str());
    let edges = super::graph_edges::page(
        &*tx,
        &link.run_id,
        revision,
        source,
        request.after_edge_id,
        limit,
    )
    .await?;
    let edges_complete = if let Some(last) = edges.last() {
        super::graph_edges::page(&*tx, &link.run_id, revision, source, Some(last.ordinal), 1)
            .await?
            .is_empty()
    } else {
        true
    };
    let page = crate::RunGraphPage {
        revision,
        next_edge_id: super::graph_edges::next_edge_id(&*tx, &link.run_id).await?,
        nodes: nodes
            .into_iter()
            .map(|node| crate::RunGraphNode {
                revision,
                entry: boundary.entries.contains(&node.id),
                exit: boundary.exits.contains(&node.id),
                node,
            })
            .collect(),
        edges: edges
            .into_iter()
            .map(|edge| crate::RunGraphEdge {
                revision,
                edge_id: edge.ordinal,
                edge: edge.edge,
            })
            .collect(),
        nodes_complete,
        edges_complete,
    };
    let (selected, outputs) = output_page(&*tx, link, request).await?;
    tx.commit().await?;
    Ok((page, selected, outputs))
}
fn parse_output(row: &switchy_database::Row) -> Result<bcode_workflow::ValidatedOutput, Error> {
    match row.get("output_json") {
        Some(DatabaseValue::String(json)) if json.len() <= super::MAX_ADMISSION_BYTES => {
            let output: bcode_workflow::ValidatedOutput =
                serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)?;
            crate::validate_output(&output).map_err(|_| Error::MaintenanceRequired)?;
            if row.get("output_id") != Some(DatabaseValue::String(output.output_id.clone()))
                || row.get("run_id") != Some(DatabaseValue::String(output.run_id.clone()))
            {
                return Err(Error::MaintenanceRequired);
            }
            Ok(output)
        }
        _ => Err(Error::MaintenanceRequired),
    }
}
fn inspect(
    output: &bcode_workflow::ValidatedOutput,
) -> Result<bcode_workflow::WorkflowOutputInspection, Error> {
    Ok(bcode_workflow::WorkflowOutputInspection {
        version: bcode_workflow::WORKFLOW_OUTPUT_INSPECTION_VERSION,
        output_id: output.output_id.clone(),
        run_id: output.run_id.clone(),
        node_id: output.node_id.clone(),
        activation_id: output.activation_id.clone(),
        schema_id: output.schema_id.clone(),
        schema_version: output.schema_version,
        checksum_sha256: bcode_workflow::workflow_canonical_value_sha256(&output.value)
            .map_err(|_| Error::MaintenanceRequired)?,
        value: output.value.clone(),
        artifact_reference: output.artifact_reference.clone(),
        created_at_ms: output.created_at_ms,
    })
}

/// Read an exact retained candidate under current durable run authority.
/// # Errors
/// Rejects foreign authority, malformed candidate or unavailable storage.
pub async fn staged(
    db: &dyn Database,
    run: &str,
    mutation: &str,
    owner: &WorkflowExecutionAuthority,
) -> Result<Option<WorkflowRunGraphEditBatch>, Error> {
    let tx = db.begin_transaction().await?;
    super::verified(&*tx).await?;
    let roots = tx
        .select(super::RUNS)
        .columns(&["authority_json"])
        .where_eq("run_id", run)
        .limit(1)
        .execute(&*tx)
        .await?;
    if roots.first().and_then(|row| row.get("authority_json"))
        != Some(DatabaseValue::String(encode(owner)?))
    {
        return Err(Error::AuthorityConflict);
    }
    let rows = tx
        .select(TABLE)
        .columns(&["request_json"])
        .where_eq("identity", encode(&(run, mutation))?)
        .limit(1)
        .execute(&*tx)
        .await?;
    let result = match rows.first() {
        None => None,
        Some(row) => {
            let Some(DatabaseValue::String(json)) = row.get("request_json") else {
                return Err(Error::MaintenanceRequired);
            };
            if json.len() > super::MAX_ADMISSION_BYTES {
                return Err(Error::MaintenanceRequired);
            }
            Some(serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)?)
        }
    };
    tx.commit().await?;
    Ok(result)
}

async fn output_page(
    db: &dyn Database,
    link: &WorkflowExecutionSessionLink,
    request: &bcode_workflow::WorkflowExecutionContextRequest,
) -> Result<
    (
        Option<bcode_workflow::WorkflowOutputInspection>,
        Vec<crate::WorkflowOutputSummary>,
    ),
    Error,
> {
    let selected = if let Some(id) = &request.output_id {
        let rows = db
            .select("workflow_outputs_v2")
            .where_eq("run_id", link.run_id.as_str())
            .where_eq("output_id", id.as_str())
            .limit(1)
            .execute(db)
            .await?;
        Some(inspect(&parse_output(
            rows.first().ok_or(Error::InvalidAdmission)?,
        )?)?)
    } else {
        None
    };
    let mut query = db
        .select("workflow_outputs_v2")
        .where_eq("run_id", link.run_id.as_str())
        .sort("output_id", switchy_database::query::SortDirection::Asc)
        .limit(request.limit);
    if let Some(after) = &request.after_output_id {
        query = query.where_gt("output_id", after.as_str());
    }
    let mut outputs = Vec::new();
    for row in query.execute(db).await? {
        let output = parse_output(&row)?;
        outputs.push(crate::WorkflowOutputSummary {
            checksum_sha256: bcode_workflow::workflow_canonical_value_sha256(&output.value)
                .map_err(|_| Error::MaintenanceRequired)?,
            output_id: output.output_id,
            run_id: output.run_id,
            node_id: output.node_id,
            activation_id: output.activation_id,
            schema_id: output.schema_id,
            schema_version: output.schema_version,
            artifact_reference: output.artifact_reference,
            created_at_ms: output.created_at_ms,
        });
    }
    Ok((selected, outputs))
}

/// Portable execution context from the same verified graph snapshot.
/// # Errors
/// Propagates execution-link, revision and canonical-output verification failures.
pub async fn context(
    db: &dyn Database,
    link: &WorkflowExecutionSessionLink,
    owner: &WorkflowExecutionAuthority,
    request: &bcode_workflow::WorkflowExecutionContextRequest,
) -> Result<bcode_workflow::WorkflowExecutionContext, Error> {
    let (page, output, outputs) = execution_context_graph_page(db, link, owner, request).await?;
    Ok(bcode_workflow::WorkflowExecutionContext {
        run_id: link.run_id.clone(),
        node_id: link.node_id.clone(),
        activation_id: link.activation_id.clone(),
        attempt: link.attempt,
        execution_allowance: None,
        output,
        outputs,
        graph: bcode_workflow::WorkflowRunGraphInspection {
            revision: page.revision,
            next_edge_id: page.next_edge_id,
            nodes_complete: page.nodes_complete,
            edges_complete: page.edges_complete,
            nodes: page
                .nodes
                .into_iter()
                .map(|node| bcode_workflow::WorkflowRunGraphNodeInspection {
                    revision: node.revision,
                    node: node.node,
                    entry: node.entry,
                    exit: node.exit,
                })
                .collect(),
            edges: page
                .edges
                .into_iter()
                .map(|edge| bcode_workflow::WorkflowRunGraphEdgeInspection {
                    revision: edge.revision,
                    edge_id: edge.edge_id,
                    edge: edge.edge,
                })
                .collect(),
        },
    })
}
