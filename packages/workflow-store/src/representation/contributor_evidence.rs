//! Bounded contributor identity proof for an unchanged ordinary direct execution edge.
//!
//! This observes workflow-owned identities only. Session content remains session-owned.
use super::{Database, DatabaseValue, RepresentationError as Error};
use crate::{WorkflowExecutionAuthority, WorkflowExecutionSessionLink};
use serde::{Deserialize, Serialize};
use switchy_database::query::FilterableQuery;

mod generated;
mod proof;
#[cfg(test)]
mod tests;

/// Exact workflow-owned reference; not proof of integration or goal completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectContributorReference {
    /// Owning run, derived from the authenticated caller.
    pub run_id: String,
    /// Exact consumer activation, never a caller-selected source.
    pub consumer_activation_id: String,
    /// Admitted and still-current graph revision.
    pub graph_revision: u64,
    /// Revision-local direct edge identity.
    pub edge_ordinal: u64,
    /// Exact committed source output identity.
    pub output_id: String,
    /// Canonical source output checksum; values do not grant authority.
    pub output_checksum_sha256: String,
    /// Exact successful source dispatch identity.
    pub dispatch_identity: String,
    /// Durable source attempt/session relationship, without owner receipt payloads.
    pub execution: WorkflowExecutionSessionLink,
}

/// Explicit coverage outcome; absence is never evidence of no execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "reference", rename_all = "snake_case")]
pub enum DirectContributorObservation {
    /// Exact ordinary direct contributor was proved.
    Available(Box<DirectContributorReference>),
    /// Topology or transform requires a stronger provenance proof.
    Unsupported,
    /// Required current admitted identity, output or terminal execution is unavailable.
    ProvenanceUnavailable,
}

/// Prove the current ordinary direct contributor under authenticated execution authority.
///
/// Supports only the initial unchanged graph, one untransformed direct incoming edge,
/// and nonparallel execution nodes. Generated delegation and revised graphs do not
/// silently inherit evidence access. No source/session selection is accepted.
/// Reads at most two incoming edges, two current activations per endpoint, one latest
/// source attempt, one exact output and one exact session link; no replay or writes.
///
/// # Errors
/// Rejects forged/stale execution links, foreign authority, malformed durable records,
/// unsupported storage contracts and database failures.
pub async fn direct_contributor(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    caller: &WorkflowExecutionSessionLink,
    expected_revision: u64,
) -> Result<DirectContributorObservation, Error> {
    crate::validate_execution_session_link(caller).map_err(|_| Error::InvalidAdmission)?;
    let tx = db.begin_transaction().await?;
    super::execution_graph::verify_execution(&*tx, owner, caller).await?;
    let revision = super::graph_pages::revision(&*tx, &caller.run_id).await?;
    let result = if revision != expected_revision || revision != super::graph::INITIAL_REVISION {
        DirectContributorObservation::ProvenanceUnavailable
    } else {
        proof::prove(&*tx, caller, revision).await?
    };
    tx.commit().await?;
    Ok(result)
}

/// Bounded execution lineage rooted in one authenticated introducing publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedContributorPage {
    /// Exact introducing mutation, not a caller-selected candidate.
    pub mutation_id: String,
    /// Original source activation retained by publication.
    pub source_activation_id: String,
    /// Exact execution references; inclusion does not certify integration.
    pub contributors: Vec<DirectContributorReference>,
    /// Exclusive node cursor for this immutable scope.
    pub next_node_id: Option<String>,
}

/// Generated scope proof outcome; unsupported history is never approximated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GeneratedContributorObservation {
    /// Authenticated bounded lineage.
    Available(Box<GeneratedContributorPage>),
    /// Topology exceeds the currently supported proof.
    Unsupported,
    /// Required immutable provenance is unavailable.
    ProvenanceUnavailable,
}

/// Read a bounded page of exact generated execution lineage for the caller.
///
/// Supports an unchanged first introducing revision and first-generation source,
/// workers, joins, continuation and consumer only. All authority comes from the
/// immutable publication/candidate and exact activation records, never value search.
/// # Errors
/// Rejects invalid limits/cursors, stale callers/owners, cancellation and damaged storage.
pub async fn generated_contributors(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    caller: &WorkflowExecutionSessionLink,
    expected_revision: u64,
    after_node_id: Option<&str>,
    limit: u16,
) -> Result<GeneratedContributorObservation, Error> {
    if !(1..=100).contains(&limit) {
        return Err(Error::InvalidAdmission);
    }
    if let Some(cursor) = after_node_id {
        crate::validate_id("contributor cursor", cursor).map_err(|_| Error::InvalidAdmission)?;
    }
    crate::validate_execution_session_link(caller).map_err(|_| Error::InvalidAdmission)?;
    let tx = db.begin_transaction().await?;
    super::execution_graph::verify_execution(&*tx, owner, caller).await?;
    if super::safety::cancellation_requested_at(&*tx, &caller.run_id)
        .await?
        .is_some()
    {
        return Err(Error::InvalidAdmission);
    }
    let revision = super::graph_pages::revision(&*tx, &caller.run_id).await?;
    let result = if revision != expected_revision || revision != 2 {
        GeneratedContributorObservation::ProvenanceUnavailable
    } else {
        generated::prove(&*tx, caller, after_node_id, limit).await?
    };
    tx.commit().await?;
    Ok(result)
}

pub(super) fn decode<T: serde::de::DeserializeOwned>(
    row: &switchy_database::Row,
    field: &str,
) -> Result<T, Error> {
    match row.get(field) {
        Some(DatabaseValue::String(json)) if json.len() <= super::MAX_ADMISSION_BYTES => {
            serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)
        }
        _ => Err(Error::MaintenanceRequired),
    }
}

pub(super) async fn current_activation(
    db: &dyn Database,
    run: &str,
    node: &str,
) -> Result<Option<switchy_database::Row>, Error> {
    let mut rows = db
        .select("workflow_activations_v2")
        .where_eq("run_id", run)
        .where_eq("node_id", node)
        .where_eq("current", true)
        .limit(2)
        .execute(db)
        .await?;
    if rows.len() > 1 {
        return Err(Error::MaintenanceRequired);
    }
    Ok(rows.pop())
}

pub(super) fn text(row: &switchy_database::Row, field: &str) -> Result<String, Error> {
    match row.get(field) {
        Some(DatabaseValue::String(value)) if value.len() <= super::MAX_ADMISSION_BYTES => {
            Ok(value)
        }
        _ => Err(Error::MaintenanceRequired),
    }
}
