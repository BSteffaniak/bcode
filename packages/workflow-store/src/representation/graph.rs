//! Revision-pinned static activation graph and deterministic output settlement.
//!
//! Unsupported control flow fails closed. This module does not enable external dispatch.
use super::{Database, DatabaseValue, RUNS, RepresentationError as Error, TransactionMode, column};
use crate::WorkflowExecutionAuthority;
use bcode_workflow::{EdgeKind, NodeKind, WorkflowDefinition, WorkflowTransformInput};
use serde_json::Value;
use std::collections::BTreeMap;
use switchy_database::{
    query::{FilterableQuery, SortDirection},
    schema::{DataType, create_index, create_table},
};
const ACTIVATIONS: &str = "workflow_activations_v2";
mod cancellation;
/// Atomic normalized output and attempt settlement.
pub mod completion;
/// Bounded branch/repeat control settlement.
pub mod control;
mod control_state;
pub mod fanout;
/// Incremental homogeneous fan-out members and capacity.
/// Durable bounded fan-out final reduction.
pub mod fanout_progress;
/// Durable bounded successor settlement frontier.
pub mod frontier;
/// Explicit typed input/approval gate resolution.
/// Bounded canonical interactive gate observations.
pub mod gate_waits;
pub mod gates;
/// Durable parallel failure and outcome collection.
pub mod parallel;
/// Epoch-fenced incremental member outcome reduction.
mod parallel_chunks;
pub mod parallel_progress;
/// Durable repeat restart discovery and retirement.
pub mod restart;
/// Durable terminal proof pagination.
pub mod terminal_scan;
#[cfg(test)]
mod tests;
const GRAPHS: &str = "workflow_graphs_v2";
/// Current initial graph revision. Later edits must use explicit graph reconciliation.
pub const INITIAL_REVISION: u64 = 1;

/// Exact activation observation; ready is not authorization to dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    /// Stable node identity.
    pub node_id: String,
    /// Stable generation-specific execution identity.
    pub activation_id: String,
    /// Unix milliseconds captured and persisted once when this activation was inserted.
    pub created_at_ms: u64,
    /// Monotonic per-node activation generation.
    pub generation: u64,
    /// Exact pinned graph revision.
    pub revision: u64,
    /// Validated canonical input.
    pub input: Value,
    /// Validated canonical output, once settled.
    pub output: Option<Value>,
}

/// Result of committing an output and its deterministic successor inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settlement {
    /// True only for an identical previously committed output.
    pub duplicate: bool,
    /// Validated graph result, if its unique exit has settled. Not run terminalization.
    pub graph_output: Option<Value>,
}

pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    control_state::install(db).await?;
    completion::install(db).await?;
    parallel::install(db).await?;
    parallel_progress::install(db).await?;
    fanout::install(db).await?;
    frontier::install(db).await?;
    restart::install(db).await?;
    terminal_scan::install(db).await?;
    create_table(GRAPHS)
        .column(column("run_id", DataType::Text, false))
        .primary_key("run_id")
        .column(column("revision", DataType::Int, false))
        .column(column("terminal_json", DataType::Text, true))
        .column(column("definition_json", DataType::Text, false))
        .column(column("edges_json", DataType::Text, false))
        .execute(db)
        .await?;
    create_table(ACTIVATIONS)
        .column(column("identity", DataType::Text, false))
        .primary_key("identity")
        .column(column("run_id", DataType::Text, false))
        .column(column("node_id", DataType::Text, false))
        .column(column("revision", DataType::Int, false))
        .column(column("generation", DataType::Int, false))
        .column(column("created_at_ms", DataType::Int, false))
        .column(column("current", DataType::Bool, false))
        .column(column("skipped", DataType::Bool, false))
        .column(column("input_json", DataType::Text, false))
        .column(column("output_json", DataType::Text, true))
        .execute(db)
        .await?;
    create_index("idx_workflow_activations_v2_generation")
        .table(ACTIVATIONS)
        .columns(vec!["run_id", "node_id", "current", "generation"])
        .execute(db)
        .await?;
    create_index("idx_workflow_activations_v2_ready")
        .table(ACTIVATIONS)
        .columns(vec!["run_id", "current", "output_json", "node_id"])
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
fn text(row: &switchy_database::Row, field: &str) -> Result<String, Error> {
    match row.get(field) {
        Some(DatabaseValue::String(s)) if s.len() <= super::MAX_ADMISSION_BYTES => Ok(s),
        _ => Err(Error::MaintenanceRequired),
    }
}
fn decode<T: serde::de::DeserializeOwned>(
    row: &switchy_database::Row,
    field: &str,
) -> Result<T, Error> {
    serde_json::from_str(&text(row, field)?).map_err(|_| Error::MaintenanceRequired)
}
fn schema(schema: &Value, value: &Value) -> Result<(), Error> {
    jsonschema::validator_for(schema)
        .map_err(|_| Error::InvalidAdmission)?
        .validate(value)
        .map_err(|_| Error::InvalidAdmission)
}

