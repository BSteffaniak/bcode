//! Bounded attempt observation from the canonical new-family ledger.
use super::{Database, DatabaseValue, RepresentationError as Error};
use switchy_database::query::{FilterableQuery, SortDirection};

#[cfg(test)]
mod diagnostic_tests;

/// Read one exact immutable new-family owner receipt using structured queries.
///
/// Legacy diagnostic lookup is unavailable (`None`), never guessed from historical data.
/// This grants no execution authority and never reconciles or repairs an attempt.
/// # Errors
/// Rejects unsupported storage, malformed coordinates, oversized receipts and conflicting identities.
pub async fn diagnostic_receipt(
    db: &dyn Database,
    run: &str,
    identity: &str,
) -> Result<Option<crate::DispatchReceipt>, Error> {
    crate::validate_id("run_id", run).map_err(|_| Error::InvalidAdmission)?;
    crate::validate_id("dispatch_identity", identity).map_err(|_| Error::InvalidAdmission)?;
    super::compatible(db).await?;
    match super::runs::lookup(db, run).await? {
        super::runs::RunLookup::Legacy | super::runs::RunLookup::Missing => return Ok(None),
        super::runs::RunLookup::Available(_) => {}
        _ => return Err(Error::MaintenanceRequired),
    }
    let rows = db
        .select("workflow_attempts_v2")
        .columns(&[
            "run_id",
            "node_id",
            "activation_id",
            "attempt_number",
            "dispatch_identity",
            "receipt_json",
        ])
        .where_eq("run_id", run)
        .where_eq("dispatch_identity", identity)
        .limit(1)
        .execute(db)
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let payload = match row.get("receipt_json") {
        Some(DatabaseValue::Null) => return Ok(None),
        Some(DatabaseValue::String(value)) if value.len() <= crate::MAX_INLINE_JSON_BYTES => value,
        _ => return Err(Error::MaintenanceRequired),
    };
    let text = |key| match row.get(key) {
        Some(DatabaseValue::String(value)) => Ok(value),
        _ => Err(Error::MaintenanceRequired),
    };
    let attempt = match row.get("attempt_number") {
        Some(DatabaseValue::Int64(value)) => {
            u32::try_from(value).map_err(|_| Error::MaintenanceRequired)?
        }
        _ => return Err(Error::MaintenanceRequired),
    };
    let node = text("node_id")?;
    let activation = text("activation_id")?;
    if text("run_id")? != run
        || text("dispatch_identity")? != identity
        || crate::dispatch_identity(run, &node, &activation, attempt) != identity
    {
        return Err(Error::MaintenanceRequired);
    }
    let receipt: crate::DispatchReceipt =
        serde_json::from_str(&payload).map_err(|_| Error::MaintenanceRequired)?;
    if receipt.run_id != run
        || receipt.dispatch_identity != identity
        || receipt.node_id != node
        || receipt.activation_id != activation
        || receipt.attempt != attempt
    {
        return Err(Error::MaintenanceRequired);
    }
    let prepared = super::attempts::owner_boundary::prepared(db, run, identity)
        .await?
        .ok_or(Error::MaintenanceRequired)?;
    if prepared.run_id != receipt.run_id
        || prepared.node_id != receipt.node_id
        || prepared.activation_id != receipt.activation_id
        || prepared.attempt != receipt.attempt
        || prepared.dispatch_identity() != identity
    {
        return Err(Error::MaintenanceRequired);
    }
    Ok(Some(receipt))
}

/// Resolve a dispatch identity to its canonical run without scanning histories.
/// # Errors
/// Rejects malformed identity rows, future representation or database failures.
pub async fn run_for_dispatch(db: &dyn Database, identity: &str) -> Result<Option<String>, Error> {
    super::verified(db).await?;
    let rows = db
        .select("workflow_attempts_v2")
        .columns(&["run_id"])
        .where_eq("dispatch_identity", identity)
        .limit(1)
        .execute(db)
        .await?;
    rows.first()
        .map(|row| match row.get("run_id") {
            Some(DatabaseValue::String(run)) => Ok(run),
            _ => Err(Error::MaintenanceRequired),
        })
        .transpose()
}

/// Read attempts in stable dispatch-identity order, verifying stored identities.
/// # Errors
/// Rejects invalid limits, unsupported state, malformed or inconsistent ledger rows.
pub async fn page(
    db: &dyn Database,
    run: &str,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<bcode_workflow::AttemptSummary>, Error> {
    if !(1..=100).contains(&limit) {
        return Err(Error::InvalidAdmission);
    }
    super::verified(db).await?;
    let mut query = db
        .select("workflow_attempts_v2")
        .where_eq("run_id", run)
        .sort("dispatch_identity", SortDirection::Asc)
        .limit(usize::from(limit));
    if let Some(after) = after {
        query = query.where_gt("dispatch_identity", after);
    }
    let mut results = Vec::new();
    for row in query.execute(db).await? {
        let text = |key| match row.get(key) {
            Some(DatabaseValue::String(value)) => Ok(value),
            _ => Err(Error::MaintenanceRequired),
        };
        let identity = text("dispatch_identity")?;
        let prepared = super::attempts::owner_boundary::prepared(db, run, &identity)
            .await?
            .ok_or(Error::MaintenanceRequired)?;
        let terminal =
            super::attempts::owner_boundary::terminal_outcome(db, run, &identity).await?;
        let receipt = super::attempts::owner_boundary::receipt(db, &identity).await?;
        results.push(bcode_workflow::AttemptSummary {
            run_id: prepared.run_id,
            node_id: prepared.node_id,
            activation_id: prepared.activation_id,
            attempt: prepared.attempt,
            dispatch_identity: identity,
            side_effect: prepared.side_effect,
            status: match terminal {
                Some(super::attempts::TerminalOutcome::Succeeded) => "succeeded",
                Some(super::attempts::TerminalOutcome::Failed) => "failed",
                Some(super::attempts::TerminalOutcome::Cancelled) => "cancelled",
                None => {
                    if receipt.is_some() {
                        "admitted"
                    } else {
                        "prepared"
                    }
                }
            }
            .into(),
            has_receipt: receipt.is_some(),
            prepared_at_ms: prepared.prepared_at_ms,
            admitted_at_ms: receipt.as_ref().map(|value| value.admitted_at_ms),
            terminal_at_ms: match row.get("terminal_at_ms") {
                Some(DatabaseValue::Int64(value)) => {
                    Some(u64::try_from(value).map_err(|_| Error::MaintenanceRequired)?)
                }
                _ => None,
            },
        });
    }
    Ok(results)
}
