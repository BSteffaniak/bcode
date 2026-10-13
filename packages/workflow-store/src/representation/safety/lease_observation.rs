//! Optional, transaction-owned acquisition facts for canonical active resource leases.
use super::{Error, LEASES, column};
use switchy_database::{
    Database, DatabaseTransaction, DatabaseValue,
    query::{FilterableQuery, SortDirection},
    schema::{DataType, create_table},
};
const FACTS: &str = "workflow_lease_acquisitions_v1";
const CONTRACT: &str = "workflow_lease_observation_contract";

pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(FACTS)
        .column(column("run_id", DataType::Text, false))
        .column(column("dispatch_identity", DataType::Text, false))
        .column(column("resource_key", DataType::Text, false))
        .column(column("version", DataType::BigInt, false))
        .column(column("acquired_at_ms", DataType::BigInt, false))
        .unique_columns(["run_id", "dispatch_identity", "resource_key"])
        .execute(db)
        .await?;
    create_table(CONTRACT)
        .column(column("version", DataType::BigInt, false))
        .primary_key("version")
        .execute(db)
        .await?;
    let versions = db.select(CONTRACT).limit(2).execute(db).await?;
    if versions.is_empty() {
        db.insert(CONTRACT)
            .value("version", 1_i64)
            .execute(db)
            .await?;
    } else if versions.len() != 1 || versions[0].get("version") != Some(DatabaseValue::Int64(1)) {
        return Err(Error::MaintenanceRequired);
    }
    Ok(())
}

async fn verify(db: &dyn Database) -> Result<(), Error> {
    if !db.table_exists(CONTRACT).await? {
        return Err(Error::MaintenanceRequired);
    }
    let versions = db.select(CONTRACT).limit(2).execute(db).await?;
    if versions.len() != 1 || versions[0].get("version") != Some(DatabaseValue::Int64(1)) {
        return Err(Error::MaintenanceRequired);
    }
    Ok(())
}

pub(super) async fn record(
    tx: &dyn DatabaseTransaction,
    run: &str,
    dispatch: &str,
    key: &str,
    now: i64,
) -> Result<(), Error> {
    if !tx.table_exists(FACTS).await? {
        return Ok(());
    }
    verify(tx).await?;
    tx.insert(FACTS)
        .value("run_id", run)
        .value("dispatch_identity", dispatch)
        .value("resource_key", key)
        .value("version", 1_i64)
        .value("acquired_at_ms", now)
        .execute(tx)
        .await?;
    Ok(())
}

pub(super) async fn release(
    tx: &dyn DatabaseTransaction,
    run: &str,
    dispatch: &str,
) -> Result<(), Error> {
    if tx.table_exists(FACTS).await? {
        tx.delete(FACTS)
            .where_eq("run_id", run)
            .where_eq("dispatch_identity", dispatch)
            .execute(tx)
            .await?;
    }
    Ok(())
}

/// One bounded active lease page, not historical lease coverage.
#[derive(Debug)]
pub struct LeasePage {
    /// Exact canonical leases with producer-recorded acquisition times.
    pub leases: Vec<bcode_workflow::WorkflowResourceLease>,
    /// True only if lookahead proves active rows are exhausted.
    pub complete: bool,
}

/// Read current canonical leases in the caller's snapshot; never install or backfill facts.
/// # Errors
/// Rejects invalid bounds, unknown versions, missing acquisition facts or inconsistent attempts.
pub async fn page_in(
    tx: &dyn DatabaseTransaction,
    run: &str,
    limit: u16,
) -> Result<LeasePage, Error> {
    if !(1..=100).contains(&limit) {
        return Err(Error::InvalidAdmission);
    }
    super::super::verified(tx).await?;
    verify(tx).await?;
    if !tx.table_exists(FACTS).await? {
        return Err(Error::MaintenanceRequired);
    }
    let mut rows = tx
        .select(LEASES)
        .where_eq("run_id", run)
        .sort("dispatch_identity", SortDirection::Asc)
        .sort("resource_key", SortDirection::Asc)
        .limit(usize::from(limit) + 1)
        .execute(tx)
        .await?;
    let complete = rows.len() <= usize::from(limit);
    rows.truncate(usize::from(limit));
    let mut leases = Vec::new();
    for row in rows {
        let dispatch = text(&row, "dispatch_identity")?;
        let key = text(&row, "resource_key")?;
        let facts = tx
            .select(FACTS)
            .where_eq("run_id", run)
            .where_eq("dispatch_identity", dispatch.as_str())
            .where_eq("resource_key", key.as_str())
            .limit(1)
            .execute(tx)
            .await?;
        let fact = facts.first().ok_or(Error::MaintenanceRequired)?;
        if fact.get("version") != Some(DatabaseValue::Int64(1)) {
            return Err(Error::MaintenanceRequired);
        }
        let acquired_at_ms = match fact.get("acquired_at_ms") {
            Some(DatabaseValue::Int64(value)) if value >= 0 => value.cast_unsigned(),
            _ => return Err(Error::MaintenanceRequired),
        };
        let prepared = super::super::attempts::owner_boundary::prepared_in(tx, run, &dispatch)
            .await?
            .ok_or(Error::MaintenanceRequired)?;
        let mode = match text(&row, "access")?.as_str() {
            "read" => bcode_workflow::ResourceLeaseMode::Read,
            "write" => bcode_workflow::ResourceLeaseMode::Write,
            _ => return Err(Error::MaintenanceRequired),
        };
        leases.push(bcode_workflow::WorkflowResourceLease {
            lease_id: serde_json::to_string(&(run, &dispatch, &key))
                .map_err(|_| Error::InvalidAdmission)?,
            run_id: run.into(),
            node_id: prepared.node_id,
            activation_id: prepared.activation_id,
            resource_key: key,
            mode,
            acquired_at_ms,
            expires_at_ms: None,
        });
    }
    Ok(LeasePage { leases, complete })
}

fn text(row: &switchy_database::Row, field: &str) -> Result<String, Error> {
    match row.get(field) {
        Some(DatabaseValue::String(value)) if value.len() <= super::super::MAX_ADMISSION_BYTES => {
            Ok(value)
        }
        _ => Err(Error::MaintenanceRequired),
    }
}

#[cfg(test)]
mod tests;
