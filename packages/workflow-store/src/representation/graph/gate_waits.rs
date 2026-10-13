//! Canonical gate waits observed without execution authority.
use super::{Database, Error, FilterableQuery, NodeKind};
#[cfg(test)]
mod tests;

/// Supported interactive gate kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateKind {
    /// Schema-checked supplied input.
    Input,
    /// Explicit approval or denial.
    Approval,
}
/// Exact current unresolved gate activation; mutation must revalidate identity and revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateWait {
    /// Canonical node ID.
    pub node_id: String,
    /// Canonical activation ID.
    pub activation_id: String,
    /// Canonical activation creation time, not the observation time.
    pub requested_at_ms: u64,
    /// Canonical retained gate input, distinct from the schema of the supplied response.
    pub input: serde_json::Value,
    /// Exact gate kind.
    pub kind: GateKind,
    /// Declared prompt, absent when none was authored.
    pub prompt: Option<String>,
    /// Supplied input schema; approval is a decision, not an input value.
    pub response_schema: Option<bcode_workflow::ValueSchema>,
}
/// Bounded scan page. Empty gates with a continuation does not mean no waits exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatePage {
    /// Same-snapshot graph revision for action targets and continuation.
    pub revision: u64,
    /// Canonically unresolved gates found within this scan slice.
    pub gates: Vec<GateWait>,
    /// Exclusive last inspected ready node when lookahead finds more work.
    pub next_node_id: Option<String>,
}
/// Read the first bounded gate scan page.
/// # Errors
/// Rejects invalid bounds, unsupported storage, malformed gate configuration or missing runs.
pub async fn page(db: &dyn Database, run: &str, limit: u16) -> Result<GatePage, Error> {
    page_after(db, run, None, None, limit).await
}
/// Continue a bounded gate scan at an exact graph revision in one read transaction.
/// No authority is granted, and cancellation/terminal/control-failure states expose no actions.
/// # Errors
/// Rejects stale revision, unpinned continuation, invalid bounds and storage failures.
pub async fn page_after(
    db: &dyn Database,
    run: &str,
    expected_revision: Option<u64>,
    after: Option<&str>,
    limit: u16,
) -> Result<GatePage, Error> {
    let tx = db.begin_transaction().await?;
    let result = page_in(&*tx, run, expected_revision, after, limit).await?;
    tx.commit().await?;
    Ok(result)
}

/// Read canonical gate waits in the caller's existing snapshot without mutation.
/// # Errors
/// Rejects invalid bounds, stale revisions, missing runs and incompatible state.
pub async fn page_in(
    tx: &dyn switchy_database::DatabaseTransaction,
    run: &str,
    expected_revision: Option<u64>,
    after: Option<&str>,
    limit: u16,
) -> Result<GatePage, Error> {
    if !(1..=100).contains(&limit) || (after.is_some() && expected_revision.is_none()) {
        return Err(Error::InvalidAdmission);
    }
    super::super::verified(tx).await?;
    let revision = super::super::graph_pages::revision(tx, run).await?;
    if expected_revision.is_some_and(|v| v != revision) {
        return Err(Error::InvalidAdmission);
    }
    let roots = tx
        .select(super::RUNS)
        .columns(&["status"])
        .where_eq("run_id", run)
        .limit(1)
        .execute(tx)
        .await?;
    let root = roots.first().ok_or(Error::InvalidAdmission)?;
    let status = super::super::runs::RunState::decode(&super::text(root, "status")?)?;
    let blocks = super::super::safety::admission_blocks(tx, run).await?;
    let mut result = GatePage {
        revision,
        gates: Vec::new(),
        next_node_id: None,
    };
    if status != super::super::runs::RunState::Paused
        || blocks.cancelled
        || blocks.control_failure
        || blocks.recovery
        || blocks.user_paused
    {
        return Ok(result);
    }
    let activations = super::ready_in(tx, run, revision, after, limit).await?;
    if let Some(last) = activations.last()
        && !super::ready_in(tx, run, revision, Some(&last.node_id), 1)
            .await?
            .is_empty()
    {
        result.next_node_id = Some(last.node_id.clone());
    }
    for activation in activations {
        if !super::is_ready_activation(tx, run, &activation.node_id, &activation.activation_id)
            .await?
        {
            continue;
        }
        let node = super::execution_node(tx, run, &activation.node_id).await?;
        let kind = match node.kind {
            NodeKind::Input => GateKind::Input,
            NodeKind::Approval => GateKind::Approval,
            _ => continue,
        };
        let prompt = match node.configuration.get("prompt") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(v)) => Some(v.clone()),
            _ => return Err(Error::MaintenanceRequired),
        };
        let response_schema = if kind == GateKind::Input {
            Some(match node.configuration.get("input_resolution") {
                Some(value) => {
                    let resolution = serde_json::from_value::<
                        bcode_workflow::WorkflowInputResolution,
                    >(value.clone())
                    .map_err(|_| Error::MaintenanceRequired)?;
                    if resolution.version != 1 || resolution.transform.output != node.output {
                        return Err(Error::MaintenanceRequired);
                    }
                    resolution
                        .transform
                        .validate()
                        .map_err(|_| Error::MaintenanceRequired)?;
                    jsonschema::validator_for(&resolution.supplied.schema)
                        .map_err(|_| Error::MaintenanceRequired)?;
                    resolution.supplied
                }
                None => node.output,
            })
        } else {
            None
        };
        result.gates.push(GateWait {
            node_id: activation.node_id,
            activation_id: activation.activation_id,
            requested_at_ms: activation.created_at_ms,
            input: activation.input,
            kind,
            prompt,
            response_schema,
        });
    }
    Ok(result)
}
