//! Bounded transaction-consistent workflow activity inventory; no execution authority.
use super::{Database, DatabaseValue, RepresentationError as Error};
use switchy_database::query::{FilterableQuery, SortDirection};

/// Inventory section. Child discovery is workflow lineage, not session ancestry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// All canonical attempts, including unresolved attempts without session links.
    Attempts,
    /// Direct child-run links; consumers recursively page each returned child.
    Children,
}

/// Exact attempt coordinates and optional verified execution-session provenance.
#[derive(Debug, Clone)]
pub struct Attempt {
    /// Canonical dispatch identity and exclusive ordering key.
    pub dispatch_identity: String,
    /// Node owning the attempt.
    pub node_id: String,
    /// Exact activation, including historical activations.
    pub activation_id: String,
    /// Canonical attempt number.
    pub attempt: u32,
    /// True only when a canonical terminal outcome is present.
    pub terminal: bool,
    /// Canonical plan owner; interpreted only by the application owner.
    pub dispatch_owner: Option<String>,
    /// Canonical preparation time.
    pub prepared_at_ms: u64,
    /// Verified receipt admission time.
    pub admitted_at_ms: Option<u64>,
    /// Verified terminal time.
    pub terminal_at_ms: Option<u64>,
    /// Verified workflow-child receipt target, when present.
    pub child_run_id: Option<String>,
    /// Absence is explicit, not evidence that an unresolved attempt is inactive.
    pub session: Option<crate::WorkflowExecutionSessionLink>,
}

/// A single bounded database snapshot. This is NOT a multi-page/subtree snapshot.
#[derive(Debug, Clone)]
pub struct InventoryPage {
    /// Current graph revision verified in this transaction.
    pub revision: u64,
    /// Canonical attempts for the requested section.
    pub attempts: Vec<Attempt>,
    /// Canonical child links for the requested section.
    pub children: Vec<crate::WorkflowRunLink>,
    /// Exclusive scanned identity, including attempts with no session link.
    pub next_after: Option<String>,
}

fn text(row: &switchy_database::Row, key: &str) -> Result<String, Error> {
    match row.get(key) {
        Some(DatabaseValue::String(value)) if value.len() <= crate::MAX_ID_BYTES => Ok(value),
        _ => Err(Error::MaintenanceRequired),
    }
}

fn payload<T: serde::de::DeserializeOwned>(
    row: &switchy_database::Row,
    key: &str,
) -> Result<T, Error> {
    match row.get(key) {
        Some(DatabaseValue::String(value)) if value.len() <= super::MAX_ADMISSION_BYTES => {
            serde_json::from_str(&value).map_err(|_| Error::MaintenanceRequired)
        }
        _ => Err(Error::MaintenanceRequired),
    }
}

