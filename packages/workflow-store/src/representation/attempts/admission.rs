//! Shared read-only fresh-leaf predicates. Ready is never authorization or owner liveness.
use super::{DatabaseValue, Error, FilterableQuery};
use switchy_database::DatabaseTransaction;
pub(super) async fn ensure_fresh(
    tx: &dyn DatabaseTransaction,
    prepared: &crate::PreparedAttempt,
) -> Result<(), Error> {
    if prepared.attempt == 1
        && matches!(
            fresh_leaf_in(
                tx,
                &prepared.run_id,
                &prepared.node_id,
                &prepared.activation_id,
                prepared.prepared_at_ms
            )
            .await?,
            FreshLeafAdmission::Blocked(_)
        )
    {
        return Err(Error::CapacityUnavailable);
    }
    Ok(())
}

/// Canonical blocker observed in this snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshLeafBlock {
    /// No durable execution authority is installed.
    MissingAuthority,
    /// Run is terminal.
    Terminal,
    /// Activation is not current unresolved work.
    Activation,
    /// Explicit user pause.
    Paused,
    /// Run or activation cancellation intent.
    Cancellation,
    /// Outstanding recovery.
    Recovery,
    /// Controller failure.
    ControlFailure,
    /// Deadline reached.
    Deadline,
    /// Local lifetime node allowance exhausted.
    Allowance,
    /// Local concurrent attempts filled.
    Concurrency,
    /// Canonical resource lease conflict.
    Resource,
    /// Root lifetime allowance exhausted.
    RootAllowance,
}
/// Scope whose full admission cannot be proven by this bounded fresh-leaf predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshLeafUnknown {
    /// Virtual member or unsupported node/controller kind.
    UnsupportedNode,
    /// Existing attempt requires exact duplicate/retry semantics, not fresh admission.
    ExistingAttempt,
}
/// Durable eligibility only; callers separately authorize and verify live execution ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshLeafAdmission {
    /// All covered canonical predicates pass.
    Ready,
    /// Exact canonical blocker.
    Blocked(FreshLeafBlock),
    /// This helper cannot establish readiness for this scope.
    Unknown(FreshLeafUnknown),
}
/// Inspect fresh Task/Agent/PluginBlock admission in the caller's snapshot without mutation.
/// # Errors
/// Rejects unsupported storage, corrupt policy/authority/lineage, missing run and database errors.
pub async fn fresh_leaf_in(
    tx: &dyn DatabaseTransaction,
    run: &str,
    node: &str,
    activation: &str,
    now_ms: u64,
) -> Result<FreshLeafAdmission, Error> {
    use FreshLeafAdmission::{Blocked, Unknown};
    use FreshLeafBlock as B;
    super::super::verified(tx).await?;
    let root = super::run(tx, run).await?;
    match root.get("authority_json") {
        Some(DatabaseValue::Null) => return Ok(Blocked(B::MissingAuthority)),
        Some(DatabaseValue::String(json)) => {
            serde_json::from_str::<crate::WorkflowExecutionAuthority>(&json)
                .map_err(|_| Error::MaintenanceRequired)?;
        }
        _ => return Err(Error::MaintenanceRequired),
    }
    if super::text(&root, "status")? != "paused" {
        super::super::runs::RunState::decode(&super::text(&root, "status")?)?;
        return Ok(Blocked(B::Terminal));
    }
    let revision = super::super::graph_pages::revision(tx, run).await?;
    let Some(definition) = super::super::graph_pages::node(tx, run, revision, node).await? else {
        return Ok(Unknown(FreshLeafUnknown::UnsupportedNode));
    };
    if !matches!(
        definition.kind,
        bcode_workflow::NodeKind::Task
            | bcode_workflow::NodeKind::Agent
            | bcode_workflow::NodeKind::PluginBlock
    ) {
        return Ok(Unknown(FreshLeafUnknown::UnsupportedNode));
    }
    if !super::super::graph::is_ready_activation(tx, run, node, activation).await? {
        return Ok(Blocked(B::Activation));
    }
    if !tx
        .select(super::TABLE)
        .columns(&["dispatch_identity"])
        .where_eq("run_id", run)
        .where_eq("activation_id", activation)
        .limit(1)
        .execute(tx)
        .await?
        .is_empty()
    {
        return Ok(Unknown(FreshLeafUnknown::ExistingAttempt));
    }
    let blocks = super::super::safety::admission_blocks(tx, run).await?;
    let blocker = if blocks.cancelled {
        Some(B::Cancellation)
    } else if blocks.user_paused {
        Some(B::Paused)
    } else if blocks.recovery {
        Some(B::Recovery)
    } else if blocks.control_failure {
        Some(B::ControlFailure)
    } else {
        None
    };
    if let Some(block) = blocker {
        return Ok(Blocked(block));
    }
    match super::ensure_activation_admission(tx, run, activation).await {
        Ok(()) => {}
        Err(Error::CapacityUnavailable) => return Ok(Blocked(B::Cancellation)),
        Err(error) => return Err(error),
    }
    let baseline = super::super::admission_projection::limits(&root)?;
    let limits = super::super::allowance::effective_limits(tx, run, &baseline).await?;
    if limits
        .deadline_at_ms
        .is_some_and(|deadline| now_ms >= deadline)
    {
        return Ok(Blocked(B::Deadline));
    }
    let consumed = u64::try_from(super::integer(&root, "consumed")?)
        .map_err(|_| Error::MaintenanceRequired)?;
    let occupied = u64::try_from(super::integer(&root, "active_attempts")?)
        .map_err(|_| Error::MaintenanceRequired)?;
    if consumed >= limits.node_execution_cap {
        return Ok(Blocked(B::Allowance));
    }
    if limits
        .concurrency_cap
        .is_some_and(|cap| occupied >= u64::from(cap))
    {
        return Ok(Blocked(B::Concurrency));
    }
    if super::super::safety::resource_conflict(tx, run, &definition.resources).await? {
        return Ok(Blocked(B::Resource));
    }
    match super::super::hierarchy::available_root_budget(tx, run).await {
        Ok(_) => {}
        Err(Error::CapacityUnavailable) => return Ok(Blocked(B::RootAllowance)),
        Err(error) => return Err(error),
    }
    Ok(FreshLeafAdmission::Ready)
}
