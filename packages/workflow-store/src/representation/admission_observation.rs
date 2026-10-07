//! Additive, independently compatible admission diagnostics. Never grants retry authority.
use super::{Database, DatabaseValue, RepresentationError as Error, column};
use crate::{PreparedAttempt, WorkflowExecutionAuthority};
use serde::{Deserialize, Serialize};
use switchy_database::{
    TransactionMode,
    query::{FilterableQuery, SortDirection},
    schema::{DataType, create_index, create_table},
};

const CONTRACT: &str = "workflow_admission_observation_contract";
const TABLE: &str = "workflow_admission_observations_v1";
/// Current independently compatible diagnostic schema.
pub const VERSION: u32 = 1;

/// Boundary whose outcome could not be established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    OwnerDispatch,
    ReceiptPersistence,
}
/// Normalized failure facts; never derived by matching private error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    OwnerAccessDeferred,
    OwnerError,
    ReceiptPersistenceUnavailable,
}
/// Diagnostic only. Acceptance and side effects remain unverified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub version: u32,
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub attempt: u32,
    pub dispatch_identity: String,
    pub phase: Phase,
    pub category: Category,
    pub observed_at_ms: u64,
}

/// Exclusive bounded-inspection cursor derived from a returned observation.
#[must_use]
pub fn cursor(value: &Observation) -> String {
    identity(value)
}

fn identity(value: &Observation) -> String {
    format!(
        "{}:{}",
        value.dispatch_identity,
        match value.phase {
            Phase::OwnerDispatch => "owner",
            Phase::ReceiptPersistence => "receipt",
        }
    )
}

/// Install under the caller's exclusive Immediate schema-install transaction.
///
/// Caller must verify base compatibility before calling and commit both tables together.
/// No normal read/write path invokes this installer.
///
/// # Errors
/// Returns maintenance-required for partial, unversioned or unsupported diagnostic state;
/// returns a database error when schema inspection or additive installation fails.
/// Unknown state is preserved; the caller must roll back its install transaction on error.
pub async fn install_in(db: &dyn Database) -> Result<(), Error> {
    let contract = db.table_exists(CONTRACT).await?;
    let table = db.table_exists(TABLE).await?;
    if contract || table {
        if !contract || !table {
            return Err(Error::MaintenanceRequired);
        }
        return verify(db).await;
    }
    create_table(CONTRACT)
        .column(column("contract_id", DataType::Int, false))
        .primary_key("contract_id")
        .column(column("version", DataType::Int, false))
        .execute(db)
        .await?;
    create_table(TABLE)
        .column(column("identity", DataType::Text, false))
        .primary_key("identity")
        .column(column("run_id", DataType::Text, false))
        .column(column("observation_json", DataType::Text, false))
        .execute(db)
        .await?;
    create_index("idx_workflow_admission_observations_run")
        .table(TABLE)
        .columns(vec!["run_id", "identity"])
        .execute(db)
        .await?;
    db.insert(CONTRACT)
        .value("contract_id", 1_i64)
        .value("version", i64::from(VERSION))
        .execute(db)
        .await?;
    Ok(())
}

async fn verify(db: &dyn Database) -> Result<(), Error> {
    if !db.table_exists(CONTRACT).await? || !db.table_exists(TABLE).await? {
        return Err(Error::MaintenanceRequired);
    }
    let rows = db
        .select(CONTRACT)
        .columns(&["contract_id", "version"])
        .limit(2)
        .execute(db)
        .await?;
    if rows.len() != 1
        || rows[0].get("contract_id") != Some(DatabaseValue::Int64(1))
        || rows[0].get("version") != Some(DatabaseValue::Int64(i64::from(VERSION)))
    {
        return Err(Error::MaintenanceRequired);
    }
    Ok(())
}

fn decode(row: &switchy_database::Row) -> Result<Observation, Error> {
    let Some(DatabaseValue::String(json)) = row.get("observation_json") else {
        return Err(Error::MaintenanceRequired);
    };
    if json.len() > 8_192 {
        return Err(Error::MaintenanceRequired);
    }
    let value: Observation = serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)?;
    validate(&value).map_err(|_| Error::MaintenanceRequired)?;
    if row.get("identity") != Some(DatabaseValue::String(identity(&value)))
        || row.get("run_id") != Some(DatabaseValue::String(value.run_id.clone()))
    {
        return Err(Error::MaintenanceRequired);
    }
    Ok(value)
}