/// Validate supported static topology without guessing semantics of future/control variants.
/// # Errors
/// Rejects cyclic/conditional/retry graphs, non-leaf/non-join controllers, ambiguous fan-in,
/// multiple exits, transformed joins and unsupported join failure policies.
pub fn validate(definition: &WorkflowDefinition) -> Result<(), Error> {
    definition.validate().map_err(|_| Error::InvalidAdmission)?;
    if definition.exits.len() != 1
        || definition
            .edges
            .iter()
            .any(|e| matches!(e.kind, EdgeKind::Retry { .. }))
    {
        return Err(Error::UnsupportedLifecycle);
    }
    let mut incoming: BTreeMap<&str, usize> =
        definition.nodes.keys().map(|id| (id.as_str(), 0)).collect();
    for edge in definition
        .edges
        .iter()
        .filter(|edge| !matches!(edge.kind, EdgeKind::Back { .. }))
    {
        if edge.transform.as_ref().is_some_and(|transform| {
            transform.referenced_sources().iter().any(|source| {
                source
                    .strip_prefix(bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_DEPENDENCY_PREFIX)
                    .is_some_and(|id| id != edge.from)
            })
        }) {
            return Err(Error::UnsupportedLifecycle);
        }
        *incoming
            .get_mut(edge.to.as_str())
            .ok_or(Error::InvalidAdmission)? += 1;
    }
    for node in definition.nodes.values() {
        if !matches!(
            node.kind,
            NodeKind::Task
                | NodeKind::Agent
                | NodeKind::PluginBlock
                | NodeKind::Parallel
                | NodeKind::Branch
                | NodeKind::Repeat
                | NodeKind::Input
                | NodeKind::Approval
                | NodeKind::WorkflowCall
                | NodeKind::FanOut
        ) {
            return Err(Error::UnsupportedLifecycle);
        }
        if node.kind == NodeKind::Parallel {
            bcode_workflow::parallel_join_member_ids(node).map_err(|_| Error::InvalidAdmission)?;
            if !node.resources.is_empty()
                || ["left_exits", "right_exits"].iter().any(|side| {
                    node.configuration
                        .get(side)
                        .and_then(Value::as_array)
                        .is_none_or(Vec::is_empty)
                })
            {
                return Err(Error::UnsupportedLifecycle);
            }
            if definition
                .edges
                .iter()
                .any(|edge| edge.to == node.id && edge.transform.is_some())
            {
                return Err(Error::UnsupportedLifecycle);
            }
        }
        if definition.entries.contains(&node.id) != (incoming[node.id.as_str()] == 0) {
            return Err(Error::UnsupportedLifecycle);
        }
    }
    let mut queue: Vec<&str> = incoming
        .iter()
        .filter_map(|(id, n)| (*n == 0).then_some(*id))
        .collect();
    let mut visited = 0;
    while let Some(id) = queue.pop() {
        visited += 1;
        for edge in definition
            .edges
            .iter()
            .filter(|e| e.from == id && !matches!(e.kind, EdgeKind::Back { .. }))
        {
            let count = incoming
                .get_mut(edge.to.as_str())
                .ok_or(Error::InvalidAdmission)?;
            *count -= 1;
            if *count == 0 {
                queue.push(&edge.to);
            }
        }
    }
    if visited != definition.nodes.len() {
        return Err(Error::UnsupportedLifecycle);
    }
    Ok(())
}

