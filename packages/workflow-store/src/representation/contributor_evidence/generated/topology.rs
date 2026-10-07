use super::{BTreeMap, BTreeSet, Error, Publication, WorkflowExecutionSessionLink};
use bcode_workflow::{
    EdgeDefinition, EdgeKind, NodeKind, WorkflowRunGraphEdit as Edit,
    WorkflowRunGraphReconciliation as Reconcile, WorkflowTransformExpression,
};

/// Restrict support to a finite newly introduced dependency DAG with one source and sink.
pub(super) fn scope(
    publication: &Publication,
    caller: &WorkflowExecutionSessionLink,
) -> Result<Option<BTreeSet<String>>, Error> {
    let target_node_id = caller.node_id.as_str();
    let mut nodes = BTreeSet::new();
    let mut added_edges = BTreeMap::new();
    let mut reconnect = None;
    for edit in &publication.batch.edits {
        match edit {
            Edit::AddNode {
                node,
                entry: false,
                exit: false,
            } => {
                if !nodes.insert(node.id.clone()) {
                    return Err(Error::MaintenanceRequired);
                }
                let Some(committed) = publication.definition.nodes.get(&node.id) else {
                    return Err(Error::MaintenanceRequired);
                };
                if node.kind != committed.kind
                    || node.input != committed.input
                    || node.output != committed.output
                {
                    return Err(Error::MaintenanceRequired);
                }
            }
            Edit::AddEdge { edge_id, edge } => {
                added_edges.insert(*edge_id, edge.clone());
            }
            Edit::ReplaceEdge { edge_id, edge }
                if edge.to == caller.node_id && reconnect.is_none() =>
            {
                reconnect = Some((*edge_id, edge));
            }
            _ => return Ok(None),
        }
    }
    // A finite proof budget is explicit coverage, not an execution graph limit.
    if nodes.is_empty() || nodes.len() > 100 || added_edges.len() > 200 {
        return Ok(None);
    }
    let Some((reconnect_id, reconnect)) = reconnect else {
        return Ok(None);
    };
    if !nodes.contains(&reconnect.from)
        || !matches!(reconnect.kind, EdgeKind::Direct)
        || publication.edges.get(&reconnect_id) != Some(reconnect)
    {
        return Ok(None);
    }
    let Some(transform) = &reconnect.transform else {
        return Ok(None);
    };
    if transform.version != bcode_workflow::WORKFLOW_TRANSFORM_VERSION
        || !matches!(&transform.expression, WorkflowTransformExpression::Input { source, path }
            if source == bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT && path == "source")
    {
        return Ok(None);
    }
    let source_id = &publication.source.node_id;
    let Some(source_definition) = publication.definition.nodes.get(source_id) else {
        return Err(Error::MaintenanceRequired);
    };
    if transform.output != source_definition.output {
        return Ok(None);
    }
    let mut outgoing = BTreeSet::new();
    for (id, edge) in &publication.edges {
        if &edge.from == source_id {
            outgoing.insert(*id);
        }
    }
    if publication.batch.reconciliation.len() != 1
        || !matches!(&publication.batch.reconciliation[0], Reconcile::RetainWithBindings { activation_id, edge_ids }
            if activation_id == &publication.source.activation_id
                && edge_ids.iter().copied().collect::<BTreeSet<_>>() == outgoing)
    {
        return Ok(None);
    }
    let mut scope = nodes.clone();
    if !scope.insert(source_id.clone()) || scope.contains(&caller.node_id) {
        return Ok(None);
    }
    if !isolated_edges(
        publication,
        &nodes,
        &scope,
        &added_edges,
        reconnect_id,
        target_node_id,
    ) {
        return Ok(None);
    }
    if !dag(&scope, &added_edges, source_id, &reconnect.from) {
        return Ok(None);
    }
    if !supported_members(publication, &scope, &added_edges, source_id)? {
        return Ok(None);
    }
    Ok(Some(scope))
}

