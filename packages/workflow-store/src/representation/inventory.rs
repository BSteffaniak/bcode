//! Generation-fenced current identity inventory; never historical semantic coverage.
//!
//! These store-local types are not the public application paging contract.
use super::{Database, DatabaseValue, RepresentationError};
use switchy_database::{
    TransactionMode,
    query::{FilterableQuery, SortDirection},
};
mod public;
pub use public::page;
mod installation;
mod observers;
#[cfg(test)]
mod public_tests;
#[cfg(test)]
mod safety_tests;
#[cfg(test)]
mod tests;
const CONTRACT: &str = "workflow_inventory_contract_v1";
const EVENTS: &str = "workflow_inventory_generations_v1";
const VERSION: i64 = 1;

/// Inventory failure without automatic boundary rebasing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A source changed since the original page.
    #[error("workflow inventory page is stale")]
    StalePage,
    /// Unsupported, unavailable or inconsistent storage.
    #[error(transparent)]
    Store(#[from] RepresentationError),
}
impl From<switchy_database::DatabaseError> for Error {
    fn from(value: switchy_database::DatabaseError) -> Self {
        Self::Store(value.into())
    }
}
/// Finite current membership source, not proof of contributor execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Canonical output identities.
    Outputs,
    /// Execution links reached through the canonical attempt index.
    SessionLinks,
    /// Direct child run identities.
    Children,
}
/// Immutable boundary retained across pages; fields are not caller-rewriteable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    run: String,
    revision: u64,
    generation: i64,
    source: Source,
    after: Option<String>,
}
/// A bounded identity page; membership alone does not prove contributor provenance.
#[derive(Debug)]
pub struct Page {
    /// Exact requested boundary, including exclusive examined source key.
    pub cursor: Cursor,
    /// Ordered canonical identities (dispatch identities for execution links).
    pub identities: Vec<String>,
    /// Continuation advances over examined attempts, including those without links.
    pub next: Option<Cursor>,
}
/// Install the additive inventory observers atomically, explicitly before capture.
///
/// # Errors
/// Rejects incompatible schema, observer conflicts and unsupported representation.
pub async fn install(db: &dyn Database) -> Result<(), Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::verified(&*tx).await?;
    installation::install(&*tx).await?;
    tx.commit().await?;
    Ok(())
}
fn text(row: &switchy_database::Row, field: &str) -> Result<String, RepresentationError> {
    match row.get(field) {
        Some(DatabaseValue::String(value)) if !value.is_empty() && value.len() <= 512 => Ok(value),
        _ => Err(RepresentationError::MaintenanceRequired),
    }
}
async fn generation(db: &dyn Database, run: &str) -> Result<i64, Error> {
    // Unattributed observations are permanent gaps, never healed by exhausting rows.
    // Two index-end probes detect every non-complete status without scanning
    // an arbitrarily long prefix of complete observations.
    for direction in [SortDirection::Asc, SortDirection::Desc] {
        let rows = db
            .select(EVENTS)
            .columns(&["attribution_status"])
            .sort("attribution_status", direction)
            .limit(1)
            .execute(db)
            .await?;
        if rows
            .first()
            .is_some_and(|row| row.get("attribution_status") != Some(DatabaseValue::Int64(1)))
        {
            return Err(RepresentationError::MaintenanceRequired.into());
        }
    }
    let rows = db
        .select(EVENTS)
        .columns(&["sequence"])
        .where_eq("run_id", run)
        .sort("sequence", SortDirection::Desc)
        .limit(1)
        .execute(db)
        .await?;
    match rows.first().map(|row| row.get("sequence")) {
        None => Ok(0),
        Some(Some(DatabaseValue::Int64(n))) if n > 0 => Ok(n),
        _ => Err(RepresentationError::MaintenanceRequired.into()),
    }
}
/// Read current identities inside a verified snapshot, rejecting changed boundaries.
///
/// # Errors
/// Rejects invalid run/source cursors, absent observers/indexes, attribution gaps and
/// changed generation/revision. Never acquires or changes execution ownership.
pub async fn identity_page(
    db: &dyn Database,
    run: &str,
    source: Source,
    requested: Option<&Cursor>,
    limit: u16,
) -> Result<Page, Error> {
    crate::validate_id("inventory run", run).map_err(|_| RepresentationError::InvalidAdmission)?;
    if !(1..=100).contains(&limit) || requested.is_some_and(|c| c.run != run || c.source != source)
    {
        return Err(RepresentationError::InvalidAdmission.into());
    }
    let tx = db.begin_transaction().await?;
    super::verified(&*tx).await?;
    installation::verify(&*tx).await?;
    let revision = super::graph_pages::revision(&*tx, run).await?;
    let generation = generation(&*tx, run).await?;
    if requested.is_some_and(|c| c.revision != revision || c.generation != generation) {
        return Err(Error::StalePage);
    }
    let cursor = requested.cloned().unwrap_or_else(|| Cursor {
        run: run.into(),
        revision,
        generation,
        source,
        after: None,
    });
    let (table, run_column, key) = match source {
        Source::Outputs => ("workflow_outputs_v2", "run_id", "output_id"),
        Source::SessionLinks => ("workflow_attempts_v2", "run_id", "dispatch_identity"),
        Source::Children => ("workflow_lineage_v2", "parent_run_id", "run_id"),
    };
    let selected = [key];
    let mut query = tx
        .select(table)
        .columns(&selected)
        .where_eq(run_column, run)
        .sort(key, SortDirection::Asc)
        .limit(usize::from(limit) + 1);
    if let Some(after) = &cursor.after {
        query = query.where_gt(key, after.as_str());
    }
    let mut rows = query.execute(&*tx).await?;
    let more = rows.len() > usize::from(limit);
    rows.truncate(usize::from(limit));
    let mut identities = Vec::new();
    let mut last = cursor.after.clone();
    for row in rows {
        let identity = text(&row, key)?;
        if last.as_ref().is_some_and(|previous| previous >= &identity) {
            return Err(RepresentationError::MaintenanceRequired.into());
        }
        last = Some(identity.clone());
        if source == Source::SessionLinks {
            let links = tx
                .select("workflow_execution_sessions_v2")
                .columns(&["dispatch_identity"])
                .where_eq("dispatch_identity", identity.as_str())
                .limit(2)
                .execute(&*tx)
                .await?;
            if links.len() > 1 {
                return Err(RepresentationError::MaintenanceRequired.into());
            }
            if links.is_empty() {
                continue;
            }
        }
        identities.push(identity);
    }
    let next = more.then(|| Cursor {
        after: last,
        ..cursor.clone()
    });
    tx.commit().await?;
    Ok(Page {
        cursor,
        identities,
        next,
    })
}