pub(super) async fn initialize(
    db: &dyn Database,
    run: &crate::NewWorkflowRun,
    definition: &WorkflowDefinition,
) -> Result<(), Error> {
    validate(definition)?;
    super::graph_pages::persist(db, &run.run_id, INITIAL_REVISION, definition).await?;
    let edges = definition
        .edges
        .iter()
        .enumerate()
        .map(|(index, edge)| (index as u64 + 1, edge.clone()))
        .collect();
    super::graph_edges::persist(db, &run.run_id, INITIAL_REVISION, definition, &edges).await?;
    db.insert(GRAPHS)
        .value("run_id", run.run_id.as_str())
        .value("revision", 1_i64)
        .value("terminal_json", DatabaseValue::Null)
        .value(
            "definition_json",
            serde_json::to_string(definition).map_err(|_| Error::InvalidAdmission)?,
        )
        .value(
            "edges_json",
            serde_json::to_string(
                &definition
                    .edges
                    .iter()
                    .enumerate()
                    .map(|(i, e)| ((i as u64) + 1, e.clone()))
                    .collect::<BTreeMap<_, _>>(),
            )
            .map_err(|_| Error::InvalidAdmission)?,
        )
        .execute(db)
        .await?;
    let input = run.input.as_ref().unwrap_or(&Value::Null);
    for node in &definition.entries {
        schema(&definition.nodes[node].input.schema, input)?;
        insert_activation(db, &run.run_id, node, input).await?;
    }
    Ok(())
}
pub(super) async fn insert_activation(
    db: &dyn Database,
    run: &str,
    node: &str,
    input: &Value,
) -> Result<(), Error> {
    insert_activation_kind(db, run, node, input, false).await
}
pub(super) async fn insert_skipped(db: &dyn Database, run: &str, node: &str) -> Result<(), Error> {
    insert_activation_kind(db, run, node, &Value::Null, true).await
}
async fn insert_activation_kind(
    db: &dyn Database,
    run: &str,
    node: &str,
    input: &Value,
    skipped: bool,
) -> Result<(), Error> {
    let revision = super::graph_pages::revision(db, run).await?;
    if !skipped {
        schema(&execution_node(db, run, node).await?.input.schema, input)?;
    }
    if activation(db, run, node).await?.is_some() {
        return Err(Error::InvalidAdmission);
    }
    let rows = db
        .select(ACTIVATIONS)
        .columns(&["generation"])
        .where_eq("run_id", run)
        .where_eq("node_id", node)
        .sort("generation", SortDirection::Desc)
        .limit(1)
        .execute(db)
        .await?;
    let generation = match rows.first().and_then(|row| row.get("generation")) {
        None => 1,
        Some(DatabaseValue::Int64(n)) if n > 0 => {
            n.checked_add(1).ok_or(Error::MaintenanceRequired)?
        }
        _ => return Err(Error::MaintenanceRequired),
    };
    let identity = format!(
        "activation:{}",
        crate::sha256_hex(encode(&(run, node, generation))?.as_bytes())
    );
    terminal_scan::invalidate(db, run).await?;
    db.insert(ACTIVATIONS)
        .value("identity", identity)
        .value("generation", generation)
        .value("created_at_ms", super::runs::timestamp()?)
        .value("current", true)
        .value("skipped", skipped)
        .value("run_id", run)
        .value("node_id", node)
        .value(
            "revision",
            i64::try_from(revision).map_err(|_| Error::InvalidAdmission)?,
        )
        .value("input_json", encode(input)?)
        .value("output_json", DatabaseValue::Null)
        .execute(db)
        .await?;
    Ok(())
}
fn creation_time(row: &switchy_database::Row) -> Result<u64, Error> {
    match row.get("created_at_ms") {
        Some(DatabaseValue::Int64(n)) => u64::try_from(n).map_err(|_| Error::MaintenanceRequired),
        _ => Err(Error::MaintenanceRequired),
    }
}

