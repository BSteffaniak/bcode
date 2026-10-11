//! Atomic receipt-backed completion and canonical output lookup.
use super::{Database, DatabaseValue, Error, FilterableQuery, TransactionMode, decode, encode};
use crate::{ValidatedOutput, WorkflowExecutionAuthority};
use switchy_database::schema::{DataType, create_index, create_table};
const OUTPUTS: &str = "workflow_outputs_v2";
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(OUTPUTS)
        .column(super::super::column("output_id", DataType::Text, false))
        .primary_key("output_id")
        .column(super::super::column("run_id", DataType::Text, false))
        .column(super::super::column("activation_id", DataType::Text, false))
        .column(super::super::column("output_json", DataType::Text, false))
        .execute(db)
        .await?;
    create_index("idx_workflow_outputs_v2_activation")
        .table(OUTPUTS)
        .columns(vec!["run_id", "activation_id"])
        .unique(true)
        .execute(db)
        .await?;
    Ok(())
}
/// Commit successful attempt, release resources and settle canonical output in one transaction.
/// # Errors
/// Rejects mismatched output/attempt/schema/owner, conflicting duplicates, or any settlement failure.
/// All attempt, resource and graph changes roll back together on error.
pub async fn complete(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    revision: u64,
    dispatch_identity: &str,
    output: &ValidatedOutput,
) -> Result<super::Settlement, Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    let result = complete_in(&*tx, owner, revision, dispatch_identity, output).await?;
    tx.commit().await?;
    Ok(result)
}
pub(super) async fn complete_in(
    tx: &dyn switchy_database::DatabaseTransaction,
    owner: &WorkflowExecutionAuthority,
    revision: u64,
    dispatch_identity: &str,
    output: &ValidatedOutput,
) -> Result<super::Settlement, Error> {
    crate::validate_output(output).map_err(|_| Error::InvalidAdmission)?;
    if output.artifact_reference.is_some() {
        return Err(Error::UnsupportedLifecycle);
    }
    super::super::verified(tx).await?;
    if duplicate_completion(tx, owner, dispatch_identity, output).await? {
        return Ok(super::Settlement {
            duplicate: true,
            graph_output: None,
        });
    }
    let current = super::super::graph_pages::revision(tx, &output.run_id).await?;
    if current != revision
        || super::activation_id(tx, &output.run_id, &output.node_id)
            .await?
            .as_ref()
            != Some(&output.activation_id)
    {
        return Err(Error::InvalidAdmission);
    }
    let node = super::execution_node(tx, &output.run_id, &output.node_id).await?;
    if output.schema_id != node.output.type_name || output.schema_version != 1 {
        return Err(Error::InvalidAdmission);
    }
    let rows = tx
        .select("workflow_attempts_v2")
        .where_eq("dispatch_identity", dispatch_identity)
        .limit(1)
        .execute(tx)
        .await?;
    let row = rows.first().ok_or(Error::InvalidAdmission)?;
    let prepared: crate::PreparedAttempt = decode(row, "prepared_json")?;
    if prepared.run_id != output.run_id
        || prepared.node_id != output.node_id
        || prepared.activation_id != output.activation_id
    {
        return Err(Error::InvalidAdmission);
    }
    let json = encode(output)?;
    let existing = tx
        .select(OUTPUTS)
        .where_eq("output_id", output.output_id.as_str())
        .limit(1)
        .execute(tx)
        .await?;
    if let Some(row) = existing.first()
        && row.get("output_json") != Some(DatabaseValue::String(json.clone()))
    {
        return Err(Error::InvalidAdmission);
    }
    super::super::attempts::settle_in(
        tx,
        owner,
        &output.run_id,
        dispatch_identity,
        super::super::attempts::TerminalOutcome::Succeeded,
        output.created_at_ms,
    )
    .await?;
    let result = super::settle_output_in(
        tx,
        owner,
        &output.run_id,
        &output.node_id,
        revision,
        &output.value,
    )
    .await?;
    if existing.is_empty() {
        tx.insert(OUTPUTS)
            .value("output_id", output.output_id.as_str())
            .value("run_id", output.run_id.as_str())
            .value("activation_id", output.activation_id.as_str())
            .value("output_json", json)
            .execute(tx)
            .await?;
    }
    Ok(result)
}
/// Read one exact committed normalized output without replay or authority inference.
/// # Errors
/// Rejects unknown contracts, malformed output or database failure.
pub async fn output(
    db: &dyn Database,
    run_id: &str,
    id: &str,
) -> Result<Option<ValidatedOutput>, Error> {
    let tx = db.begin_transaction().await?;
    super::super::verified(&*tx).await?;
    let rows = tx
        .select(OUTPUTS)
        .where_eq("run_id", run_id)
        .where_eq("output_id", id)
        .limit(1)
        .execute(&*tx)
        .await?;
    let value = rows
        .first()
        .map(|row| decode(row, "output_json"))
        .transpose()?;
    tx.commit().await?;
    Ok(value)
}

