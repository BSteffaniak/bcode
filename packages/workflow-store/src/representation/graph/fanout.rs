//! Incremental homogeneous fan-out materialization and member capacity reservation.
use super::{
    Database, DatabaseValue, Error, FilterableQuery, SortDirection, TransactionMode, Value,
    WorkflowExecutionAuthority, decode, encode, text,
};
use switchy_database::{
    DatabaseTransaction,
    schema::{DataType, create_index, create_table},
};
const CONTROLLERS: &str = "workflow_fanout_v2";
#[cfg(test)]
mod tests;
const MEMBERS: &str = "workflow_fanout_members_v2";
const CLAIMS: &str = "workflow_fanout_claims_v2";
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    super::fanout_progress::install(db).await?;
    create_table(CONTROLLERS)
        .column(super::super::column("activation_id", DataType::Text, false))
        .primary_key("activation_id")
        .column(super::super::column("run_id", DataType::Text, false))
        .column(super::super::column("node_id", DataType::Text, false))
        .column(super::super::column("next_index", DataType::Int, false))
        .column(super::super::column("active", DataType::Int, false))
        .column(super::super::column("cap", DataType::Int, false))
        .execute(db)
        .await?;
    create_table(MEMBERS)
        .column(super::super::column("node_id", DataType::Text, false))
        .primary_key("node_id")
        .column(super::super::column("run_id", DataType::Text, false))
        .column(super::super::column("controller", DataType::Text, false))
        .column(super::super::column("member_index", DataType::Int, false))
        .column(super::super::column("node_json", DataType::Text, false))
        .execute(db)
        .await?;
    create_index("idx_workflow_fanout_members_controller")
        .table(MEMBERS)
        .columns(vec!["run_id", "controller", "member_index"])
        .execute(db)
        .await?;
    create_table(CLAIMS)
        .column(super::super::column("activation_id", DataType::Text, false))
        .primary_key("activation_id")
        .column(super::super::column("controller", DataType::Text, false))
        .execute(db)
        .await?;
    Ok(())
}
/// Materialize at most `limit` exact member activations, retaining original input order.
/// # Errors
/// Rejects stale controller/owner/revision, invalid config/input, cancellation or storage failures.
pub async fn materialize(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run: &str,
    node: &str,
    id: &str,
    revision: u64,
    limit: u16,
) -> Result<usize, Error> {
    if limit == 0 || limit > 100 {
        return Err(Error::InvalidAdmission);
    }
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::super::verified(&*tx).await?;
    let roots = tx
        .select(super::RUNS)
        .where_eq("run_id", run)
        .limit(1)
        .execute(&*tx)
        .await?;
    let root = roots.first().ok_or(Error::InvalidAdmission)?;
    if text(root, "authority_json")? != encode(owner)? {
        return Err(Error::AuthorityConflict);
    }
    let current = super::super::graph_pages::revision(&*tx, run).await?;
    if current != revision {
        return Err(Error::InvalidAdmission);
    }
    let target = super::super::graph_pages::node(&*tx, run, current, node)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    let config = configuration(&target)?;
    let activation = super::activation(&*tx, run, node)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    if text(&activation, "identity")? != id {
        return Err(Error::InvalidAdmission);
    }
    if activation.get("output_json") != Some(DatabaseValue::Null) {
        return Ok(0);
    }
    if text(root, "status")? != "paused" {
        return Err(Error::InvalidAdmission);
    }
    super::super::safety::ensure_admission_allowed(&*tx, run).await?;
    let input: Value = decode(&activation, "input_json")?;
    let items = input.as_array().ok_or(Error::InvalidAdmission)?;
    if items.len() > usize::try_from(config.max_members).map_err(|_| Error::InvalidAdmission)? {
        return Err(Error::InvalidAdmission);
    }
    let rows = tx
        .select(CONTROLLERS)
        .where_eq("activation_id", id)
        .limit(1)
        .execute(&*tx)
        .await?;
    let start = if let Some(row) = rows.first() {
        number(row, "next_index")?
    } else {
        tx.insert(CONTROLLERS)
            .value("activation_id", id)
            .value("run_id", run)
            .value("node_id", node)
            .value("next_index", 0_i64)
            .value("active", 0_i64)
            .value("cap", i64::from(config.max_concurrency))
            .execute(&*tx)
            .await?;
        0
    };
    let start = usize::try_from(start).map_err(|_| Error::MaintenanceRequired)?;
    let end = start.saturating_add(usize::from(limit)).min(items.len());
    for (index, item) in items.iter().enumerate().take(end).skip(start) {
        let name = format!(
            "fanout:{}",
            crate::sha256_hex(encode(&(run, id, index))?.as_bytes())
        );
        let mut member = *config.member_node.clone();
        member.id = name.clone();
        super::schema(&member.input.schema, item)?;
        tx.insert(MEMBERS)
            .value("node_id", name.as_str())
            .value("run_id", run)
            .value("controller", id)
            .value(
                "member_index",
                i64::try_from(index).map_err(|_| Error::InvalidAdmission)?,
            )
            .value("node_json", encode(&member)?)
            .execute(&*tx)
            .await?;
        super::insert_activation(&*tx, run, &name, item).await?;
    }
    tx.update(CONTROLLERS)
        .value(
            "next_index",
            i64::try_from(end).map_err(|_| Error::InvalidAdmission)?,
        )
        .where_eq("activation_id", id)
        .execute(&*tx)
        .await?;
    tx.commit().await?;
    Ok(end - start)
}
fn configuration(
    node: &bcode_workflow::NodeDefinition,
) -> Result<bcode_workflow::WorkflowFanOutConfiguration, Error> {
    if node.kind != bcode_workflow::NodeKind::FanOut {
        return Err(Error::InvalidAdmission);
    }
    let config: bcode_workflow::WorkflowFanOutConfiguration =
        serde_json::from_value(node.configuration.clone()).map_err(|_| Error::InvalidAdmission)?;
    config.validate().map_err(|_| Error::InvalidAdmission)?;
    Ok(config)
}

