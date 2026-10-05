//! Exact-revision pending topology edits with immutable revision receipts.
use super::{Database, DatabaseValue, RepresentationError as Error, TransactionMode};
use crate::WorkflowExecutionAuthority;
use bcode_workflow::{
    EdgeDefinition, WorkflowDefinition, WorkflowRunGraphEdit as Edit, WorkflowRunGraphEditBatch,
    WorkflowRunGraphReconciliation as Reconcile,
};
use std::collections::{BTreeMap, BTreeSet};
use switchy_database::{
    query::FilterableQuery,
    schema::{DataType, create_table},
};
const RECEIPTS: &str = "workflow_graph_revisions_v2";
#[cfg(test)]
mod tests;

pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(RECEIPTS)
        .column(super::column("identity", DataType::Text, false))
        .primary_key("identity")
        .column(super::column("run_id", DataType::Text, false))
        .column(super::column("revision", DataType::Int, false))
        .column(super::column("request_json", DataType::Text, false))
        .column(super::column("definition_json", DataType::Text, false))
        .column(super::column("edges_json", DataType::Text, false))
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
fn decode<T: serde::de::DeserializeOwned>(
    row: &switchy_database::Row,
    key: &str,
) -> Result<T, Error> {
    match row.get(key) {
        Some(DatabaseValue::String(json)) => {
            serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)
        }
        _ => Err(Error::MaintenanceRequired),
    }
}
/// Load the canonical current graph and its revision in the caller's transaction.
/// # Errors
/// Rejects absent/malformed graph state and database failures.
pub async fn current(db: &dyn Database, run: &str) -> Result<(u64, WorkflowDefinition), Error> {
    let rows = db
        .select("workflow_graphs_v2")
        .where_eq("run_id", run)
        .limit(1)
        .execute(db)
        .await?;
    let row = rows.first().ok_or(Error::InvalidAdmission)?;
    let Some(DatabaseValue::Int64(revision)) = row.get("revision") else {
        return Err(Error::MaintenanceRequired);
    };
    let revision = u64::try_from(revision).map_err(|_| Error::MaintenanceRequired)?;
    if revision == 0 {
        return Err(Error::MaintenanceRequired);
    }
    Ok((revision, decode(row, "definition_json")?))
}
fn apply(
    definition: &mut WorkflowDefinition,
    edges: &mut BTreeMap<u64, EdgeDefinition>,
    edit: &Edit,
) -> Result<(), Error> {
    match edit {
        Edit::AddNode { node, entry, exit } | Edit::ReplaceNode { node, entry, exit } => {
            if definition.nodes.contains_key(&node.id) != matches!(edit, Edit::ReplaceNode { .. }) {
                return Err(Error::InvalidAdmission);
            }
            definition.entries.retain(|id| id != &node.id);
            definition.exits.retain(|id| id != &node.id);
            if *entry {
                definition.entries.push(node.id.clone());
            }
            if *exit {
                definition.exits.push(node.id.clone());
            }
            definition.nodes.insert(node.id.clone(), node.clone());
        }
        Edit::RemoveNode { node_id } => {
            definition
                .nodes
                .remove(node_id)
                .ok_or(Error::InvalidAdmission)?;
            definition.entries.retain(|id| id != node_id);
            definition.exits.retain(|id| id != node_id);
        }
        Edit::AddEdge { edge_id, edge } | Edit::ReplaceEdge { edge_id, edge } => {
            if edges.contains_key(edge_id) != matches!(edit, Edit::ReplaceEdge { .. }) {
                return Err(Error::InvalidAdmission);
            }
            edges.insert(*edge_id, edge.clone());
        }
        Edit::RemoveEdge { edge_id } => {
            edges.remove(edge_id).ok_or(Error::InvalidAdmission)?;
        }
    }
    Ok(())
}

