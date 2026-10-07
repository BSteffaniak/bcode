//! Exact first-publication lineage; later generations require additional admission provenance.
use super::{Database, DatabaseValue, Error, WorkflowExecutionSessionLink, decode, text};
use bcode_workflow::{EdgeDefinition, WorkflowDefinition, WorkflowRunGraphEditBatch};
use std::collections::{BTreeMap, BTreeSet};
use switchy_database::query::FilterableQuery;
#[cfg(test)]
mod tests;
mod topology;
use super::{GeneratedContributorObservation as Observation, GeneratedContributorPage};

struct Publication {
    batch: WorkflowRunGraphEditBatch,
    edges: BTreeMap<u64, EdgeDefinition>,
    definition: WorkflowDefinition,
    source: WorkflowExecutionSessionLink,
}

pub(super) async fn prove(
    db: &dyn Database,
    caller: &WorkflowExecutionSessionLink,
    after: Option<&str>,
    limit: u16,
) -> Result<Observation, Error> {
    let Some(publication) = publication(db, caller).await? else {
        return Ok(Observation::ProvenanceUnavailable);
    };
    let Some(scope) = topology::scope(&publication, caller)? else {
        return Ok(Observation::Unsupported);
    };
    let initial = super::super::graph_edges::edges(
        db,
        &caller.run_id,
        1,
        &caller.node_id,
        super::super::graph_edges::EdgeDirection::Incoming,
        None,
        2,
    )
    .await?;
    let [original] = initial.as_slice() else {
        return Ok(Observation::ProvenanceUnavailable);
    };
    if original.edge.from != publication.source.node_id
        || !matches!(original.edge.kind, bcode_workflow::EdgeKind::Direct)
        || original.edge.transform.is_some()
        || !publication.batch.edits.iter().any(|edit| {
            matches!(edit,
            bcode_workflow::WorkflowRunGraphEdit::ReplaceEdge { edge_id, edge }
                if *edge_id == original.ordinal && edge.to == caller.node_id)
        })
    {
        return Ok(Observation::Unsupported);
    }
    for node_id in &scope {
        if node_id != &publication.source.node_id
            && super::super::graph_pages::node(db, &caller.run_id, 1, node_id)
                .await?
                .is_some()
        {
            return Ok(Observation::ProvenanceUnavailable);
        }
    }
    if after.is_some_and(|cursor| !scope.contains(cursor)) {
        return Err(Error::InvalidAdmission);
    }
    // Verify the entire finite scope before returning any member. Large scopes stay explicit.
    let Some(contributors) = verified_members(db, caller, &publication, &scope).await? else {
        return Ok(Observation::ProvenanceUnavailable);
    };
    let mut selected = contributors.into_iter().filter(|reference| {
        after.is_none_or(|cursor| reference.execution.node_id.as_str() > cursor)
    });
    let page: Vec<_> = selected.by_ref().take(usize::from(limit)).collect();
    let next_node_id = if selected.next().is_some() {
        page.last()
            .map(|reference| reference.execution.node_id.clone())
    } else {
        None
    };
    Ok(Observation::Available(Box::new(GeneratedContributorPage {
        mutation_id: publication.batch.mutation_id,
        source_activation_id: publication.source.activation_id,
        contributors: page,
        next_node_id,
    })))
}