fn number(row: &switchy_database::Row, key: &str) -> Result<i64, Error> {
    match row.get(key) {
        Some(DatabaseValue::Int64(n)) if n >= 0 => Ok(n),
        _ => Err(Error::MaintenanceRequired),
    }
}
pub(super) async fn member_node(
    db: &dyn Database,
    run: &str,
    node: &str,
) -> Result<Option<bcode_workflow::NodeDefinition>, Error> {
    let rows = db
        .select(MEMBERS)
        .where_eq("run_id", run)
        .where_eq("node_id", node)
        .limit(1)
        .execute(db)
        .await?;
    rows.first().map(|row| decode(row, "node_json")).transpose()
}
/// Reserve virtual-member capacity in the same attempt transaction; ordinary nodes are unchanged.
/// # Errors
/// Rejects exhausted controller capacity, malformed lineage or conflicting storage state.
pub async fn reserve(
    tx: &dyn DatabaseTransaction,
    run: &str,
    node: &str,
    activation: &str,
) -> Result<(), Error> {
    let rows = tx
        .select(MEMBERS)
        .where_eq("run_id", run)
        .where_eq("node_id", node)
        .limit(1)
        .execute(tx)
        .await?;
    let Some(member) = rows.first() else {
        return Ok(());
    };
    let controller = text(member, "controller")?;
    if !tx
        .select(CLAIMS)
        .where_eq("activation_id", activation)
        .limit(1)
        .execute(tx)
        .await?
        .is_empty()
    {
        return Err(Error::InvalidAdmission);
    }
    let rows = tx
        .select(CONTROLLERS)
        .where_eq("activation_id", controller.as_str())
        .limit(1)
        .execute(tx)
        .await?;
    let row = rows.first().ok_or(Error::MaintenanceRequired)?;
    let active = number(row, "active")?;
    if active >= number(row, "cap")? {
        return Err(Error::CapacityUnavailable);
    }
    tx.insert(CLAIMS)
        .value("activation_id", activation)
        .value("controller", controller.as_str())
        .execute(tx)
        .await?;
    tx.update(CONTROLLERS)
        .value(
            "active",
            active.checked_add(1).ok_or(Error::CapacityUnavailable)?,
        )
        .where_eq("activation_id", controller)
        .execute(tx)
        .await?;
    Ok(())
}
/// Release member capacity only inside fenced authoritative attempt terminalization.
/// # Errors
/// Rejects inconsistent counters or storage failure.
pub async fn release(
    tx: &dyn DatabaseTransaction,
    _run: &str,
    _node: &str,
    activation: &str,
) -> Result<(), Error> {
    let claims = tx
        .select(CLAIMS)
        .where_eq("activation_id", activation)
        .limit(1)
        .execute(tx)
        .await?;
    let Some(claim) = claims.first() else {
        return Ok(());
    };
    let controller = text(claim, "controller")?;
    let rows = tx
        .select(CONTROLLERS)
        .where_eq("activation_id", controller.as_str())
        .limit(1)
        .execute(tx)
        .await?;
    let count = number(rows.first().ok_or(Error::MaintenanceRequired)?, "active")?
        .checked_sub(1)
        .filter(|n| *n >= 0)
        .ok_or(Error::MaintenanceRequired)?;
    tx.update(CONTROLLERS)
        .value("active", count)
        .where_eq("activation_id", controller)
        .execute(tx)
        .await?;
    tx.delete(CLAIMS)
        .where_eq("activation_id", activation)
        .execute(tx)
        .await?;
    Ok(())
}
/// Page canonical virtual member identities without loading all inputs.
/// # Errors
/// Rejects invalid bounds or database errors.
pub async fn members(
    db: &dyn Database,
    run: &str,
    controller: &str,
    after: Option<u32>,
    limit: u16,
) -> Result<Vec<String>, Error> {
    if limit == 0 || limit > 100 {
        return Err(Error::InvalidAdmission);
    }
    let mut query = db
        .select(MEMBERS)
        .columns(&["node_id"])
        .where_eq("run_id", run)
        .where_eq("controller", controller)
        .sort("member_index", SortDirection::Asc)
        .limit(usize::from(limit));
    if let Some(after) = after {
        query = query.where_gt("member_index", i64::from(after));
    }
    query
        .execute(db)
        .await?
        .iter()
        .map(|row| text(row, "node_id"))
        .collect()
}