/// Publish an authorized pending-topology edit under exact durable execution authority.
///
/// Attempt-backed/completed nodes and their incident edges are protected. They require explicit
/// Retain reconciliation; cancellation and output rebinding are not guessed. Pending rows may be
/// retired/reinitialized atomically. Dispatch remains disabled until broader lifecycle integration.
/// # Errors
/// Rejects stale revision/owner, conflicting duplicates, affected active work, unsupported
/// reconciliation/control flow, invalid graph/schema or storage failure.
pub async fn publish(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    batch: &WorkflowRunGraphEditBatch,
) -> Result<u64, Error> {
    publish_with_policy(db, owner, batch, super::GraphAdmissionPolicy::default()).await
}
/// Publish with a caller-selected complete-candidate validation budget.
/// # Errors
/// Rejects snapshots exceeding policy or any ordinary publication failure.
pub async fn publish_with_policy(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    batch: &WorkflowRunGraphEditBatch,
    policy: super::GraphAdmissionPolicy,
) -> Result<u64, Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    let result = publish_in(&*tx, owner, batch, policy).await?;
    tx.commit().await?;
    Ok(result)
}
pub(super) async fn publish_in(
    tx: &dyn switchy_database::DatabaseTransaction,
    owner: &WorkflowExecutionAuthority,
    batch: &WorkflowRunGraphEditBatch,
    policy: super::GraphAdmissionPolicy,
) -> Result<u64, Error> {
    batch.validate().map_err(|_| Error::InvalidAdmission)?;
    let request = encode(batch)?;
    super::verified(tx).await?;
    let root = owned_run(tx, &batch.run_id, owner).await?;
    let receipt_id = encode(&(batch.run_id.as_str(), batch.mutation_id.as_str()))?;
    if let Some(revision) = duplicate(tx, &receipt_id, &request).await? {
        return Ok(revision);
    }
    if root.get("status") != Some(DatabaseValue::String("paused".into())) {
        return Err(Error::InvalidAdmission);
    }
    super::safety::ensure_admission_allowed(tx, &batch.run_id).await?;
    ensure_frontier_idle(tx, &batch.run_id).await?;
    let (revision, original) = current(tx, &batch.run_id).await?;
    if revision != batch.expected_revision {
        return Err(Error::InvalidAdmission);
    }
    let rows = tx
        .select("workflow_graphs_v2")
        .where_eq("run_id", batch.run_id.as_str())
        .limit(1)
        .execute(tx)
        .await?;
    let mut edges: BTreeMap<u64, EdgeDefinition> = decode(
        rows.first().ok_or(Error::MaintenanceRequired)?,
        "edges_json",
    )?;
    let old_edges = edges.clone();
    let mut candidate = original.clone();
    apply_edits(tx, batch, &mut candidate, &mut edges).await?;
    candidate.edges = edges.values().cloned().collect();
    super::graph::validate(&candidate)?;
    let protected = protected_nodes(tx, batch, &original).await?;
    protect(
        tx, batch, &original, &candidate, &old_edges, &edges, &protected,
    )
    .await?;
    super::graph::terminal_scan::invalidate(tx, &batch.run_id).await?;
    let next = revision
        .checked_add(1)
        .and_then(|n| i64::try_from(n).ok())
        .ok_or(Error::InvalidAdmission)?;
    persist_candidate_rows(tx, &batch.run_id, next, &candidate, &edges).await?;
    let definition = serde_json::to_string(&candidate).map_err(|_| Error::InvalidAdmission)?;
    policy.check(definition.len())?;
    let edge_json = serde_json::to_string(&edges).map_err(|_| Error::InvalidAdmission)?;
    tx.insert(RECEIPTS)
        .value("identity", receipt_id)
        .value("run_id", batch.run_id.as_str())
        .value("revision", next)
        .value("request_json", request)
        .value("definition_json", definition.clone())
        .value("edges_json", edge_json.clone())
        .execute(tx)
        .await?;
    tx.update("workflow_graphs_v2")
        .value("revision", next)
        .value("definition_json", definition)
        .value("edges_json", edge_json)
        .where_eq("run_id", batch.run_id.as_str())
        .execute(tx)
        .await?;
    let admission: crate::NewWorkflowRun = decode(&root, "admission_json")?;
    for id in original.nodes.keys().filter(|id| !protected.contains(*id)) {
        if original.nodes.get(id) == candidate.nodes.get(id)
            && old_edges
                .iter()
                .filter(|(_, e)| &e.to == id)
                .collect::<Vec<_>>()
                == edges
                    .iter()
                    .filter(|(_, e)| &e.to == id)
                    .collect::<Vec<_>>()
        {
            continue;
        }
        tx.update("workflow_activations_v2")
            .value("current", false)
            .where_eq("run_id", batch.run_id.as_str())
            .where_eq("node_id", id.as_str())
            .execute(tx)
            .await?;
    }
    initialize_entries(tx, &batch.run_id, &candidate, &protected, &admission).await?;
    u64::try_from(next).map_err(|_| Error::MaintenanceRequired)
}
async fn protect(
    db: &dyn Database,
    batch: &WorkflowRunGraphEditBatch,
    original: &WorkflowDefinition,
    candidate: &WorkflowDefinition,
    old: &BTreeMap<u64, EdgeDefinition>,
    new: &BTreeMap<u64, EdgeDefinition>,
    protected: &BTreeSet<String>,
) -> Result<(), Error> {
    for id in protected {
        if original.nodes.get(id) != candidate.nodes.get(id)
            || original.entries.contains(id) != candidate.entries.contains(id)
            || original.exits.contains(id) != candidate.exits.contains(id)
        {
            return Err(Error::UnsupportedLifecycle);
        }
        let activation = super::graph::activation_id(db, &batch.run_id, id)
            .await?
            .ok_or(Error::MaintenanceRequired)?;
        let bindings = batch.reconciliation.iter().find_map(|r| match r {
            Reconcile::RetainWithBindings {
                activation_id,
                edge_ids,
            } if activation_id == &activation => Some(edge_ids),
            _ => None,
        });
        let incoming = |edges: &BTreeMap<u64, EdgeDefinition>| {
            edges
                .iter()
                .filter(|(_, edge)| &edge.to == id)
                .map(|(key, edge)| (*key, edge.clone()))
                .collect::<BTreeMap<_, _>>()
        };
        if incoming(old) != incoming(new) {
            return Err(Error::UnsupportedLifecycle);
        }
        let before: BTreeMap<_, _> = old.iter().filter(|(_, edge)| &edge.from == id).collect();
        let after: BTreeMap<_, _> = new.iter().filter(|(_, edge)| &edge.from == id).collect();
        if before != after {
            let Some(bindings) = bindings else {
                return Err(Error::UnsupportedLifecycle);
            };
            if bindings.iter().copied().collect::<BTreeSet<_>>()
                != after.keys().map(|key| **key).collect()
            {
                return Err(Error::InvalidAdmission);
            }
            // Already-settled sources need explicit replay bookkeeping; preserve them for now.
            if super::graph::output(db, &batch.run_id, id).await?.is_some() {
                return Err(Error::UnsupportedLifecycle);
            }
        } else if bindings.is_some_and(|ids| {
            ids.iter().copied().collect::<BTreeSet<_>>() != after.keys().map(|key| **key).collect()
        }) {
            return Err(Error::InvalidAdmission);
        }
    }
    Ok(())
}