async fn activation(
    db: &dyn Database,
    run: &str,
    node: &str,
) -> Result<Option<switchy_database::Row>, Error> {
    Ok(db
        .select(ACTIVATIONS)
        .where_eq("run_id", run)
        .where_eq("node_id", node)
        .where_eq("current", true)
        .limit(1)
        .execute(db)
        .await?
        .into_iter()
        .next())
}
pub(super) async fn output(
    db: &dyn Database,
    run: &str,
    node: &str,
) -> Result<Option<Value>, Error> {
    let Some(row) = activation(db, run, node).await? else {
        return Ok(None);
    };
    if row.get("output_json") == Some(DatabaseValue::Null) {
        Ok(None)
    } else {
        Ok(Some(decode(&row, "output_json")?))
    }
}

/// Check an exact initial-revision activation before reserving an attempt.
/// # Errors
/// Rejects absent/completed activations or inconsistent durable state.
pub async fn is_ready(db: &dyn Database, run: &str, node: &str) -> Result<bool, Error> {
    if restart::pending(db, run).await? {
        return Ok(false);
    }
    Ok(activation(db, run, node).await?.is_some_and(|row| {
        row.get("skipped") == Some(DatabaseValue::Int64(0))
            && row.get("output_json") == Some(DatabaseValue::Null)
    }))
}

async fn join_input(
    db: &dyn Database,
    run: &str,
    node: &bcode_workflow::NodeDefinition,
) -> Result<Option<Value>, Error> {
    let mut pair = Vec::new();
    for side in ["left_exits", "right_exits"] {
        let members = node
            .configuration
            .get(side)
            .and_then(Value::as_array)
            .ok_or(Error::InvalidAdmission)?;
        let mut selected = None;
        for member in members {
            let member = member.as_str().ok_or(Error::InvalidAdmission)?;
            let Some(row) = activation(db, run, member).await? else {
                return Ok(None);
            };
            if row.get("skipped") == Some(DatabaseValue::Int64(1)) {
                continue;
            }
            let Some(value) = output(db, run, member).await? else {
                return Ok(None);
            };
            if selected.replace(value).is_some() {
                return Err(Error::MaintenanceRequired);
            }
        }
        pair.push(selected.ok_or(Error::MaintenanceRequired)?);
    }
    Ok(Some(Value::Array(pair)))
}

async fn transformed(
    db: &dyn Database,
    run: &crate::NewWorkflowRun,
    edge: &bcode_workflow::EdgeDefinition,
    current: &Value,
) -> Result<Value, Error> {
    let Some(transform) = &edge.transform else {
        return Ok(current.clone());
    };
    let mut values = BTreeMap::from([
        (
            bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT.to_string(),
            current.clone(),
        ),
        (
            bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_STATE.to_string(),
            run.input.clone().unwrap_or(Value::Null),
        ),
        (
            bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CONFIGURATION.to_string(),
            run.authored_provenance
                .as_ref()
                .map_or(Value::Null, |p| p.configuration.clone()),
        ),
    ]);
    for source in transform.referenced_sources() {
        if let Some(node) =
            source.strip_prefix(bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_DEPENDENCY_PREFIX)
        {
            values.insert(
                source.clone(),
                output(db, &run.run_id, node)
                    .await?
                    .ok_or(Error::UnsupportedLifecycle)?,
            );
        }
    }
    transform
        .evaluate(
            &values
                .iter()
                .map(|(name, value)| WorkflowTransformInput { name, value })
                .collect::<Vec<_>>(),
        )
        .map_err(|_| Error::InvalidAdmission)
}