/// Settle the controller only after all materialized members have canonical outputs.
///
/// Member discovery remains paged; result bytes are bounded by the normal output envelope.
/// # Errors
/// Rejects stale authority/revision, malformed ordering, invalid result schema or storage failure.
pub async fn settle(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run: &str,
    node: &str,
    id: &str,
    revision: u64,
) -> Result<bool, Error> {
    settle_with_policy(
        db,
        owner,
        run,
        node,
        id,
        revision,
        super::parallel_progress::OutputPolicy::default(),
    )
    .await
}
/// Settle with an explicit inline output byte budget, never a graph lifetime limit.
/// # Errors
/// Returns ordinary settlement errors or an exceeded output policy.
pub async fn settle_with_policy(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run: &str,
    node: &str,
    id: &str,
    revision: u64,
    output_policy: super::parallel_progress::OutputPolicy,
) -> Result<bool, Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::super::verified(&*tx).await?;
    let root = owned_root(&*tx, run, owner).await?;
    let current = super::super::graph_pages::revision(&*tx, run).await?;
    let boundary = super::super::graph_edges::boundary(&*tx, run, current).await?;
    if current != revision {
        return Err(Error::InvalidAdmission);
    }
    let target = super::super::graph_pages::node(&*tx, run, current, node)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    let activation = super::activation(&*tx, run, node)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    if text(&activation, "identity")? != id {
        return Err(Error::InvalidAdmission);
    }
    if activation.get("output_json") != Some(DatabaseValue::Null) {
        return Ok(false);
    }
    if text(&root, "status")? != "paused" {
        return Err(Error::InvalidAdmission);
    }
    super::super::safety::ensure_admission_allowed(&*tx, run).await?;
    let input: Value = decode(&activation, "input_json")?;
    let count = input.as_array().ok_or(Error::InvalidAdmission)?.len();
    let controllers = tx
        .select(CONTROLLERS)
        .where_eq("activation_id", id)
        .limit(1)
        .execute(&*tx)
        .await?;
    let controller = controllers.first().ok_or(Error::InvalidAdmission)?;
    if usize::try_from(number(controller, "next_index")?).ok() != Some(count) {
        return Ok(false);
    }
    let config: bcode_workflow::WorkflowFanOutConfiguration =
        serde_json::from_value(target.configuration.clone())
            .map_err(|_| Error::InvalidAdmission)?;
    let values = if config.failure_policy == bcode_workflow::ParallelFailurePolicy::WaitAll {
        let Some(state) =
            super::fanout_progress::reduce(&*tx, run, id, count, output_policy).await?
        else {
            tx.commit().await?;
            return Ok(false);
        };
        if !state.pending.is_empty() {
            tx.commit().await?;
            return Ok(false);
        }
        if state.failed {
            super::control_state::fail(&*tx, run, &format!("fanout:{id}")).await?;
            tx.commit().await?;
            return Ok(false);
        }
        collected_values(state)?
    } else {
        if reconcile_failure(&*tx, run, id, config.failure_policy).await? {
            tx.commit().await?;
            return Ok(false);
        }
        let Some(values) = collect_outputs(&*tx, run, id, count).await? else {
            return Ok(false);
        };
        values
    };
    let value = serde_json::to_value(
        bcode_workflow::WorkflowFanOutResult::new(values)
            .map_err(|_| Error::MaintenanceRequired)?,
    )
    .map_err(|_| Error::InvalidAdmission)?;
    if encode(&value)?.len() > output_policy.max_inline_bytes {
        return Err(Error::InvalidAdmission);
    }
    super::schema(&target.output.schema, &value)?;
    super::terminal_scan::invalidate(&*tx, run).await?;
    tx.update(super::ACTIVATIONS)
        .value("output_json", encode(&value)?)
        .where_eq("identity", id)
        .execute(&*tx)
        .await?;
    super::completion::retain_control(&*tx, run, &target, id, &value).await?;
    let request: crate::NewWorkflowRun = decode(&root, "admission_json")?;
    super::propagate_paged(&*tx, &request, current, node, &value).await?;
    if boundary.exits.iter().any(|exit| exit == node) {
        super::schema(&boundary.output.schema, &value)?;
        tx.update(super::GRAPHS)
            .value("terminal_json", encode(&value)?)
            .where_eq("run_id", run)
            .execute(&*tx)
            .await?;
    }
    tx.commit().await?;
    Ok(true)
}

