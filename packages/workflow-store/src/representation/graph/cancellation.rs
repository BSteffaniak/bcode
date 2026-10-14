//! Exact never-prepared cancellation classification under the caller's write transaction.
use super::{DatabaseValue, Error, FilterableQuery};
use switchy_database::DatabaseTransaction;

/// Persist cancellation intent and skip only activations with no durable attempt whatsoever.
///
/// Caller must hold current run authority and an immediate transaction. Prepared intents without
/// receipts remain ambiguous work; they are never classified as skipped or stripped of resources.
/// # Errors
/// Rejects missing activation, inconsistent identity or database failure.
pub(super) async fn cancel_in(
    tx: &dyn DatabaseTransaction,
    run: &str,
    id: &str,
) -> Result<bool, Error> {
    let rows = tx
        .select(super::ACTIVATIONS)
        .where_eq("run_id", run)
        .where_eq("identity", id)
        .limit(1)
        .execute(tx)
        .await?;
    let row = rows.first().ok_or(Error::InvalidAdmission)?;
    if row.get("output_json") != Some(DatabaseValue::Null) {
        return Ok(false);
    }
    let intent_created = if tx
        .select("workflow_parallel_cancellations_v2")
        .where_eq("activation_id", id)
        .limit(1)
        .execute(tx)
        .await?
        .is_empty()
    {
        tx.insert("workflow_parallel_cancellations_v2")
            .value("activation_id", id)
            .value("run_id", run)
            .execute(tx)
            .await?;
        true
    } else {
        false
    };
    let attempts = tx
        .select("workflow_attempts_v2")
        .columns(&["dispatch_identity"])
        .where_eq("run_id", run)
        .where_eq("activation_id", id)
        .limit(1)
        .execute(tx)
        .await?;
    if !attempts.is_empty() {
        return Ok(intent_created);
    }
    if row.get("skipped") == Some(DatabaseValue::Int64(1)) {
        return Ok(intent_created);
    }
    super::terminal_scan::invalidate(tx, run).await?;
    tx.update(super::ACTIVATIONS)
        .value("skipped", true)
        .where_eq("run_id", run)
        .where_eq("identity", id)
        .execute(tx)
        .await?;
    Ok(true)
}
