//! Immutable producer-recorded repeat emission outcomes; never reconstructed from cycle counters.
use super::super::{Database, DatabaseValue, Error, FilterableQuery, decode, encode};
use bcode_workflow::WorkflowRepeatOutcomeSummary;
use switchy_database::{
    DatabaseTransaction,
    query::SortDirection,
    schema::{DataType, create_index, create_table},
};
const TABLE: &str = "workflow_repeat_outcomes_v1";

pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(TABLE)
        .column(super::super::super::column(
            "output_id",
            DataType::Text,
            false,
        ))
        .primary_key("output_id")
        .column(super::super::super::column("run_id", DataType::Text, false))
        .column(super::super::super::column(
            "version",
            DataType::BigInt,
            false,
        ))
        .column(super::super::super::column(
            "summary_json",
            DataType::Text,
            false,
        ))
        .execute(db)
        .await?;
    create_index("workflow_repeat_outcomes_v1_run")
        .table(TABLE)
        .columns(vec!["run_id", "output_id"])
        .execute(db)
        .await?;
    Ok(())
}

/// Record an actual `EmitOutcome` control output in the authority-fenced settlement transaction.
/// Does not claim historical coverage or install optional state.
/// # Errors
/// Rejects mismatched output/outcome, conflicting duplicates, malformed state and storage errors.
pub async fn record_in(
    tx: &dyn DatabaseTransaction,
    owner: &crate::WorkflowExecutionAuthority,
    output: &crate::ValidatedOutput,
    outcome: &bcode_workflow::WorkflowRepeatOutcome<serde_json::Value>,
) -> Result<(), Error> {
    super::super::super::verified(tx).await?;
    let roots = tx
        .select(super::super::RUNS)
        .columns(&["authority_json"])
        .where_eq("run_id", output.run_id.as_str())
        .limit(1)
        .execute(tx)
        .await?;
    let expected = serde_json::to_string(owner).map_err(|_| Error::InvalidAdmission)?;
    if roots.first().and_then(|row| row.get("authority_json"))
        != Some(DatabaseValue::String(expected))
    {
        return Err(Error::AuthorityConflict);
    }
    if !tx.table_exists(TABLE).await? {
        return Ok(());
    }
    if outcome.version != bcode_workflow::WORKFLOW_REPEAT_OUTCOME_VERSION
        || serde_json::to_value(outcome).map_err(|_| Error::InvalidAdmission)? != output.value
    {
        return Err(Error::InvalidAdmission);
    }
    let canonical = tx
        .select("workflow_outputs_v2")
        .columns(&["output_json"])
        .where_eq("run_id", output.run_id.as_str())
        .where_eq("output_id", output.output_id.as_str())
        .limit(1)
        .execute(tx)
        .await?;
    let retained: crate::ValidatedOutput = decode(
        canonical.first().ok_or(Error::MaintenanceRequired)?,
        "output_json",
    )?;
    if retained != *output {
        return Err(Error::InvalidAdmission);
    }
    let node = super::super::execution_node(tx, &output.run_id, &output.node_id).await?;
    if node.kind != bcode_workflow::NodeKind::Repeat {
        return Err(Error::InvalidAdmission);
    }
    if outcome.iterations_completed == 0
        || outcome.effective_iteration_bound != outcome.max_iterations.min(outcome.cycle_cap)
        || outcome.iterations_completed > outcome.effective_iteration_bound
    {
        return Err(Error::InvalidAdmission);
    }
    let summary = summarize(output, outcome)?;
    let encoded = encode(&summary)?;
    let rows = tx
        .select(TABLE)
        .where_eq("output_id", output.output_id.as_str())
        .limit(1)
        .execute(tx)
        .await?;
    if let Some(row) = rows.first() {
        if row.get("version") != Some(DatabaseValue::Int64(1))
            || row.get("summary_json") != Some(DatabaseValue::String(encoded))
        {
            return Err(Error::MaintenanceRequired);
        }
        return Ok(());
    }
    super::super::super::journal::public_events::repeat::record_in(tx, &summary).await?;
    tx.insert(TABLE)
        .value("output_id", output.output_id.as_str())
        .value("run_id", output.run_id.as_str())
        .value("version", 1_i64)
        .value("summary_json", encoded)
        .execute(tx)
        .await?;
    Ok(())
}

