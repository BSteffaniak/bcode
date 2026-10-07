//! Opt-in transactional row-transition observations, independent of representation v30.
//!
//! This is finite diagnostic coverage, not reconstructed execution history. JSON owner payloads
//! and authority material are never observed. Sources without direct run scope are excluded.
use super::{Database, DatabaseValue, RepresentationError as Error, TransactionMode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use switchy_database::{
    Row,
    query::{FilterableQuery, SortDirection},
};

pub(super) use installation::install_in;
mod installation;
mod observers;
#[cfg(test)]
mod tests;

const VERSION: i64 = 1;
const CONTRACT: &str = "workflow_observation_contract_v1";
const COVERAGE: &str = "workflow_observation_runs_v1";
const EVENTS: &str = "workflow_observations_v1";

/// A declared finite source and exact selected columns; absent tables are not covered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceCoverage {
    /// Physical representation table, not a guessed execution event.
    pub table: String,
    /// Observed scalar columns only.
    pub columns: Vec<String>,
    /// Insert, update and delete observations are installed for these sources.
    pub operations: Vec<String>,
}

/// Per-run origin: historical state is never backfilled or silently enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageOrigin {
    /// Opt-in preceded the canonical admission insert in the same transaction.
    Admission,
    /// No opt-in marker exists. An empty page is not evidence of no transitions.
    NotEnabled,
}

/// A scalar fact, preserving null rather than inventing an absent value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ScalarFact {
    /// Selected column was null.
    Null,
    /// Stored integer; booleans in `SQLite` are represented by integers.
    Integer(i64),
    /// Bounded identifier or status text, never private owner JSON.
    Text(String),
    /// Selected value could not be retained; distinct from an actual null.
    Unavailable(FactUnavailableReason),
}

/// Why a selected diagnostic fact was not retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactUnavailableReason {
    /// Text exceeds the transactional byte bound.
    Oversized,
    /// Source storage type does not match the declared fact type.
    UnsupportedType,
}

/// One committed source row transition. Sequence orders observations, not execution effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionObservation {
    /// Generated journal sequence, scoped by run for pagination (gaps are permitted).
    pub sequence: u64,
    /// Owning run.
    pub run_id: String,
    /// Exact observed source table.
    pub source_table: String,
    /// Normalized storage operation: insert, update or delete.
    pub operation: String,
    /// Selected new row facts (old facts for delete); not a before/after execution diff.
    pub facts: BTreeMap<String, ScalarFact>,
    /// Actual source transition time only when a recorded timestamp changed or was inserted.
    pub timestamp_ms: Option<u64>,
    /// Source column for the optional timestamp.
    pub timestamp_source: Option<String>,
}

/// Versioned bounded diagnostic snapshot; not a durable event subscription/resume contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationPage {
    /// Independently evolving observation contract version.
    pub version: u32,
    /// Owning run.
    pub run_id: String,
    /// Explicit coverage start, never inferred from event presence.
    pub origin: CoverageOrigin,
    /// Exact finite observer scope, not all representation transitions.
    pub sources: Vec<SourceCoverage>,
    /// Explicit exclusions and limits of this coverage.
    pub limitations: Vec<String>,
    /// Fixed upper sequence for this scan; later writes require a fresh scan.
    pub through_sequence: u64,
    /// Exclusive cursor for a further page, absent when this bounded snapshot is exhausted.
    pub next_after_sequence: Option<u64>,
    /// Retained scalar transitions.
    pub observations: Vec<TransitionObservation>,
}