pub(super) async fn validate_cancellation_candidate(
    tx: &dyn switchy_database::DatabaseTransaction,
    batch: &WorkflowRunGraphEditBatch,
) -> Result<(), Error> {
    ensure_frontier_idle(tx, &batch.run_id).await?;
    let (_, original) = current(tx, &batch.run_id).await?;
    let rows = tx
        .select("workflow_graphs_v2")
        .where_eq("run_id", batch.run_id.as_str())
        .limit(1)
        .execute(tx)
        .await?;
    let mut edges: BTreeMap<u64, EdgeDefinition> = decode(
        rows.first().ok_or(Error::MaintenanceRequired)?,
        "edges_json",
    )?;
    let old_edges = edges.clone();
    let mut candidate = original.clone();
    apply_edits(tx, batch, &mut candidate, &mut edges).await?;
    candidate.edges = edges.values().cloned().collect();
    super::graph::validate(&candidate)?;
    let mut retained = BTreeSet::new();
    for id in original.nodes.keys() {
        if let Some(activation) = super::graph::activation_id(tx, &batch.run_id, id).await? {
            if batch
                .reconciliation
                .iter()
                .any(|r| matches!(r,Reconcile::Cancel{activation_id} if activation_id==&activation))
            {
                if candidate.nodes.contains_key(id) {
                    return Err(Error::UnsupportedLifecycle);
                }
            } else {
                retained.insert(id.clone());
            }
        }
    }
    protect(
        tx, batch, &original, &candidate, &old_edges, &edges, &retained,
    )
    .await
}