fn summarize(
    output: &crate::ValidatedOutput,
    outcome: &bcode_workflow::WorkflowRepeatOutcome<serde_json::Value>,
) -> Result<WorkflowRepeatOutcomeSummary, Error> {
    Ok(WorkflowRepeatOutcomeSummary {
        run_id: output.run_id.clone(),
        node_id: output.node_id.clone(),
        activation_id: output.activation_id.clone(),
        output_id: output.output_id.clone(),
        outcome: outcome.outcome,
        iterations_completed: outcome.iterations_completed,
        max_iterations: outcome.max_iterations,
        cycle_cap: outcome.cycle_cap,
        effective_iteration_bound: outcome.effective_iteration_bound,
        checksum_sha256: bcode_workflow::workflow_canonical_value_sha256(&output.value)
            .map_err(|_| Error::MaintenanceRequired)?,
        created_at_ms: output.created_at_ms,
    })
}

async fn verify_summary(
    tx: &dyn DatabaseTransaction,
    summary: &WorkflowRepeatOutcomeSummary,
) -> Result<(), Error> {
    let rows = tx
        .select("workflow_outputs_v2")
        .where_eq("run_id", summary.run_id.as_str())
        .where_eq("output_id", summary.output_id.as_str())
        .limit(1)
        .execute(tx)
        .await?;
    let output: crate::ValidatedOutput = decode(
        rows.first().ok_or(Error::MaintenanceRequired)?,
        "output_json",
    )?;
    let outcome: bcode_workflow::WorkflowRepeatOutcome<serde_json::Value> =
        serde_json::from_value(output.value.clone()).map_err(|_| Error::MaintenanceRequired)?;
    if outcome.version != bcode_workflow::WORKFLOW_REPEAT_OUTCOME_VERSION
        || summarize(&output, &outcome)? != *summary
    {
        return Err(Error::MaintenanceRequired);
    }
    Ok(())
}

/// Record the already-retained canonical control output without opening a nested transaction.
/// # Errors
/// Rejects missing canonical output, malformed outcome or stale authority.
pub async fn record_control_in(
    tx: &dyn DatabaseTransaction,
    owner: &crate::WorkflowExecutionAuthority,
    run: &str,
    activation: &str,
    outcome: &bcode_workflow::WorkflowRepeatOutcome<serde_json::Value>,
) -> Result<(), Error> {
    let rows = tx
        .select("workflow_outputs_v2")
        .where_eq("run_id", run)
        .where_eq("activation_id", activation)
        .limit(2)
        .execute(tx)
        .await?;
    if rows.len() != 1 {
        return Err(Error::MaintenanceRequired);
    }
    let output: crate::ValidatedOutput = decode(&rows[0], "output_json")?;
    record_in(tx, owner, &output, outcome).await
}

/// Read a bounded actual-outcome page without inferring absence coverage from empty rows.
/// # Errors
/// Rejects missing optional state, unsupported versions, corrupt identity or invalid bounds.
pub async fn page_in(
    tx: &dyn DatabaseTransaction,
    run: &str,
    after: Option<&str>,
    limit: u16,
) -> Result<(Vec<WorkflowRepeatOutcomeSummary>, Option<String>), Error> {
    if !(1..=100).contains(&limit) {
        return Err(Error::InvalidAdmission);
    }
    super::super::super::verified(tx).await?;
    if !tx.table_exists(TABLE).await? {
        return Err(Error::MaintenanceRequired);
    }
    let mut query = tx
        .select(TABLE)
        .where_eq("run_id", run)
        .sort("output_id", SortDirection::Asc)
        .limit(usize::from(limit) + 1);
    if let Some(after) = after {
        query = query.where_gt("output_id", after);
    }
    let mut rows = query.execute(tx).await?;
    let more = rows.len() > usize::from(limit);
    rows.truncate(usize::from(limit));
    let mut summaries = Vec::new();
    for row in rows {
        if row.get("version") != Some(DatabaseValue::Int64(1)) {
            return Err(Error::MaintenanceRequired);
        }
        let summary: WorkflowRepeatOutcomeSummary = decode(&row, "summary_json")?;
        if summary.run_id != run
            || row.get("output_id") != Some(DatabaseValue::String(summary.output_id.clone()))
        {
            return Err(Error::MaintenanceRequired);
        }
        verify_summary(tx, &summary).await?;
        summaries.push(summary);
    }
    let next = if more {
        Some(
            summaries
                .last()
                .ok_or(Error::MaintenanceRequired)?
                .output_id
                .clone(),
        )
    } else {
        None
    };
    Ok((summaries, next))
}

#[cfg(test)]
mod tests;