pub(super) async fn retain_control(
    db: &dyn Database,
    run: &str,
    node: &bcode_workflow::NodeDefinition,
    id: &str,
    value: &serde_json::Value,
) -> Result<(), Error> {
    let output = ValidatedOutput {
        output_id: format!("{id}:control-output"),
        run_id: run.into(),
        node_id: node.id.clone(),
        activation_id: id.into(),
        schema_id: node.output.type_name.clone(),
        schema_version: 1,
        value: value.clone(),
        artifact_reference: None,
        created_at_ms: u64::try_from(super::super::runs::timestamp()?)
            .map_err(|_| Error::InvalidAdmission)?,
    };
    let rows = db
        .select(OUTPUTS)
        .where_eq("run_id", run)
        .where_eq("activation_id", id)
        .limit(1)
        .execute(db)
        .await?;
    if let Some(row) = rows.first() {
        let old: ValidatedOutput = decode(row, "output_json")?;
        if old.value != *value {
            return Err(Error::InvalidAdmission);
        }
        return Ok(());
    }
    db.insert(OUTPUTS)
        .value("output_id", output.output_id.as_str())
        .value("run_id", run)
        .value("activation_id", id)
        .value("output_json", encode(&output)?)
        .execute(db)
        .await?;
    Ok(())
}

/// Read a verified canonical terminal output reference from the current exit activation.
/// # Errors
/// Rejects unknown graph/output schemas or conflicting terminal values.
pub async fn terminal(db: &dyn Database, run: &str) -> Result<Option<ValidatedOutput>, Error> {
    let tx = db.begin_transaction().await?;
    let result = terminal_in(&*tx, run).await?;
    tx.commit().await?;
    Ok(result)
}
/// Read the validated terminal output in the caller's read snapshot.
/// # Errors
/// Rejects unsupported representation, inconsistent output or query failures.
pub async fn terminal_in(
    tx: &dyn switchy_database::DatabaseTransaction,
    run: &str,
) -> Result<Option<ValidatedOutput>, Error> {
    super::super::verified(tx).await?;
    let revision = super::super::graph_pages::revision(tx, run).await?;
    let definition = super::super::graph_edges::boundary(tx, run, revision).await?;
    let Some(exit) = definition.exits.first() else {
        return Err(Error::MaintenanceRequired);
    };
    let Some(id) = super::activation_id(tx, run, exit).await? else {
        return Ok(None);
    };
    let rows = tx
        .select(OUTPUTS)
        .where_eq("run_id", run)
        .where_eq("activation_id", id)
        .limit(1)
        .execute(tx)
        .await?;
    let value: Option<ValidatedOutput> = rows
        .first()
        .map(|row| decode(row, "output_json"))
        .transpose()?;
    if let Some(value) = &value {
        super::schema(&definition.output.schema, &value.value)?;
        if super::output(tx, run, exit).await?.as_ref() != Some(&value.value) {
            return Err(Error::MaintenanceRequired);
        }
    }
    Ok(value)
}

/// Reconcile an exact child result and parent receipt in one parent-authority transaction.
/// # Errors
/// Rejects mismatched lineage, absent parent receipt, stale authority, invalid output or storage failure.
pub async fn reconcile_child(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run: &str,
    node: &str,
    activation: &str,
    attempt: u32,
    terminal_at_ms: u64,
) -> Result<Option<super::Settlement>, Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    let result =
        super::super::hierarchy::child_result_in(&*tx, owner, run, node, activation, attempt)
            .await?;
    let dispatch = crate::dispatch_identity(run, node, activation, attempt);
    let settled = match result {
        None | Some(super::super::hierarchy::ChildResult::Pending) => None,
        Some(super::super::hierarchy::ChildResult::Completed(value)) => {
            let revision = super::super::graph_pages::revision(&*tx, run).await?;
            let target = super::super::graph_pages::node(&*tx, run, revision, node)
                .await?
                .ok_or(Error::InvalidAdmission)?;
            let output = ValidatedOutput {
                output_id: format!("{activation}:child-output"),
                run_id: run.into(),
                node_id: node.into(),
                activation_id: activation.into(),
                schema_id: target.output.type_name.clone(),
                schema_version: 1,
                value,
                artifact_reference: None,
                created_at_ms: terminal_at_ms,
            };
            Some(complete_in(&*tx, owner, revision, &dispatch, &output).await?)
        }
        Some(outcome) => {
            let outcome = match outcome {
                super::super::hierarchy::ChildResult::Failed => {
                    super::super::attempts::TerminalOutcome::Failed
                }
                super::super::hierarchy::ChildResult::Cancelled => {
                    super::super::attempts::TerminalOutcome::Cancelled
                }
                _ => return Err(Error::MaintenanceRequired),
            };
            super::super::attempts::settle_in(&*tx, owner, run, &dispatch, outcome, terminal_at_ms)
                .await?;
            None
        }
    };
    tx.commit().await?;
    Ok(settled)
}

