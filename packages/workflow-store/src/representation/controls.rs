//! Versioned product controls. Callers authorize intent before invoking these fenced mutations.
use super::{
    DataType, Database, DatabaseValue, FilterableQuery, RUNS, RepresentationError as Error,
    TransactionMode, column, create_table,
};
use crate::WorkflowExecutionAuthority;
use serde::{Deserialize, Serialize};
const PAUSE: &str = "workflow_user_pause_v2";
#[cfg(test)]
mod tests;

/// Explicit user pause, distinct from the runnable persisted `paused` run status.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PauseRequest {
    /// Supported operation version is one.
    pub version: u32,
    /// Canonical run identity.
    pub run_id: String,
    /// Exact current graph revision.
    pub expected_revision: u64,
    /// True pauses admission; false resumes without resetting counters or cancellation.
    pub paused: bool,
}
/// Exact gate resolution, never a search for the next waiting node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateRequest {
    /// Supported operation version is one.
    pub version: u32,
    /// Canonical run identity.
    pub run_id: String,
    /// Canonical gate node.
    pub node_id: String,
    /// Exact activation (including repeat generation).
    pub activation_id: String,
    /// Exact current graph revision.
    pub expected_revision: u64,
    /// Explicit decision; denial is never represented as successful output.
    pub decision: GateDecision,
}
/// Typed resolution semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GateDecision {
    /// Provide schema-checked input to an input gate.
    Input(serde_json::Value),
    /// Grant an approval gate.
    Approve,
    /// Deny an approval gate without pretending outstanding effects were undone.
    Deny,
}
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(PAUSE)
        .column(column("run_id", DataType::Text, false))
        .primary_key("run_id")
        .execute(db)
        .await?;
    Ok(())
}
/// Read explicit user pause without acquiring execution authority.
/// # Errors
/// Rejects unsupported storage and database failures.
pub async fn user_paused(db: &dyn Database, run: &str) -> Result<bool, Error> {
    super::verified(db).await?;
    Ok(!db
        .select(PAUSE)
        .where_eq("run_id", run)
        .limit(1)
        .execute(db)
        .await?
        .is_empty())
}
/// Set user pause under exact graph revision and durable execution authority.
/// Existing attempts may settle; resuming neither resets allowance nor clears cancellation.
/// # Errors
/// Rejects unknown versions, stale ownership/revision, terminal runs and storage failures.
pub async fn set_pause(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    request: &PauseRequest,
) -> Result<bool, Error> {
    if request.version != 1 {
        return Err(Error::InvalidAdmission);
    }
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::verified(&*tx).await?;
    let roots = tx
        .select(RUNS)
        .where_eq("run_id", request.run_id.as_str())
        .limit(1)
        .execute(&*tx)
        .await?;
    let root = roots.first().ok_or(Error::InvalidAdmission)?;
    if root.get("authority_json")
        != Some(DatabaseValue::String(
            serde_json::to_string(owner).map_err(|_| Error::InvalidAdmission)?,
        ))
    {
        return Err(Error::AuthorityConflict);
    }
    if root.get("status") != Some(DatabaseValue::String("paused".into()))
        || super::graph_pages::revision(&*tx, &request.run_id).await? != request.expected_revision
    {
        return Err(Error::InvalidAdmission);
    }
    let paused = user_paused(&*tx, &request.run_id).await?;
    if request.paused && !paused {
        tx.insert(PAUSE)
            .value("run_id", request.run_id.as_str())
            .execute(&*tx)
            .await?;
    }
    if !request.paused && paused {
        tx.delete(PAUSE)
            .where_eq("run_id", request.run_id.as_str())
            .execute(&*tx)
            .await?;
    }
    tx.commit().await?;
    Ok(paused != request.paused)
}
/// Resolve exactly one waiting gate through the canonical transactional gate implementation.
/// # Errors
/// Rejects unknown versions, wrong gate kind, conflicting duplicates, invalid input,
/// stale activation/revision/owner and persistence failures.
pub async fn resolve_gate(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    request: &GateRequest,
) -> Result<super::graph::gates::GateResolution, Error> {
    if request.version != 1 {
        return Err(Error::InvalidAdmission);
    }
    match &request.decision {
        GateDecision::Deny => {
            super::graph::gates::deny_observed(
                db,
                owner,
                &request.run_id,
                &request.node_id,
                &request.activation_id,
                request.expected_revision,
            )
            .await
        }
        decision => {
            let input = match decision {
                GateDecision::Input(value) => Some(value),
                _ => None,
            };
            super::graph::gates::resolve_observed(
                db,
                owner,
                &request.run_id,
                &request.node_id,
                &request.activation_id,
                request.expected_revision,
                input,
            )
            .await
        }
    }
}
