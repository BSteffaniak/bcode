//! Bounded attempt observation from the canonical new-family ledger.
use super::{Database, DatabaseValue, RepresentationError as Error};
use switchy_database::query::{FilterableQuery, SortDirection};

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
            admitted_at_ms: None,
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
