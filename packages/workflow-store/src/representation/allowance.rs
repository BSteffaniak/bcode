//! Authorized monotonic allowance overrides; immutable admission remains unchanged.
use super::{Database, DatabaseValue, RUNS, RepresentationError as Error, column};
use crate::{NewWorkflowRun, WorkflowExecutionAuthority, WorkflowRunLimits};
use switchy_database::{
    TransactionMode,
    query::FilterableQuery,
    schema::{DataType, create_table},
};
const OVERRIDES: &str = "workflow_allowance_overrides_v2";
const CHANGES: &str = "workflow_allowance_changes_v2";
/// Exact authorized extension; absolute limits make retries unambiguous.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AllowanceExtension {
    pub run_id: String,
    pub mutation_id: String,
    pub expected_graph_revision: u64,
    pub node_execution_cap: u64,
    pub retry_cap: u32,
}
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(OVERRIDES)
        .column(column("run_id", DataType::Text, false))
        .primary_key("run_id")
        .column(column("version", DataType::Int, false))
        .column(column("node_execution_cap", DataType::BigInt, false))
        .column(column("retry_cap", DataType::BigInt, false))
        .execute(db)
        .await?;
    create_table(CHANGES)
        .column(column("run_id", DataType::Text, false))
        .column(column("mutation_id", DataType::Text, false))
        .unique_columns(["run_id", "mutation_id"])
        .column(column("request_json", DataType::Text, false))
        .execute(db)
        .await?;
    Ok(())
}
/// Apply a known override to immutable admission policy within caller's read/write snapshot.
/// # Errors
/// Rejects unknown versions, narrowing/corrupt overrides and database failure.
pub async fn effective_limits(
    db: &dyn Database,
    run_id: &str,
    baseline: &WorkflowRunLimits,
) -> Result<WorkflowRunLimits, Error> {
    let rows = db
        .select(OVERRIDES)
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(db)
        .await?;
    let mut limits = baseline.clone();
    if let Some(row) = rows.first() {
        if row.get("version") != Some(DatabaseValue::Int64(1)) {
            return Err(Error::MaintenanceRequired);
        }
        let cap = number(row, "node_execution_cap")?;
        let retries =
            u32::try_from(number(row, "retry_cap")?).map_err(|_| Error::MaintenanceRequired)?;
        if cap < baseline.node_execution_cap || retries < baseline.retry_cap {
            return Err(Error::MaintenanceRequired);
        }
        limits.node_execution_cap = cap;
        limits.retry_cap = retries;
    }
    Ok(limits)
}
fn number(row: &switchy_database::Row, key: &str) -> Result<u64, Error> {
    match row.get(key) {
        Some(DatabaseValue::Int64(n)) if n >= 0 => Ok(n.cast_unsigned()),
        _ => Err(Error::MaintenanceRequired),
    }
}
async fn root(db: &dyn Database, id: &str) -> Result<switchy_database::Row, Error> {
    db.select(RUNS)
        .where_eq("run_id", id)
        .limit(1)
        .execute(db)
        .await?
        .into_iter()
        .next()
        .ok_or(Error::InvalidAdmission)
}
fn admission(row: &switchy_database::Row) -> Result<NewWorkflowRun, Error> {
    match row.get("admission_json") {
        Some(DatabaseValue::String(json)) => {
            serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)
        }
        _ => Err(Error::MaintenanceRequired),
    }
}
/// Extend execution/retry allowance under exact current ownership and graph revision.
///
/// Caller authorizes this control before invocation. Does not reopen terminal runs,
/// clear cancellation/recovery or pause, change admission history, or reset counters.
/// # Errors
/// Rejects stale owner/revision, conflicting idempotency, narrowing limits or inherited-policy violation.
pub async fn extend(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    request: &AllowanceExtension,
) -> Result<(), Error> {
    for id in [&request.run_id, &request.mutation_id] {
        crate::validate_id("allowance identity", id).map_err(|_| Error::InvalidAdmission)?;
    }
    let cap = i64::try_from(request.node_execution_cap).map_err(|_| Error::InvalidAdmission)?;
    if cap == 0 {
        return Err(Error::InvalidAdmission);
    }
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::verified(&*tx).await?;
    let row = root(&*tx, &request.run_id).await?;
    if row.get("authority_json")
        != Some(DatabaseValue::String(
            serde_json::to_string(owner).map_err(|_| Error::InvalidAdmission)?,
        ))
    {
        return Err(Error::AuthorityConflict);
    }
    let encoded = serde_json::to_string(request).map_err(|_| Error::InvalidAdmission)?;
    let prior = tx
        .select(CHANGES)
        .where_eq("run_id", request.run_id.as_str())
        .where_eq("mutation_id", request.mutation_id.as_str())
        .limit(1)
        .execute(&*tx)
        .await?;
    if let Some(prior) = prior.first() {
        if prior.get("request_json") != Some(DatabaseValue::String(encoded)) {
            return Err(Error::InvalidAdmission);
        }
        tx.commit().await?;
        return Ok(());
    }
    if row.get("status") != Some(DatabaseValue::String("paused".into()))
        || super::graph_pages::revision(&*tx, &request.run_id).await?
            != request.expected_graph_revision
    {
        return Err(Error::InvalidAdmission);
    }
    let current = effective_limits(&*tx, &request.run_id, &admission(&row)?.limits).await?;
    if request.node_execution_cap < current.node_execution_cap
        || request.retry_cap < current.retry_cap
    {
        return Err(Error::InvalidAdmission);
    }
    validate_parent(&*tx, request).await?;
    let existing = tx
        .select(OVERRIDES)
        .columns(&["run_id"])
        .where_eq("run_id", request.run_id.as_str())
        .limit(1)
        .execute(&*tx)
        .await?;
    if existing.is_empty() {
        tx.insert(OVERRIDES)
            .value("run_id", request.run_id.as_str())
            .value("version", 1_i64)
            .value("node_execution_cap", cap)
            .value("retry_cap", i64::from(request.retry_cap))
            .execute(&*tx)
            .await?;
    } else {
        tx.update(OVERRIDES)
            .value("node_execution_cap", cap)
            .value("retry_cap", i64::from(request.retry_cap))
            .where_eq("run_id", request.run_id.as_str())
            .execute(&*tx)
            .await?;
    }
    tx.insert(CHANGES)
        .value("run_id", request.run_id.as_str())
        .value("mutation_id", request.mutation_id.as_str())
        .value("request_json", encoded)
        .execute(&*tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
/// Apply the existing product execution-allowance action, without increasing retry policy.
/// Returns true for a committed new grant, false for an exact already-applied grant.
/// # Errors
/// Rejects stale owner/cap, terminal/cancelled/recovery state, inherited limits or conflicting grants.
pub async fn increase_execution_allowance(
    db: &dyn Database,
    run_id: &str,
    owner: &WorkflowExecutionAuthority,
    expected_cap: u64,
    target_cap: u64,
    now_ms: u64,
) -> Result<bool, Error> {
    crate::validate_id("run_id", run_id).map_err(|_| Error::InvalidAdmission)?;
    if expected_cap == 0 || target_cap <= expected_cap {
        return Err(Error::InvalidAdmission);
    }
    let target = i64::try_from(target_cap).map_err(|_| Error::InvalidAdmission)?;
    i64::try_from(now_ms).map_err(|_| Error::InvalidAdmission)?;
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::verified(&*tx).await?;
    let row = root(&*tx, run_id).await?;
    if row.get("authority_json")
        != Some(DatabaseValue::String(
            serde_json::to_string(owner).map_err(|_| Error::InvalidAdmission)?,
        ))
    {
        return Err(Error::AuthorityConflict);
    }
    if row.get("status") != Some(DatabaseValue::String("paused".into()))
        || super::safety::cancellation_requested_at(&*tx, run_id)
            .await?
            .is_some()
        || super::safety::recovery_required(&*tx, run_id).await?
    {
        return Err(Error::InvalidAdmission);
    }
    let current = effective_limits(&*tx, run_id, &admission(&row)?.limits).await?;
    let mutation = format!("execution-allowance:{expected_cap}:{target_cap}");
    let evidence =
        serde_json::json!({"version":1,"previous_cap":expected_cap,"target_cap":target_cap})
            .to_string();
    let prior = tx
        .select(CHANGES)
        .where_eq("run_id", run_id)
        .where_eq("mutation_id", mutation.as_str())
        .limit(1)
        .execute(&*tx)
        .await?;
    if current.node_execution_cap == target_cap {
        if prior.first().and_then(|r| r.get("request_json"))
            != Some(DatabaseValue::String(evidence))
        {
            return Err(Error::InvalidAdmission);
        }
        tx.commit().await?;
        return Ok(false);
    }
    if current.node_execution_cap != expected_cap || !prior.is_empty() {
        return Err(Error::InvalidAdmission);
    }
    let request = AllowanceExtension {
        run_id: run_id.into(),
        mutation_id: mutation.clone(),
        expected_graph_revision: super::graph_pages::revision(&*tx, run_id).await?,
        node_execution_cap: target_cap,
        retry_cap: current.retry_cap,
    };
    validate_parent(&*tx, &request).await?;
    let rows = tx
        .select(OVERRIDES)
        .columns(&["run_id"])
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(&*tx)
        .await?;
    if rows.is_empty() {
        tx.insert(OVERRIDES)
            .value("run_id", run_id)
            .value("version", 1_i64)
            .value("node_execution_cap", target)
            .value("retry_cap", i64::from(current.retry_cap))
            .execute(&*tx)
            .await?;
    } else {
        tx.update(OVERRIDES)
            .value("node_execution_cap", target)
            .where_eq("run_id", run_id)
            .execute(&*tx)
            .await?;
    }
    tx.insert(CHANGES)
        .value("run_id", run_id)
        .value("mutation_id", mutation)
        .value("request_json", evidence)
        .execute(&*tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

async fn validate_parent(db: &dyn Database, request: &AllowanceExtension) -> Result<(), Error> {
    let rows = db
        .select("workflow_lineage_v2")
        .columns(&["parent_run_id"])
        .where_eq("run_id", request.run_id.as_str())
        .limit(1)
        .execute(db)
        .await?;
    match rows.first().and_then(|r| r.get("parent_run_id")) {
        Some(DatabaseValue::Null) => Ok(()),
        Some(DatabaseValue::String(id)) => {
            let parent = root(db, &id).await?;
            let limits = effective_limits(db, &id, &admission(&parent)?.limits).await?;
            if request.node_execution_cap > limits.node_execution_cap
                || request.retry_cap > limits.retry_cap
            {
                return Err(Error::InvalidAdmission);
            }
            Ok(())
        }
        _ => Err(Error::MaintenanceRequired),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    async fn assert_product_allowance(
        db: &dyn Database,
        owner: &WorkflowExecutionAuthority,
        run: &NewWorkflowRun,
    ) {
        let before = root(db, &run.run_id).await.unwrap();
        assert!(
            increase_execution_allowance(db, &run.run_id, owner, 2, 3, 10)
                .await
                .unwrap()
        );
        assert!(
            !increase_execution_allowance(db, &run.run_id, owner, 2, 3, 11)
                .await
                .unwrap()
        );
        assert!(
            increase_execution_allowance(db, &run.run_id, owner, 2, 4, 12)
                .await
                .is_err()
        );
        assert!(
            increase_execution_allowance(db, &run.run_id, owner, 1, 3, 12)
                .await
                .is_err()
        );
        let effective = effective_limits(db, &run.run_id, &run.limits)
            .await
            .unwrap();
        assert_eq!(effective.node_execution_cap, 3);
        assert_eq!(effective.retry_cap, 1);
        assert_eq!(root(db, &run.run_id).await.unwrap().columns, before.columns);
    }

    #[tokio::test]
    async fn extension_preserves_admission_and_consumed_budget_and_is_fenced() {
        let db = super::super::tests::database().await;
        super::super::install(&*db).await.unwrap();
        let definition = bcode_workflow::WorkflowBuilder::new(
            "allowance",
            bcode_workflow::Step::task("work", |v: u32, _| async move { Ok(v) }),
        )
        .build()
        .unwrap()
        .definition()
        .clone();
        let identity =
            bcode_workflow::WorkflowDefinitionIdentity::for_definition("allowance", &definition)
                .unwrap();
        let mut run = super::super::tests::run("allowance", false);
        run.definition_id = identity.definition_id;
        run.definition_version = identity.definition_version;
        run.limits.node_execution_cap = 1;
        run.limits.retry_cap = 0;
        super::super::admit(&*db, &run, &definition).await.unwrap();
        let owner = WorkflowExecutionAuthority {
            target_artifact_id: "a".into(),
            daemon_instance_id: "d".into(),
            generation: 1,
            fencing_token: "f".into(),
        };
        super::super::attempts::claim_authority(&*db, &run.run_id, &owner)
            .await
            .unwrap();
        let tx = db
            .begin_transaction_with_mode(TransactionMode::Immediate)
            .await
            .unwrap();
        super::super::hierarchy::consume_attempt(&*tx, &run.run_id)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let change = AllowanceExtension {
            run_id: run.run_id.clone(),
            mutation_id: "more".into(),
            expected_graph_revision: 1,
            node_execution_cap: 2,
            retry_cap: 1,
        };
        let stale = WorkflowExecutionAuthority {
            generation: 2,
            ..owner.clone()
        };
        assert!(extend(&*db, &stale, &change).await.is_err());
        extend(&*db, &owner, &change).await.unwrap();
        extend(&*db, &owner, &change).await.unwrap();
        let wrong_revision = AllowanceExtension {
            mutation_id: "wrong-revision".into(),
            expected_graph_revision: 2,
            ..change.clone()
        };
        assert!(extend(&*db, &owner, &wrong_revision).await.is_err());
        let narrowing = AllowanceExtension {
            mutation_id: "narrow".into(),
            node_execution_cap: 1,
            ..change.clone()
        };
        assert!(extend(&*db, &owner, &narrowing).await.is_err());
        let conflict = AllowanceExtension {
            retry_cap: 2,
            ..change.clone()
        };
        assert!(extend(&*db, &owner, &conflict).await.is_err());
        assert_eq!(
            admission(&root(&*db, &run.run_id).await.unwrap())
                .unwrap()
                .limits,
            run.limits
        );
        let effective = effective_limits(&*db, &run.run_id, &run.limits)
            .await
            .unwrap();
        assert_eq!(effective.node_execution_cap, 2);
        assert_eq!(effective.retry_cap, 1);
        let tx = db
            .begin_transaction_with_mode(TransactionMode::Immediate)
            .await
            .unwrap();
        super::super::hierarchy::consume_attempt(&*tx, &run.run_id)
            .await
            .unwrap();
        assert!(
            super::super::hierarchy::consume_attempt(&*tx, &run.run_id)
                .await
                .is_err()
        );
        tx.commit().await.unwrap();
        assert_product_allowance(&*db, &owner, &run).await;
    }
}