async fn publication(
    db: &dyn Database,
    caller: &WorkflowExecutionSessionLink,
) -> Result<Option<Publication>, Error> {
    let receipts = db
        .select("workflow_graph_revisions_v2")
        .where_eq("run_id", caller.run_id.as_str())
        .where_eq("revision", 2_i64)
        .limit(2)
        .execute(db)
        .await?;
    let [receipt] = receipts.as_slice() else {
        return Ok(None);
    };
    let batch: WorkflowRunGraphEditBatch = decode(receipt, "request_json")?;
    batch.validate().map_err(|_| Error::MaintenanceRequired)?;
    if batch.run_id != caller.run_id || batch.expected_revision != 1 {
        return Err(Error::MaintenanceRequired);
    }
    let identity = serde_json::to_string(&(&batch.run_id, &batch.mutation_id))
        .map_err(|_| Error::MaintenanceRequired)?;
    if text(receipt, "identity")? != identity {
        return Err(Error::MaintenanceRequired);
    }
    let candidates = db
        .select("workflow_execution_graph_candidates_v2")
        .where_eq("identity", identity)
        .limit(1)
        .execute(db)
        .await?;
    let Some(candidate) = candidates.first() else {
        return Ok(None);
    };
    let candidate_batch: WorkflowRunGraphEditBatch = decode(candidate, "request_json")?;
    if candidate_batch != batch {
        return Err(Error::MaintenanceRequired);
    }
    let source: WorkflowExecutionSessionLink = decode(candidate, "link_json")?;
    crate::validate_execution_session_link(&source).map_err(|_| Error::MaintenanceRequired)?;
    if source.run_id != caller.run_id || source.workspace_snapshot != caller.workspace_snapshot {
        return Err(Error::MaintenanceRequired);
    }
    let edges = decode(receipt, "edges_json")?;
    let definition = decode(receipt, "definition_json")?;
    Ok(Some(Publication {
        batch,
        edges,
        definition,
        source,
    }))
}

async fn verified_members(
    db: &dyn Database,
    caller: &WorkflowExecutionSessionLink,
    publication: &Publication,
    scope: &BTreeSet<String>,
) -> Result<Option<Vec<super::DirectContributorReference>>, Error> {
    let Some(target) = super::current_activation(db, &caller.run_id, &caller.node_id).await? else {
        return Ok(None);
    };
    if !first(&target, 2) || text(&target, "identity")? != caller.activation_id {
        return Ok(None);
    }
    let mut references = Vec::new();
    for node_id in scope {
        let Some(activation) = super::current_activation(db, &caller.run_id, node_id).await? else {
            return Ok(None);
        };
        let is_source = node_id == &publication.source.node_id;
        if !first(&activation, if is_source { 1 } else { 2 }) {
            return Ok(None);
        }
        let activation_id = text(&activation, "identity")?;
        if is_source && activation_id != publication.source.activation_id {
            return Ok(None);
        }
        let definition = &publication.definition.nodes[node_id];
        if definition.kind == bcode_workflow::NodeKind::Parallel {
            if activation.get("output_json") == Some(DatabaseValue::Null) {
                return Ok(None);
            }
            continue;
        }
        let outputs = db
            .select("workflow_outputs_v2")
            .where_eq("run_id", caller.run_id.as_str())
            .where_eq("activation_id", activation_id.as_str())
            .limit(2)
            .execute(db)
            .await?;
        let [row] = outputs.as_slice() else {
            return Ok(None);
        };
        let output: crate::ValidatedOutput = decode(row, "output_json")?;
        crate::validate_output(&output).map_err(|_| Error::MaintenanceRequired)?;
        if output.run_id != caller.run_id
            || output.node_id != *node_id
            || output.activation_id != activation_id
            || text(row, "output_id")? != output.output_id
        {
            return Err(Error::MaintenanceRequired);
        }
        let activation_output: serde_json::Value = decode(&activation, "output_json")?;
        if activation_output != output.value {
            return Err(Error::MaintenanceRequired);
        }
        let Some((dispatch_identity, execution)) =
            super::proof::successful_execution(db, caller, node_id, &activation_id).await?
        else {
            return Ok(None);
        };
        if is_source && execution != publication.source {
            return Ok(None);
        }
        references.push(super::DirectContributorReference {
            run_id: caller.run_id.clone(),
            consumer_activation_id: caller.activation_id.clone(),
            graph_revision: 2,
            edge_ordinal: publication
                .edges
                .iter()
                .find(|(_, edge)| edge.from == *node_id)
                .map_or(0, |(id, _)| *id),
            output_id: output.output_id,
            output_checksum_sha256: bcode_workflow::workflow_canonical_value_sha256(&output.value)
                .map_err(|_| Error::MaintenanceRequired)?,
            dispatch_identity,
            execution,
        });
    }
    Ok(Some(references))
}

fn first(row: &switchy_database::Row, revision: i64) -> bool {
    row.get("revision") == Some(DatabaseValue::Int64(revision))
        && row.get("generation") == Some(DatabaseValue::Int64(1))
        && row.get("skipped") == Some(DatabaseValue::Int64(0))
}