async fn propagate_paged(
    db: &dyn Database,
    run: &crate::NewWorkflowRun,
    revision: u64,
    node: &str,
    value: &Value,
) -> Result<(), Error> {
    frontier::enqueue(db, &run.run_id, revision, node, value).await?;
    frontier::drain_in(db, run, 32).await?;
    Ok(())
}
async fn settle_edge(
    db: &dyn Database,
    run: &crate::NewWorkflowRun,
    revision: u64,
    edge: &bcode_workflow::EdgeDefinition,
    value: &Value,
) -> Result<Option<(String, Value)>, Error> {
    match &edge.kind {
        EdgeKind::Direct => {}
        EdgeKind::Conditional {
            predicate,
            expected,
        } => {
            if predicate
                .evaluate_value(value)
                .map_err(|_| Error::InvalidAdmission)?
                != *expected
            {
                return Ok(None);
            }
        }
        EdgeKind::Back { .. } => return Ok(None),
        EdgeKind::Retry { .. } => return Err(Error::UnsupportedLifecycle),
    }
    if activation(db, &run.run_id, &edge.to).await?.is_some() {
        return Ok(None);
    }
    let target = super::graph_pages::node(db, &run.run_id, revision, &edge.to)
        .await?
        .ok_or(Error::MaintenanceRequired)?;
    if target.kind == NodeKind::Parallel
        && target
            .configuration
            .get("failure_policy")
            .is_some_and(|policy| policy != "wait_all")
    {
        return Ok(None);
    }

    let input = if target.kind == NodeKind::Parallel {
        let Some(input) = join_input(db, &run.run_id, &target).await? else {
            return Ok(None);
        };
        input
    } else {
        transformed(db, run, edge, value).await?
    };
    schema(&target.input.schema, &input)?;
    insert_activation(db, &run.run_id, &edge.to, &input).await?;
    if target.kind == NodeKind::Parallel {
        schema(&target.output.schema, &input)?;
        db.update(ACTIVATIONS)
            .value("output_json", encode(&input)?)
            .where_eq(
                "identity",
                activation_id(db, &run.run_id, &edge.to)
                    .await?
                    .ok_or(Error::MaintenanceRequired)?,
            )
            .execute(db)
            .await?;
        completion::retain_control(
            db,
            &run.run_id,
            &target,
            &activation_id(db, &run.run_id, &edge.to)
                .await?
                .ok_or(Error::MaintenanceRequired)?,
            &input,
        )
        .await?;
        return Ok(Some((edge.to.clone(), input)));
    }
    Ok(None)
}