fn validate(value: &Observation) -> Result<(), Error> {
    for (name, id) in [
        ("run_id", &value.run_id),
        ("node_id", &value.node_id),
        ("activation_id", &value.activation_id),
    ] {
        crate::validate_id(name, id).map_err(|_| Error::InvalidAdmission)?;
    }
    if value.version != VERSION
        || value.attempt == 0
        || value.dispatch_identity.len() != 64
        || !value
            .dispatch_identity
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || i64::try_from(value.observed_at_ms).is_err()
        || !matches!(
            (value.phase, value.category),
            (
                Phase::OwnerDispatch,
                Category::OwnerAccessDeferred | Category::OwnerError
            ) | (
                Phase::ReceiptPersistence,
                Category::ReceiptPersistenceUnavailable
            )
        )
    {
        return Err(Error::InvalidAdmission);
    }
    Ok(())
}

/// Record an immutable diagnostic under exact current authority and preparation.
///
/// # Errors
/// Rejects stale authority, malformed or mismatched prepared identities, unsupported contracts
/// and conflicting duplicates. Returns database errors without inferring commit outcome.
/// Attempts, receipts, outcomes and retry grants are never modified.
pub async fn record(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    value: &Observation,
) -> Result<(), Error> {
    validate(value)?;
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::verified(&*tx).await?;
    verify(&*tx).await?;
    let runs = tx
        .select(super::RUNS)
        .columns(&["authority_json"])
        .where_eq("run_id", value.run_id.as_str())
        .limit(1)
        .execute(&*tx)
        .await?;
    let encoded_owner = serde_json::to_string(owner).map_err(|_| Error::InvalidAdmission)?;
    if runs.first().and_then(|row| row.get("authority_json"))
        != Some(DatabaseValue::String(encoded_owner))
    {
        return Err(Error::AuthorityConflict);
    }
    let rows = tx
        .select("workflow_attempts_v2")
        .where_eq("dispatch_identity", value.dispatch_identity.as_str())
        .limit(1)
        .execute(&*tx)
        .await?;
    let row = rows.first().ok_or(Error::InvalidAdmission)?;
    let Some(DatabaseValue::String(json)) = row.get("prepared_json") else {
        return Err(Error::MaintenanceRequired);
    };
    if json.len() > super::MAX_ADMISSION_BYTES {
        return Err(Error::MaintenanceRequired);
    }
    let prepared: PreparedAttempt =
        serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)?;
    check_prepared(&prepared, value)?;
    if row.get("run_id") != Some(DatabaseValue::String(value.run_id.clone()))
        || row.get("node_id") != Some(DatabaseValue::String(value.node_id.clone()))
        || row.get("activation_id") != Some(DatabaseValue::String(value.activation_id.clone()))
        || row.get("attempt_number") != Some(DatabaseValue::Int64(i64::from(value.attempt)))
    {
        return Err(Error::InvalidAdmission);
    }
    let rows = tx
        .select(TABLE)
        .where_eq("identity", identity(value))
        .limit(1)
        .execute(&*tx)
        .await?;
    if let Some(row) = rows.first() {
        let mut previous = decode(row)?;
        previous.observed_at_ms = value.observed_at_ms;
        if previous != *value {
            return Err(Error::InvalidAdmission);
        }
    } else {
        tx.insert(TABLE)
            .value("identity", identity(value))
            .value("run_id", value.run_id.as_str())
            .value(
                "observation_json",
                serde_json::to_string(value).map_err(|_| Error::InvalidAdmission)?,
            )
            .execute(&*tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn check_prepared(prepared: &PreparedAttempt, value: &Observation) -> Result<(), Error> {
    if prepared.run_id != value.run_id
        || prepared.node_id != value.node_id
        || prepared.activation_id != value.activation_id
        || prepared.attempt != value.attempt
        || prepared.dispatch_identity() != value.dispatch_identity
    {
        return Err(Error::InvalidAdmission);
    }
    Ok(())
}

/// Read at most 100 immutable diagnostics; exclusive cursor is the last returned identity.
/// # Errors
/// Rejects invalid run identities, limits outside 1..=100 and oversized cursors;
/// reports missing, partial, corrupt or unsupported diagnostic contracts/records as
/// maintenance-required, and query failures as database errors. Never installs or repairs.
pub async fn inspect(
    db: &dyn Database,
    run_id: &str,
    after_identity: Option<&str>,
    limit: usize,
) -> Result<Vec<Observation>, Error> {
    crate::validate_id("run_id", run_id).map_err(|_| Error::InvalidAdmission)?;
    if !(1..=100).contains(&limit) || after_identity.is_some_and(|id| id.len() > 80) {
        return Err(Error::InvalidAdmission);
    }
    verify(db).await?;
    let mut query = db
        .select(TABLE)
        .where_eq("run_id", run_id)
        .sort("identity", SortDirection::Asc)
        .limit(limit);
    if let Some(after) = after_identity {
        query = query.where_gt("identity", after);
    }
    query.execute(db).await?.iter().map(decode).collect()
}

#[cfg(test)]
mod tests;