/// Read a bounded inventory page within ONE read transaction, with exact revision checking.
///
/// No repairs, migrations, authority acquisition or history replay. Graph revisions
/// do not fence concurrent child admission, session-link writes or attempt settlement.
/// Therefore even exhausted pages do not certify cross-page/subtree stability.
/// # Errors
/// Rejects invalid bounds/identities, revision conflicts, unsupported representation,
/// malformed provenance, missing runs and database failures.
pub async fn page(
    db: &dyn Database,
    run: &str,
    expected_revision: Option<u64>,
    section: Section,
    after: Option<&str>,
    limit: u16,
) -> Result<InventoryPage, Error> {
    crate::validate_id("run_id", run).map_err(|_| Error::InvalidAdmission)?;
    if !(1..=100).contains(&limit) || expected_revision == Some(0) {
        return Err(Error::InvalidAdmission);
    }
    if let Some(after) = after {
        crate::validate_id("activity_cursor", after).map_err(|_| Error::InvalidAdmission)?;
    }
    let tx = db.begin_transaction().await?;
    match page_in(&*tx, run, expected_revision, section, after, limit).await {
        Ok(result) => {
            tx.commit().await?;
            Ok(result)
        }
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

/// Read within a caller-retained transaction, without starting or ending a transaction.
///
/// Every recursively discovered run can use the same snapshot. Exhaustion describes
/// that snapshot only, never current execution or durable resume.
/// # Errors
/// Rejects invalid coordinates, conflicting revisions, malformed data and storage failures.
pub async fn page_in(
    db: &dyn Database,
    run: &str,
    expected_revision: Option<u64>,
    section: Section,
    after: Option<&str>,
    limit: u16,
) -> Result<InventoryPage, Error> {
    crate::validate_id("run_id", run).map_err(|_| Error::InvalidAdmission)?;
    if !(1..=100).contains(&limit) || expected_revision == Some(0) {
        return Err(Error::InvalidAdmission);
    }
    if let Some(after) = after {
        crate::validate_id("activity_cursor", after).map_err(|_| Error::InvalidAdmission)?;
    }
    super::verified(db).await?;
    match super::runs::lookup_in(db, run).await? {
        super::runs::RunLookup::Available(_) => {}
        _ => return Err(Error::MaintenanceRequired),
    }
    let revision = super::graph_pages::revision(db, run).await?;
    if expected_revision.is_some_and(|expected| expected != revision) {
        return Err(Error::InvalidAdmission);
    }
    let (table, scope, key) = match section {
        Section::Attempts => ("workflow_attempts_v2", "run_id", "dispatch_identity"),
        Section::Children => ("workflow_lineage_v2", "parent_run_id", "run_id"),
    };
    let mut query = db
        .select(table)
        .where_eq(scope, run)
        .sort(key, SortDirection::Asc)
        .limit(usize::from(limit) + 1);
    if let Some(after) = after {
        query = query.where_gt(key, after);
    }
    let mut rows = query.execute(db).await?;
    let more = rows.len() > usize::from(limit);
    rows.truncate(usize::from(limit));
    let next_after = if more {
        rows.last().map(|row| text(row, key)).transpose()?
    } else {
        None
    };
    let mut result = InventoryPage {
        revision,
        attempts: Vec::new(),
        children: Vec::new(),
        next_after,
    };
    for row in rows {
        match section {
            Section::Children => {
                let link: crate::WorkflowRunLink = payload(&row, "link_json")?;
                if link.parent_run_id != run || link.child_run_id != text(&row, "run_id")? {
                    return Err(Error::MaintenanceRequired);
                }
                result.children.push(link);
            }
            Section::Attempts => {
                result.attempts.push(decode_attempt(db, run, &row).await?);
            }
        }
    }
    Ok(result)
}

async fn decode_attempt(
    db: &dyn Database,
    run: &str,
    row: &switchy_database::Row,
) -> Result<Attempt, Error> {
    let identity = text(row, "dispatch_identity")?;
    let prepared: crate::PreparedAttempt = payload(row, "prepared_json")?;
    if prepared.run_id != run
        || prepared.dispatch_identity() != identity
        || prepared.node_id != text(row, "node_id")?
        || prepared.activation_id != text(row, "activation_id")?
        || row.get("attempt_number") != Some(DatabaseValue::Int64(i64::from(prepared.attempt)))
    {
        return Err(Error::MaintenanceRequired);
    }
    let ledger =
        super::attempts::owner_boundary::attempt_observation_in(db, run, &identity).await?;
    let terminal = ledger.outcome.is_some();
    let dispatch_owner = prepared
        .intent
        .get("owner")
        .and_then(serde_json::Value::as_str)
        .filter(|owner| owner.len() <= crate::MAX_ID_BYTES)
        .map(str::to_owned);
    let child_run_id = ledger
        .receipt
        .as_ref()
        .and_then(|receipt| receipt.receipt.get("child_run_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    if let Some(child) = &child_run_id {
        crate::validate_id("child_run_id", child).map_err(|_| Error::MaintenanceRequired)?;
        let rows = db
            .select("workflow_lineage_v2")
            .where_eq("run_id", child.as_str())
            .limit(1)
            .execute(db)
            .await?;
        let row = rows.first().ok_or(Error::MaintenanceRequired)?;
        let link: crate::WorkflowRunLink = payload(row, "link_json")?;
        if link.parent_run_id != run
            || link.child_run_id != *child
            || link.parent_node_id != prepared.node_id
            || link.parent_activation_id != prepared.activation_id
            || link.parent_attempt != prepared.attempt
        {
            return Err(Error::MaintenanceRequired);
        }
    }
    let links = db
        .select("workflow_execution_sessions_v2")
        .where_eq("dispatch_identity", identity.as_str())
        .limit(2)
        .execute(db)
        .await?;
    if links.len() > 1 {
        return Err(Error::MaintenanceRequired);
    }
    let session = links
        .first()
        .map(|row| decode_session_link(row, &prepared, &identity))
        .transpose()?;
    Ok(Attempt {
        dispatch_identity: identity,
        node_id: prepared.node_id,
        activation_id: prepared.activation_id,
        attempt: prepared.attempt,
        terminal,
        dispatch_owner,
        prepared_at_ms: prepared.prepared_at_ms,
        admitted_at_ms: ledger
            .receipt
            .as_ref()
            .map(|receipt| receipt.admitted_at_ms),
        terminal_at_ms: ledger.terminal_at_ms,
        child_run_id,
        session,
    })
}

fn decode_session_link(
    row: &switchy_database::Row,
    prepared: &crate::PreparedAttempt,
    identity: &str,
) -> Result<crate::WorkflowExecutionSessionLink, Error> {
    let link: crate::WorkflowExecutionSessionLink = payload(row, "link_json")?;
    crate::validate_execution_session_link(&link).map_err(|_| Error::MaintenanceRequired)?;
    if link.run_id != prepared.run_id
        || link.node_id != prepared.node_id
        || link.activation_id != prepared.activation_id
        || link.attempt != prepared.attempt
        || text(row, "dispatch_identity")? != identity
    {
        return Err(Error::MaintenanceRequired);
    }
    Ok(link)
}

/// Verify membership under an exact canonical root in the caller's snapshot.
///
/// Subtree scans start at a canonical root, not an arbitrary intermediate run.
/// # Errors
/// Rejects missing or malformed lineage and database failures.
pub async fn in_subtree(db: &dyn Database, root: &str, run: &str) -> Result<bool, Error> {
    crate::validate_id("root_run_id", root).map_err(|_| Error::InvalidAdmission)?;
    crate::validate_id("run_id", run).map_err(|_| Error::InvalidAdmission)?;
    let roots = db
        .select("workflow_lineage_v2")
        .where_eq("run_id", root)
        .limit(1)
        .execute(db)
        .await?;
    let root_row = roots.first().ok_or(Error::MaintenanceRequired)?;
    if text(root_row, "root_run_id")? != root
        || root_row.get("parent_run_id") != Some(DatabaseValue::Null)
    {
        return Err(Error::InvalidAdmission);
    }
    let rows = db
        .select("workflow_lineage_v2")
        .where_eq("run_id", run)
        .limit(1)
        .execute(db)
        .await?;
    let row = rows.first().ok_or(Error::MaintenanceRequired)?;
    Ok(text(row, "root_run_id")? == root)
}

#[cfg(test)]
#[path = "activity_tests.rs"]
mod tests;