/// Commit a receipt-backed successful output and deterministic successors at an exact revision.
///
/// Graph completion does not terminalize the run or enable dispatch. Failed/cancelled attempts
/// require separate graph failure policy and cannot call this API.
/// # Errors
/// Rejects stale authority/revision, missing successful attempt, invalid schemas or conflicting output.
pub async fn settle_output(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run_id: &str,
    node: &str,
    revision: u64,
    value: &Value,
) -> Result<Settlement, Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    let result = settle_output_in(&*tx, owner, run_id, node, revision, value).await?;
    tx.commit().await?;
    Ok(result)
}
pub(super) async fn settle_output_in(
    tx: &dyn switchy_database::DatabaseTransaction,
    owner: &WorkflowExecutionAuthority,
    run_id: &str,
    node: &str,
    revision: u64,
    value: &Value,
) -> Result<Settlement, Error> {
    if revision == 0 {
        return Err(Error::InvalidAdmission);
    }
    terminal_scan::invalidate(tx, run_id).await?;
    let encoded = encode(value)?;
    super::verified(tx).await?;
    let root = tx
        .select(RUNS)
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(tx)
        .await?
        .into_iter()
        .next()
        .ok_or(Error::InvalidAdmission)?;
    if text(&root, "authority_json")? != encode(owner)? {
        return Err(Error::AuthorityConflict);
    }
    if super::graph_pages::revision(tx, run_id).await? != revision {
        return Err(Error::InvalidAdmission);
    }
    let boundary = super::graph_edges::boundary(tx, run_id, revision).await?;
    let request: crate::NewWorkflowRun = decode(&root, "admission_json")?;
    let target = execution_node(tx, run_id, node).await?;
    schema(&target.output.schema, value)?;
    let active = activation(tx, run_id, node)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    let duplicate = active.get("output_json") != Some(DatabaseValue::Null);
    if duplicate {
        if text(&active, "output_json")? != encoded {
            return Err(Error::InvalidAdmission);
        }
    } else {
        if text(&root, "status")? != "paused" {
            return Err(Error::InvalidAdmission);
        }
        successful_attempt(tx, run_id, node, &text(&active, "identity")?).await?;
        tx.update(ACTIVATIONS)
            .value("output_json", encoded)
            .where_eq("identity", text(&active, "identity")?)
            .execute(tx)
            .await?;
        propagate_paged(tx, &request, revision, node, value).await?;
    }
    let terminal = output(tx, run_id, &boundary.exits[0]).await?;
    if let Some(result) = &terminal {
        if !tx
            .select(ACTIVATIONS)
            .columns(&["identity"])
            .where_eq("run_id", run_id)
            .where_eq("current", true)
            .where_eq("skipped", false)
            .where_eq("output_json", DatabaseValue::Null)
            .where_not_in(
                "identity",
                super::runs::collected_terminal_activations(tx, run_id),
            )
            .limit(1)
            .execute(tx)
            .await?
            .is_empty()
        {
            return Err(Error::UnsupportedLifecycle);
        }
        schema(&boundary.output.schema, result)?;
        tx.update(GRAPHS)
            .value("terminal_json", encode(result)?)
            .where_eq("run_id", run_id)
            .execute(tx)
            .await?;
    }
    Ok(Settlement {
        duplicate,
        graph_output: terminal,
    })
}

