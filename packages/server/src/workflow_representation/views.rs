//! Bounded graph-node inspection through typed store read APIs; never execution authority.
use super::{ServerError, ServerState, open, representation, retained_store, unavailable};
use bcode_workflow_view_models::{WorkflowNodeStatus, WorkflowNodeView, WorkflowRunView};
use switchy_database::Database;

/// Add a canonical revision-pinned node prefix for ordinary node selection.
/// # Errors
/// Rejects malformed limits, unavailable graph or invalid stored definitions; does not repair.
pub async fn populate_nodes(
    state: &ServerState,
    run_id: &str,
    limit: usize,
    view: &mut WorkflowRunView,
) -> Result<(), ServerError> {
    let limit = u16::try_from(limit)
        .ok()
        .filter(|limit| (1..=100).contains(limit))
        .ok_or_else(unavailable)?;
    let retained = retained_store(state)?;
    let db = open(retained.path(), false)?;
    let tx = db.begin_transaction().await.map_err(|_| unavailable())?;
    let revision = representation::graph_pages::revision(&*tx, run_id)
        .await
        .map_err(|_| unavailable())?;
    let nodes = representation::graph_pages::nodes(&*tx, run_id, revision, None, limit)
        .await
        .map_err(|_| unavailable())?;
    let mut projected = Vec::with_capacity(nodes.len());
    for node in nodes {
        let activation_id = representation::graph::activation_id(&*tx, run_id, &node.id)
            .await
            .map_err(|_| unavailable())?;
        projected.push(WorkflowNodeView {
            node_id: node.id,
            name: node.name,
            kind: bcode_workflow_view::project_node_kind(node.kind),
            activation_id,
            status: WorkflowNodeStatus::Unknown("activation lifecycle not observed".into()),
        });
    }
    tx.commit().await.map_err(|_| unavailable())?;
    view.nodes = projected;
    view.health = bcode_workflow_view_models::WorkflowProjectionHealth::Degraded {
        reason: format!(
            "Canonical graph revision {revision}: bounded node and output prefixes. Node lifecycle, edges, progress, ancestry and controls unavailable; empty sections are not proof of absence. Continuation not exposed by this view."
        ),
    };
    Ok(())
}