/// Reject uncommitted edges, sibling imports, and continuation bypasses.
fn isolated_edges(
    publication: &Publication,
    nodes: &BTreeSet<String>,
    scope: &BTreeSet<String>,
    added_edges: &BTreeMap<u64, EdgeDefinition>,
    reconnect_id: u64,
    target_node_id: &str,
) -> bool {
    for (id, edge) in added_edges {
        if publication.edges.get(id) != Some(edge)
            || !scope.contains(&edge.from)
            || !nodes.contains(&edge.to)
            || !matches!(edge.kind, EdgeKind::Direct)
        {
            return false;
        }
    }
    for (id, edge) in &publication.edges {
        if (nodes.contains(&edge.to) || scope.contains(&edge.from))
            && !added_edges.contains_key(id)
            && *id != reconnect_id
        {
            return false;
        }
        if edge.to == target_node_id && *id != reconnect_id {
            return false;
        }
    }
    true
}

/// Validate executable/join members and their declared transform dependencies.
fn supported_members(
    publication: &Publication,
    scope: &BTreeSet<String>,
    added_edges: &BTreeMap<u64, EdgeDefinition>,
    source_id: &str,
) -> Result<bool, Error> {
    for node_id in scope {
        let Some(node) = publication.definition.nodes.get(node_id) else {
            return Err(Error::MaintenanceRequired);
        };
        if !matches!(
            node.kind,
            NodeKind::Agent | NodeKind::Task | NodeKind::Parallel
        ) {
            return Ok(false);
        }
        let incoming: Vec<_> = added_edges
            .values()
            .filter(|edge| edge.to == *node_id)
            .collect();
        if node_id != source_id && node.kind != NodeKind::Parallel && incoming.len() != 1 {
            return Ok(false);
        }
        for edge in incoming {
            if let Some(transform) = &edge.transform {
                if transform.validate().is_err() {
                    return Err(Error::MaintenanceRequired);
                }
                for name in transform.referenced_sources() {
                    if let Some(dependency) = name
                        .strip_prefix(bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_DEPENDENCY_PREFIX)
                        && !added_edges.values().any(|candidate| {
                            candidate.from == dependency && candidate.to == *node_id
                        })
                    {
                        return Ok(false);
                    }
                }
            }
        }
        if node.kind == NodeKind::Parallel && !join(node_id, &node.configuration, added_edges) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn dag(
    scope: &BTreeSet<String>,
    edges: &BTreeMap<u64, EdgeDefinition>,
    source: &str,
    sink: &str,
) -> bool {
    let mut reached = BTreeSet::from([source.to_owned()]);
    for _ in 0..scope.len() {
        let ready: Vec<_> = scope
            .iter()
            .filter(|node| !reached.contains(*node))
            .filter(|node| {
                let incoming: Vec<_> = edges.values().filter(|edge| edge.to == **node).collect();
                !incoming.is_empty() && incoming.iter().all(|edge| reached.contains(&edge.from))
            })
            .cloned()
            .collect();
        if ready.is_empty() {
            break;
        }
        reached.extend(ready);
    }
    if &reached != scope {
        return false;
    }
    scope.iter().all(|node| {
        let outgoing: Vec<_> = edges.values().filter(|edge| edge.from == *node).collect();
        if node == sink {
            outgoing.is_empty()
        } else {
            !outgoing.is_empty()
        }
    })
}

fn join(node: &str, config: &serde_json::Value, edges: &BTreeMap<u64, EdgeDefinition>) -> bool {
    if config
        .get("failure_policy")
        .and_then(serde_json::Value::as_str)
        != Some("wait_all")
    {
        return false;
    }
    let mut members = BTreeSet::new();
    for side in ["left_exits", "right_exits"] {
        let Some(values) = config.get(side).and_then(serde_json::Value::as_array) else {
            return false;
        };
        let [value] = values.as_slice() else {
            return false;
        };
        let Some(id) = value.as_str() else {
            return false;
        };
        if !members.insert(id) {
            return false;
        }
    }
    members
        == edges
            .values()
            .filter(|edge| edge.to == node)
            .map(|edge| edge.from.as_str())
            .collect::<BTreeSet<_>>()
}