/// Return a bounded initial-revision page of ready activation inputs.
/// # Errors
/// Rejects unsupported revisions, invalid bounds or malformed durable state.
pub async fn ready(
    db: &dyn Database,
    run: &str,
    revision: u64,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<Activation>, Error> {
    if revision == 0 || limit == 0 || limit > 100 {
        return Err(Error::InvalidAdmission);
    }
    let tx = db.begin_transaction().await?;
    super::verified(&*tx).await?;
    if super::graph_pages::revision(&*tx, run).await? != revision {
        return Err(Error::InvalidAdmission);
    }
    let result = ready_in(&*tx, run, revision, after, limit).await?;
    tx.commit().await?;
    Ok(result)
}
async fn ready_in(
    tx: &dyn switchy_database::DatabaseTransaction,
    run: &str,
    revision: u64,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<Activation>, Error> {
    if restart::pending(tx, run).await? {
        return Ok(Vec::new());
    }
    let mut query = tx
        .select(ACTIVATIONS)
        .where_eq("run_id", run)
        .where_eq("current", true)
        .where_eq("skipped", false)
        .where_eq("output_json", DatabaseValue::Null)
        .sort("node_id", SortDirection::Asc)
        .limit(usize::from(limit));
    if let Some(after) = after {
        query = query.where_gt("node_id", after);
    }
    let mut result = Vec::new();
    for row in query.execute(tx).await? {
        result.push(Activation {
            node_id: text(&row, "node_id")?,
            activation_id: text(&row, "identity")?,
            created_at_ms: creation_time(&row)?,
            generation: match row.get("generation") {
                Some(DatabaseValue::Int64(n)) if n > 0 => {
                    u64::try_from(n).map_err(|_| Error::MaintenanceRequired)?
                }
                _ => return Err(Error::MaintenanceRequired),
            },
            revision,
            input: decode(&row, "input_json")?,
            output: None,
        });
    }
    Ok(result)
}

/// Read the current canonical activation identity without deriving it from graph revision.
/// # Errors
/// Returns database or malformed-row errors.
pub async fn activation_id(
    db: &dyn Database,
    run: &str,
    node: &str,
) -> Result<Option<String>, Error> {
    activation(db, run, node)
        .await?
        .map(|row| text(&row, "identity"))
        .transpose()
}
/// Check the exact current activation before reserving external work.
/// # Errors
/// Returns database or malformed-row errors.
pub async fn is_ready_activation(
    db: &dyn Database,
    run: &str,
    node: &str,
    id: &str,
) -> Result<bool, Error> {
    Ok(activation_id(db, run, node).await?.as_deref() == Some(id)
        && is_ready(db, run, node).await?)
}

async fn successful_attempt(
    db: &dyn Database,
    run_id: &str,
    node: &str,
    identity: &str,
) -> Result<(), Error> {
    let rows = db
        .select("workflow_attempts_v2")
        .where_eq("run_id", run_id)
        .where_eq("node_id", node)
        .where_eq("activation_id", identity)
        .sort("attempt_number", SortDirection::Desc)
        .limit(1)
        .execute(db)
        .await?;
    let attempt = rows.first().ok_or(Error::InvalidAdmission)?;
    if attempt.get("outcome_json")
        != Some(DatabaseValue::String(encode(
            &super::attempts::TerminalOutcome::Succeeded,
        )?))
        || attempt.get("receipt_json") == Some(DatabaseValue::Null)
    {
        return Err(Error::InvalidAdmission);
    }
    Ok(())
}

/// Resolve a canonical graph node or an exact persisted virtual fan-out member.
/// # Errors
/// Rejects missing identity or malformed virtual-node state; never guesses from a name prefix.
pub async fn execution_node(
    db: &dyn Database,
    run: &str,
    node: &str,
) -> Result<bcode_workflow::NodeDefinition, Error> {
    let revision = super::graph_pages::revision(db, run).await?;
    if let Some(node) = super::graph_pages::node(db, run, revision, node).await? {
        return Ok(node);
    }
    fanout::member_node(db, run, node)
        .await?
        .ok_or(Error::InvalidAdmission)
}

/// Read canonical ready activations and exact nodes in one revision-pinned bounded snapshot.
/// # Errors
/// Rejects invalid bounds, missing node bindings or incompatible durable state.
pub async fn ready_nodes(
    db: &dyn Database,
    run: &str,
    after: Option<&str>,
    limit: u16,
) -> Result<(u64, Vec<(Activation, bcode_workflow::NodeDefinition)>), Error> {
    if limit == 0 || limit > 100 {
        return Err(Error::InvalidAdmission);
    }
    let tx = db.begin_transaction().await?;
    super::verified(&*tx).await?;
    let revision = super::graph_pages::revision(&*tx, run).await?;
    let mut result = Vec::new();
    for activation in ready_in(&*tx, run, revision, after, limit).await? {
        let node = execution_node(&*tx, run, &activation.node_id).await?;
        result.push((activation, node));
    }
    tx.commit().await?;
    Ok((revision, result))
}

/// Snapshot revalidation outcome for a previously observed activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationValidity {
    /// Exact current activation remains ready.
    Ready,
    /// Exact observed activation has completed or is explicitly skipped.
    Settled,
    /// The identity is historical; a newer generation or graph revision superseded it.
    Superseded,
}
/// Revalidate one observed identity without conflating stale work with corrupt input.
/// # Errors
/// Rejects an unknown identity rather than guessing it was previously valid.
pub async fn revalidate(
    db: &dyn Database,
    run: &str,
    node: &str,
    id: &str,
) -> Result<ActivationValidity, Error> {
    let rows = db
        .select(ACTIVATIONS)
        .columns(&["current", "skipped", "output_json"])
        .where_eq("run_id", run)
        .where_eq("node_id", node)
        .where_eq("identity", id)
        .limit(1)
        .execute(db)
        .await?;
    let row = rows.first().ok_or(Error::InvalidAdmission)?;
    if row.get("current") == Some(DatabaseValue::Int64(0)) {
        return Ok(ActivationValidity::Superseded);
    }
    if row.get("skipped") == Some(DatabaseValue::Int64(1))
        || row.get("output_json") != Some(DatabaseValue::Null)
    {
        return Ok(ActivationValidity::Settled);
    }
    Ok(ActivationValidity::Ready)
}