/// Install and verify the additive journal in an owned Immediate transaction.
/// Neither the global store contract nor representation version changes.
///
/// # Errors
/// Rejects unsupported representation, conflicting schema/observers and database failures.
pub async fn install(db: &dyn Database) -> Result<(), Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::verified(&*tx).await?;
    installation::install_in(&*tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Check whether the independently installed extension is available without installing it.
/// # Errors
/// Rejects partial/unversioned journal tables or unsupported/damaged extension definitions.
pub async fn available(db: &dyn Database) -> Result<bool, Error> {
    if db.get_table_info(CONTRACT).await?.is_none() {
        if db.get_table_info(COVERAGE).await?.is_some()
            || db.get_table_info(EVENTS).await?.is_some()
        {
            return Err(Error::MaintenanceRequired);
        }
        return Ok(false);
    }
    installation::verify(db).await?;
    Ok(true)
}

/// Enable only a new admission, on the caller's admission transaction before inserting the run.
/// Installation must already have completed. Rollback also removes opt-in and observations.
/// # Errors
/// Rejects historical/legacy runs, repeated opt-in, invalid IDs or unverified extension state.
pub async fn enable_admission(
    tx: &dyn switchy_database::DatabaseTransaction,
    run_id: &str,
) -> Result<(), Error> {
    validate_run(run_id)?;
    super::verified(tx).await?;
    installation::verify(tx).await?;
    for table in [super::RUNS, "workflow_runs", COVERAGE] {
        if !tx
            .select(table)
            .where_eq("run_id", run_id)
            .limit(1)
            .execute(tx)
            .await?
            .is_empty()
        {
            return Err(Error::InvalidAdmission);
        }
    }
    tx.insert(COVERAGE)
        .value("run_id", run_id)
        .value("version", VERSION)
        .execute(tx)
        .await?;
    Ok(())
}

const fn validate_run(run: &str) -> Result<(), Error> {
    if run.is_empty() || run.len() > 256 {
        return Err(Error::InvalidAdmission);
    }
    Ok(())
}

fn integer(row: &Row, key: &str) -> Result<u64, Error> {
    match row.get(key) {
        Some(DatabaseValue::Int64(value)) => {
            u64::try_from(value).map_err(|_| Error::MaintenanceRequired)
        }
        _ => Err(Error::MaintenanceRequired),
    }
}

fn text(row: &Row, key: &str) -> Result<String, Error> {
    match row.get(key) {
        Some(DatabaseValue::String(value)) if value.len() <= 512 => Ok(value),
        _ => Err(Error::MaintenanceRequired),
    }
}

/// Read a bounded run-local sequence page without installation, backfill or source replay.
/// A page contains at most 100 rows of bounded selected scalar facts.
/// # Errors
/// Rejects invalid cursors/limits, unsupported or damaged extension, oversized facts and DB errors.
pub async fn page(
    db: &dyn Database,
    run_id: &str,
    after_sequence: u64,
    through_sequence: Option<u64>,
    limit: u16,
) -> Result<ObservationPage, Error> {
    validate_run(run_id)?;
    if !(1..=100).contains(&limit) {
        return Err(Error::InvalidAdmission);
    }
    let tx = db.begin_transaction().await?;
    super::verified(&*tx).await?;
    installation::verify(&*tx).await?;
    let coverage = tx
        .select(COVERAGE)
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(&*tx)
        .await?;
    let origin = if let Some(row) = coverage.first() {
        if integer(row, "version")?
            != u64::try_from(VERSION).map_err(|_| Error::MaintenanceRequired)?
        {
            return Err(Error::MaintenanceRequired);
        }
        CoverageOrigin::Admission
    } else {
        CoverageOrigin::NotEnabled
    };
    let latest = tx
        .select(EVENTS)
        .where_eq("run_id", run_id)
        .sort("sequence", SortDirection::Desc)
        .limit(1)
        .execute(&*tx)
        .await?;
    let high = latest
        .first()
        .map(|row| integer(row, "sequence"))
        .transpose()?
        .unwrap_or(0);
    let through = through_sequence.unwrap_or(high);
    if through > high || after_sequence > through {
        return Err(Error::InvalidAdmission);
    }
    let rows = tx
        .select(EVENTS)
        .where_eq("run_id", run_id)
        .where_gt(
            "sequence",
            i64::try_from(after_sequence).map_err(|_| Error::InvalidAdmission)?,
        )
        .where_lte(
            "sequence",
            i64::try_from(through).map_err(|_| Error::InvalidAdmission)?,
        )
        .sort("sequence", SortDirection::Asc)
        .limit(usize::from(limit) + 1)
        .execute(&*tx)
        .await?;
    if origin == CoverageOrigin::NotEnabled && !rows.is_empty() {
        return Err(Error::MaintenanceRequired);
    }
    let has_more = rows.len() > usize::from(limit);
    let observations = rows
        .iter()
        .take(usize::from(limit))
        .map(observers::decode)
        .collect::<Result<Vec<_>, _>>()?;
    let next_after_sequence = has_more
        .then(|| observations.last().map(|row| row.sequence))
        .flatten();
    let result = ObservationPage {
        version: 1, run_id: run_id.into(), origin, sources: observers::coverage(),
        limitations: vec![
            "Only declared direct-run-id sources and selected scalar columns are observed".into(),
            "No owner JSON, exact judgement evidence, session links or graph staging/cancellation-publication sources".into(),
            "Text facts are transactionally bounded to 512 bytes; integer and timestamp facts retain only integer storage, with explicit unavailable reasons".into(),
            "No transaction identity, execution meaning, historical backfill or durable resume guarantee".into(),
        ],
        through_sequence: through, next_after_sequence, observations,
    };
    tx.commit().await?;
    Ok(result)
}