/// Page exact canonical outputs in stable identity order, without replaying graph history.
/// # Errors
/// Rejects invalid bounds, unsupported representation, malformed output or database failure.
pub async fn outputs(
    db: &dyn Database,
    run: &str,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<ValidatedOutput>, Error> {
    if limit == 0 || limit > 100 {
        return Err(Error::InvalidAdmission);
    }
    let tx = db.begin_transaction().await?;
    let results = outputs_in(&*tx, run, after, limit).await?;
    tx.commit().await?;
    Ok(results)
}

/// Read bounded validated outputs inside the caller's read snapshot.
/// # Errors
/// Rejects invalid bounds, unsupported representation, inconsistent outputs or query failures.
pub async fn outputs_in(
    tx: &dyn switchy_database::DatabaseTransaction,
    run: &str,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<ValidatedOutput>, Error> {
    if limit == 0 || limit > 100 {
        return Err(Error::InvalidAdmission);
    }
    super::super::verified(tx).await?;
    let mut query = tx
        .select(OUTPUTS)
        .where_eq("run_id", run)
        .sort("output_id", switchy_database::query::SortDirection::Asc)
        .limit(usize::from(limit));
    if let Some(after) = after {
        query = query.where_gt("output_id", after);
    }
    let mut results = Vec::new();
    for row in query.execute(tx).await? {
        let value: ValidatedOutput = decode(&row, "output_json")?;
        if row.get("output_id") != Some(DatabaseValue::String(value.output_id.clone()))
            || value.run_id != run
        {
            return Err(Error::MaintenanceRequired);
        }
        crate::validate_output(&value).map_err(|_| Error::MaintenanceRequired)?;
        results.push(value);
    }
    Ok(results)
}

async fn duplicate_completion(
    tx: &dyn switchy_database::DatabaseTransaction,
    owner: &WorkflowExecutionAuthority,
    dispatch: &str,
    output: &ValidatedOutput,
) -> Result<bool, Error> {
    let roots = tx
        .select(super::RUNS)
        .columns(&["authority_json"])
        .where_eq("run_id", output.run_id.as_str())
        .limit(1)
        .execute(tx)
        .await?;
    if roots.first().and_then(|row| row.get("authority_json"))
        != Some(DatabaseValue::String(encode(owner)?))
    {
        return Err(Error::AuthorityConflict);
    }
    let rows = tx
        .select(OUTPUTS)
        .where_eq("output_id", output.output_id.as_str())
        .limit(1)
        .execute(tx)
        .await?;
    let Some(row) = rows.first() else {
        return Ok(false);
    };
    let old: ValidatedOutput = decode(row, "output_json")?;
    let mut semantic = output.clone();
    semantic.created_at_ms = old.created_at_ms;
    if old != semantic {
        return Err(Error::InvalidAdmission);
    }
    let attempts = tx
        .select("workflow_attempts_v2")
        .where_eq("dispatch_identity", dispatch)
        .limit(1)
        .execute(tx)
        .await?;
    let attempt = attempts.first().ok_or(Error::MaintenanceRequired)?;
    let prepared: crate::PreparedAttempt = decode(attempt, "prepared_json")?;
    if prepared.dispatch_identity() != dispatch
        || prepared.run_id != old.run_id
        || prepared.node_id != old.node_id
        || prepared.activation_id != old.activation_id
        || attempt.get("outcome_json")
            != Some(DatabaseValue::String(encode(
                &super::super::attempts::TerminalOutcome::Succeeded,
            )?))
        || attempt.get("receipt_json") == Some(DatabaseValue::Null)
    {
        return Err(Error::InvalidAdmission);
    }
    Ok(true)
}