fn collected_values(
    state: super::parallel::MemberState,
) -> Result<Vec<bcode_workflow::WorkflowFanOutMember<Value>>, Error> {
    state
        .outcomes
        .into_iter()
        .enumerate()
        .map(|(index, (key, outcome))| {
            let parsed: usize = key.parse().map_err(|_| Error::MaintenanceRequired)?;
            if parsed != index {
                return Err(Error::MaintenanceRequired);
            }
            let value = outcome
                .get("value")
                .cloned()
                .ok_or(Error::MaintenanceRequired)?;
            Ok(bcode_workflow::WorkflowFanOutMember {
                index: u32::try_from(index).map_err(|_| Error::InvalidAdmission)?,
                value,
            })
        })
        .collect()
}

async fn reconcile_failure(
    db: &dyn DatabaseTransaction,
    run: &str,
    id: &str,
    policy: bcode_workflow::ParallelFailurePolicy,
) -> Result<bool, Error> {
    let mut after = None;
    let mut failed = false;
    let mut pending = false;
    loop {
        let page = members(db, run, id, after, 100).await?;
        if page.is_empty() {
            break;
        }
        for member in page {
            after = Some(after.map_or(0, |n| n + 1));
            if super::output(db, run, &member).await?.is_some() {
                continue;
            }
            if super::activation(db, run, &member)
                .await?
                .is_some_and(|row| row.get("skipped") == Some(DatabaseValue::Int64(1)))
            {
                continue;
            }
            let activation = super::activation_id(db, run, &member)
                .await?
                .ok_or(Error::MaintenanceRequired)?;
            let rows = db
                .select("workflow_attempts_v2")
                .where_eq("run_id", run)
                .where_eq("activation_id", activation)
                .sort("attempt_number", SortDirection::Desc)
                .limit(1)
                .execute(db)
                .await?;
            match rows.first().and_then(|row| row.get("outcome_json")) {
                Some(DatabaseValue::String(value))
                    if value == "\"Failed\"" || value == "\"Cancelled\"" =>
                {
                    failed = true;
                }
                _ => pending = true,
            }
        }
    }
    if !failed {
        return Ok(false);
    }
    if policy == bcode_workflow::ParallelFailurePolicy::FailFast {
        let mut after = None;
        loop {
            let page = members(db, run, id, after, 100).await?;
            if page.is_empty() {
                break;
            }
            for member in page {
                after = Some(after.map_or(0, |n| n + 1));
                if super::output(db, run, &member).await?.is_some() {
                    continue;
                }
                let activation = super::activation_id(db, run, &member)
                    .await?
                    .ok_or(Error::MaintenanceRequired)?;
                super::cancellation::cancel_in(db, run, &activation).await?;
            }
        }
    }
    if !pending {
        super::control_state::fail(db, run, &format!("fanout:{id}")).await?;
    }
    Ok(true)
}

