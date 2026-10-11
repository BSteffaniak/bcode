//! Public revision-pinned graph inspection for either canonical run representation.
use super::backpressure::{normalize, retry};
use super::{
    RunLookup, ServerError, ServerState, lookup_at, open, representation, retained_store,
    unavailable, unsupported,
};
use bcode_workflow::{
    WorkflowRunGraphEdgeInspection, WorkflowRunGraphInspection, WorkflowRunGraphNodeInspection,
    WorkflowRunGraphPageRequest,
};
use switchy_database::Database;

pub async fn page(
    state: &ServerState,
    request: &WorkflowRunGraphPageRequest,
) -> Result<WorkflowRunGraphInspection, ServerError> {
    let retained = retained_store(state).await?;
    match lookup_at(retained.path(), &request.run_id).await? {
        RunLookup::Legacy => {
            return crate::workflow_operations::inspect_graph_page(state, request)
                .map_err(Into::into);
        }
        RunLookup::Available(_) => {}
        RunLookup::UnsupportedRepresentation => return Err(unsupported()),
        RunLookup::Missing | RunLookup::Degraded => return Err(unavailable()),
    }
    let db = open(retained.path(), false)?;
    read(&db, request).await
}

async fn read(
    db: &dyn Database,
    request: &WorkflowRunGraphPageRequest,
) -> Result<WorkflowRunGraphInspection, ServerError> {
    let limit = u16::try_from(request.limit)
        .ok()
        .filter(|limit| (1..=100).contains(limit))
        .ok_or_else(unavailable)?;
    let tx = retry(|| async {
        db.begin_transaction()
            .await
            .map_err(representation::RepresentationError::from)
    })
    .await
    .map_err(normalize)?;
    let result = read_in(&*tx, request, limit).await?;
    tx.commit().await.map_err(|_| unavailable())?;
    Ok(result)
}

/// Read graph facts inside the caller's canonical read transaction.
pub(super) async fn read_in(
    tx: &dyn Database,
    request: &WorkflowRunGraphPageRequest,
    limit: u16,
) -> Result<WorkflowRunGraphInspection, ServerError> {
    let revision = retry(|| representation::graph_pages::revision(tx, &request.run_id))
        .await
        .map_err(normalize)?;
    if revision != request.expected_revision {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "workflow graph revision conflict".into(),
        )
        .into());
    }
    let boundary = retry(|| representation::graph_edges::boundary(tx, &request.run_id, revision))
        .await
        .map_err(normalize)?;
    let nodes = retry(|| {
        representation::graph_pages::nodes(
            tx,
            &request.run_id,
            revision,
            request.after_node_id.as_deref(),
            limit,
        )
    })
    .await
    .map_err(normalize)?;
    let edges = retry(|| {
        representation::graph_edges::page(
            tx,
            &request.run_id,
            revision,
            None,
            request.after_edge_id,
            limit,
        )
    })
    .await
    .map_err(normalize)?;
    let nodes_complete = match nodes.last() {
        Some(node) => retry(|| {
            representation::graph_pages::nodes(tx, &request.run_id, revision, Some(&node.id), 1)
        })
        .await
        .map_err(normalize)?
        .is_empty(),
        None => true,
    };
    let edges_complete = match edges.last() {
        Some(edge) => retry(|| {
            representation::graph_edges::page(
                tx,
                &request.run_id,
                revision,
                None,
                Some(edge.ordinal),
                1,
            )
        })
        .await
        .map_err(normalize)?
        .is_empty(),
        None => true,
    };
    let next_edge_id = retry(|| representation::graph_edges::next_edge_id(tx, &request.run_id))
        .await
        .map_err(normalize)?;
    let result = graph_view(
        revision,
        next_edge_id,
        nodes_complete,
        edges_complete,
        nodes,
        edges,
        &boundary,
    );
    Ok(result)
}

fn graph_view(
    revision: u64,
    next_edge_id: Option<u64>,
    nodes_complete: bool,
    edges_complete: bool,
    nodes: Vec<bcode_workflow::NodeDefinition>,
    edges: Vec<representation::graph_edges::Edge>,
    boundary: &representation::graph_edges::Boundary,
) -> WorkflowRunGraphInspection {
    WorkflowRunGraphInspection {
        revision,
        next_edge_id,
        nodes_complete,
        edges_complete,
        nodes: nodes
            .into_iter()
            .map(|node| WorkflowRunGraphNodeInspection {
                revision,
                entry: boundary.entries.contains(&node.id),
                exit: boundary.exits.contains(&node.id),
                node,
            })
            .collect(),
        edges: edges
            .into_iter()
            .map(|edge| WorkflowRunGraphEdgeInspection {
                revision,
                edge_id: edge.ordinal,
                edge: edge.edge,
            })
            .collect(),
    }
}