async fn protected_nodes(
    db: &dyn Database,
    batch: &WorkflowRunGraphEditBatch,
    original: &WorkflowDefinition,
) -> Result<BTreeSet<String>, Error> {
    let mut protected = BTreeSet::new();
    for id in original.nodes.keys() {
        let Some(activation) = super::graph::activation_id(db, &batch.run_id, id).await? else {
            continue;
        };
        let identity = crate::dispatch_identity(&batch.run_id, id, &activation, 1);
        let attempts = db
            .select("workflow_attempts_v2")
            .columns(&["dispatch_identity"])
            .where_eq("dispatch_identity", identity)
            .limit(1)
            .execute(db)
            .await?;
        if !attempts.is_empty() || super::graph::output(db, &batch.run_id, id).await?.is_some() {
            protected.insert(id.clone());
        }
    }
    let mut cancelled = BTreeSet::new();
    for id in &protected {
        let activation = super::graph::activation_id(db, &batch.run_id, id)
            .await?
            .ok_or(Error::MaintenanceRequired)?;
        if batch.reconciliation.iter().any(
            |r| matches!(r, Reconcile::Cancel { activation_id } if activation_id == &activation),
        ) {
            if !super::execution_graph::cancellation::cancellation_reconciled(
                db,
                batch,
                &activation,
            )
            .await?
                || super::graph::output(db, &batch.run_id, id).await?.is_some()
            {
                return Err(Error::UnsupportedLifecycle);
            }
            cancelled.insert(id.clone());
            continue;
        }
        if !batch.reconciliation.iter().any(|r| match r {
            Reconcile::Retain { activation_id }
            | Reconcile::RetainWithBindings { activation_id, .. } => activation_id == &activation,
            Reconcile::Cancel { .. } => false,
        }) {
            return Err(Error::UnsupportedLifecycle);
        }
    }
    for r in &batch.reconciliation {
        let id = match r {
            Reconcile::Retain { activation_id }
            | Reconcile::RetainWithBindings { activation_id, .. }
            | Reconcile::Cancel { activation_id } => activation_id,
        };
        let mut known = false;
        for node in &protected {
            if super::graph::activation_id(db, &batch.run_id, node)
                .await?
                .as_ref()
                == Some(id)
            {
                known = true;
            }
        }
        if !known {
            return Err(Error::InvalidAdmission);
        }
    }
    for id in cancelled {
        protected.remove(&id);
    }
    Ok(protected)
}

async fn duplicate(db: &dyn Database, id: &str, request: &str) -> Result<Option<u64>, Error> {
    let rows = db
        .select(RECEIPTS)
        .where_eq("identity", id)
        .limit(1)
        .execute(db)
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    if row.get("request_json") != Some(DatabaseValue::String(request.into())) {
        return Err(Error::InvalidAdmission);
    }
    match row.get("revision") {
        Some(DatabaseValue::Int64(revision)) => Ok(Some(
            u64::try_from(revision).map_err(|_| Error::MaintenanceRequired)?,
        )),
        _ => Err(Error::MaintenanceRequired),
    }
}

async fn owned_run(
    db: &dyn Database,
    run: &str,
    owner: &WorkflowExecutionAuthority,
) -> Result<switchy_database::Row, Error> {
    let rows = db
        .select(super::RUNS)
        .where_eq("run_id", run)
        .limit(1)
        .execute(db)
        .await?;
    let root = rows.into_iter().next().ok_or(Error::InvalidAdmission)?;
    if root.get("authority_json") != Some(DatabaseValue::String(encode(owner)?)) {
        return Err(Error::AuthorityConflict);
    }
    Ok(root)
}

async fn initialize_entries(
    db: &dyn Database,
    run: &str,
    candidate: &WorkflowDefinition,
    protected: &BTreeSet<String>,
    admission: &crate::NewWorkflowRun,
) -> Result<(), Error> {
    for id in candidate
        .entries
        .iter()
        .filter(|id| !protected.contains(*id))
    {
        if super::graph::is_ready(db, run, id).await? {
            continue;
        }
        super::graph::insert_activation(
            db,
            run,
            id,
            admission.input.as_ref().unwrap_or(&serde_json::Value::Null),
        )
        .await?;
    }
    Ok(())
}

async fn ensure_frontier_idle(db: &dyn Database, run: &str) -> Result<(), Error> {
    if super::graph::frontier::pending(db, run).await? {
        return Err(Error::UnsupportedLifecycle);
    }
    Ok(())
}

async fn apply_edits(
    db: &dyn Database,
    batch: &WorkflowRunGraphEditBatch,
    candidate: &mut WorkflowDefinition,
    edges: &mut BTreeMap<u64, EdgeDefinition>,
) -> Result<(), Error> {
    let next = super::graph_edges::next_edge_id(db, &batch.run_id).await?;
    for edit in &batch.edits {
        if let Edit::AddEdge { edge_id, .. } = edit
            && next.is_none_or(|next| *edge_id < next)
        {
            return Err(Error::InvalidAdmission);
        }
        apply(candidate, edges, edit)?;
    }
    Ok(())
}

async fn persist_candidate_rows(
    tx: &dyn Database,
    run: &str,
    next: i64,
    candidate: &WorkflowDefinition,
    edges: &BTreeMap<u64, EdgeDefinition>,
) -> Result<(), Error> {
    super::graph_pages::persist(
        tx,
        run,
        u64::try_from(next).map_err(|_| Error::InvalidAdmission)?,
        candidate,
    )
    .await?;
    super::graph_edges::persist(
        tx,
        run,
        u64::try_from(next).map_err(|_| Error::InvalidAdmission)?,
        candidate,
        edges,
    )
    .await?;
    Ok(())
}