async fn collect_outputs(
    db: &dyn Database,
    run: &str,
    id: &str,
    count: usize,
) -> Result<Option<Vec<bcode_workflow::WorkflowFanOutMember<Value>>>, Error> {
    let mut values = Vec::new();
    let mut after = None;
    let mut bytes = 0_usize;
    loop {
        let page = members(db, run, id, after, 100).await?;
        if page.is_empty() {
            break;
        }
        for member in page {
            let Some(value) = super::output(db, run, &member).await? else {
                return Ok(None);
            };
            bytes = bytes
                .checked_add(encode(&value)?.len())
                .ok_or(Error::InvalidAdmission)?;
            if bytes > super::super::MAX_ADMISSION_BYTES {
                return Err(Error::InvalidAdmission);
            }
            let index = u32::try_from(values.len()).map_err(|_| Error::InvalidAdmission)?;
            values.push(bcode_workflow::WorkflowFanOutMember { index, value });
            after = Some(index);
        }
    }
    if values.len() != count {
        return Err(Error::MaintenanceRequired);
    }
    Ok(Some(values))
}

async fn owned_root(
    db: &dyn Database,
    run: &str,
    owner: &WorkflowExecutionAuthority,
) -> Result<switchy_database::Row, Error> {
    let roots = db
        .select(super::RUNS)
        .where_eq("run_id", run)
        .limit(1)
        .execute(db)
        .await?;
    let root = roots.into_iter().next().ok_or(Error::InvalidAdmission)?;
    if text(&root, "authority_json")? != encode(owner)? {
        return Err(Error::AuthorityConflict);
    }
    Ok(root)
}
