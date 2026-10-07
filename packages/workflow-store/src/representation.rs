//! Versioned run admission on one caller-owned async database transaction.
//!
//! This representation stores canonical admission only. It deliberately grants no execution
//! authority until its complete lifecycle implementation is available.
use switchy_database::query::FilterableQuery;
use switchy_database::schema::{Column, DataType, create_index, create_table};
use switchy_database::{Database, DatabaseValue, TransactionMode};

mod admission_policy;
mod admission_projection;
mod registered;
pub use registered::{admit_registered, admit_registered_observed};
/// Monotonic execution and retry allowance controls.
pub mod allowance;
/// Fenced attempt ledger; not permission to dispatch unfinished representation lifecycles.
pub mod attempts;
/// Versioned user pause and exact gate controls.
pub mod controls;
#[cfg(test)]
mod deployment_tests;
pub use admission_policy::GraphAdmissionPolicy;
#[cfg(test)]
mod coexistence_tests;
/// Canonical bounded scheduling explanations.
pub mod diagnostics;
/// Authenticated execution graph snapshots and exact staged publication.
pub mod execution_graph;
/// Canonical static activation graphs and deterministic settlement.
pub mod graph;
/// Exact endpoint edge pages and graph boundary metadata.
pub mod graph_edges;
/// Exact retained node pages and revision scalars.
pub mod graph_pages;
/// Exact-revision pending topology publication.
pub mod graph_revision;
mod guards;
/// Root allowance and child-lineage accounting.
pub mod hierarchy;
/// Exact activation observation metadata.
pub mod inspection_activations;
/// Bounded canonical attempt inspection.
pub mod inspection_attempts;
/// Bounded canonical execution-session links.
pub mod inspection_sessions;
/// Independently versioned opt-in transactional transition observations.
pub mod journal;
/// Bounded lookup, association detachment and canonical terminalization.
pub mod runs;
/// Cancellation intent and transactional resource safety.
pub mod safety;
/// Same-snapshot bounded scheduling observations without execution authority.
pub mod scheduling;
#[cfg(test)]
mod tests;

const RUNS: &str = "workflow_runs_v2";
pub(super) const EXECUTION_VERSION: i64 = 30;
const CONTRACT: &str = "workflow_representation_contract";
const MAX_ADMISSION_BYTES: usize = 1_048_576;

/// Admission/storage errors with no implicit compatibility fallback.
#[derive(Debug, thiserror::Error)]
pub enum RepresentationError {
    /// The execution graph lifecycle is not yet implemented for this operation.
    #[error("workflow representation lifecycle unsupported")]
    UnsupportedLifecycle,
    /// Current durable owner does not match the caller.
    #[error("workflow representation authority conflict")]
    AuthorityConflict,
    /// Deadline, total allowance or concurrency prevents reservation.
    #[error("workflow representation capacity unavailable")]
    CapacityUnavailable,
    /// The legacy surface is not the explicitly supported compatibility contract.
    #[error("workflow representation requires compatibility2 schema42 or newer")]
    UnsupportedStore,
    /// Unknown or damaged new representation; preserve it for explicit maintenance.
    #[error("workflow representation requires maintenance")]
    MaintenanceRequired,
    /// Input is invalid or exceeds the bounded admission envelope.
    #[error("invalid workflow representation admission")]
    InvalidAdmission,
    /// Database failure. Applications must normalize this before public presentation.
    #[error("workflow representation persistence failed")]
    Database(#[from] switchy_database::DatabaseError),
}

/// Canonical identity location; absence never authorizes fallback from damaged storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunRepresentation {
    /// Existing capped legacy surface.
    Legacy,
    /// New admission representation; lifecycle dispatch is not implemented.
    Version2,
}

/// Explicit reason why a persisted admission cannot execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchAvailability {
    /// Agent leaves and tested branch/repeat/input/approval/wait-all controls only.
    AgentControlLifecycle,
    /// Definition or external owner integration is unsupported.
    UnsupportedLifecycle,
}

/// Admission receipt is not an execution receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepresentationAdmission {
    /// Stable canonical identity.
    pub run_id: String,
    /// Whether an identical admission had already committed.
    pub already_present: bool,
    /// The executable lifecycle remains unavailable.
    pub dispatch: DispatchAvailability,
}

fn column(name: &str, data_type: DataType, nullable: bool) -> Column {
    Column {
        name: name.into(),
        data_type,
        nullable,
        auto_increment: false,
        default: None,
    }
}

async fn compatible(db: &dyn Database) -> Result<(), RepresentationError> {
    let contract = db
        .select("workflow_storage_compatibility")
        .columns(&["compatibility"])
        .where_eq("contract_id", 1_i64)
        .limit(1)
        .execute(db)
        .await?;
    let revision = db
        .select("workflow_store_contract")
        .columns(&["schema_version"])
        .where_eq("contract_id", 1_i64)
        .limit(1)
        .execute(db)
        .await?;
    if contract.first().and_then(|r| r.get("compatibility")) != Some(DatabaseValue::Int64(2))
        || !matches!(revision.first().and_then(|r| r.get("schema_version")), Some(DatabaseValue::Int64(n)) if n >= 42)
    {
        return Err(RepresentationError::UnsupportedStore);
    }
    Ok(())
}

async fn verified(db: &dyn Database) -> Result<(), RepresentationError> {
    compatible(db).await?;
    let rows = db
        .select(CONTRACT)
        .columns(&["version"])
        .where_eq("representation", 2_i64)
        .limit(1)
        .execute(db)
        .await?;
    if rows.first().and_then(|r| r.get("version")) != Some(DatabaseValue::Int64(EXECUTION_VERSION))
    {
        return Err(RepresentationError::MaintenanceRequired);
    }
    if !db.verify_write_guards(guards::all()).await? {
        return Err(RepresentationError::MaintenanceRequired);
    }
    Ok(())
}

/// Install the additive admission schema and bidirectional old-writer guards atomically.
/// Does not change the legacy compatibility marker or any legacy run.
///
/// # Errors
/// Rejects incompatible stores, conflicting guards, unknown versions and storage failures.
pub async fn install(db: &dyn Database) -> Result<(), RepresentationError> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    compatible(&*tx).await?;
    create_table(CONTRACT)
        .if_not_exists(true)
        .column(column("representation", DataType::Int, false))
        .primary_key("representation")
        .column(column("version", DataType::Int, false))
        .execute(&*tx)
        .await?;
    let existing = tx
        .select(CONTRACT)
        .columns(&["version"])
        .where_eq("representation", 2_i64)
        .limit(1)
        .execute(&*tx)
        .await?;
    if !existing.is_empty() {
        verified(&*tx).await?;
        tx.commit().await?;
        return Ok(());
    }
    // Do not adopt a pre-existing unversioned table: CREATE without IF NOT EXISTS fails closed.
    let mut table = create_table(RUNS)
        .column(column("run_id", DataType::Text, false))
        .primary_key("run_id");
    for name in ["owner_plugin_id", "workflow_kind", "scope_key"] {
        table = table.column(column(name, DataType::Text, true));
    }
    table
        .column(column("single_active", DataType::Bool, false))
        .column(column("status", DataType::Text, false))
        .column(column("updated_at_ms", DataType::Int, false))
        .column(column("association_session_id", DataType::Text, true))
        .column(column("admission_json", DataType::Text, false))
        .column(column("definition_json", DataType::Text, false))
        .column(column("definition_sha256", DataType::Text, false))
        .column(column("authority_json", DataType::Text, true))
        .column(column("consumed", DataType::Int, false))
        .column(column("active_attempts", DataType::Int, false))
        .execute(&*tx)
        .await?;
    attempts::install(&*tx).await?;
    graph::install(&*tx).await?;
    graph_revision::install(&*tx).await?;
    graph_pages::install(&*tx).await?;
    graph_edges::install(&*tx).await?;
    execution_graph::install(&*tx).await?;
    safety::install(&*tx).await?;
    controls::install(&*tx).await?;
    allowance::install(&*tx).await?;
    hierarchy::install(&*tx).await?;
    create_index("idx_workflow_runs_v2_binding")
        .table(RUNS)
        .columns(vec![
            "owner_plugin_id",
            "workflow_kind",
            "scope_key",
            "status",
        ])
        .execute(&*tx)
        .await?;
    for guard in guards::all() {
        tx.install_write_guard(guard).await?;
    }
    tx.insert(CONTRACT)
        .value("representation", 0_i64)
        .value("version", 1_i64)
        .execute(&*tx)
        .await?;
    tx.insert(CONTRACT)
        .value("representation", 2_i64)
        .value("version", EXECUTION_VERSION)
        .execute(&*tx)
        .await?;
    journal::install_in(&*tx).await?;
    verified(&*tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn identity_in(
    db: &dyn Database,
    run_id: &str,
) -> Result<Option<RunRepresentation>, RepresentationError> {
    let mut found = None;
    for (table, representation) in [
        ("workflow_runs", RunRepresentation::Legacy),
        (RUNS, RunRepresentation::Version2),
    ] {
        if !db
            .select(table)
            .columns(&["run_id"])
            .where_eq("run_id", run_id)
            .limit(1)
            .execute(db)
            .await?
            .is_empty()
        {
            if found.is_some() {
                return Err(RepresentationError::MaintenanceRequired);
            }
            found = Some(representation);
        }
    }
    Ok(found)
}

/// Resolve an identity using bounded exact lookups in a consistent transaction.
///
/// # Errors
/// Rejects missing guards, unsupported contracts, duplicate authority or database failures.
pub async fn identity(
    db: &dyn Database,
    run_id: &str,
) -> Result<Option<RunRepresentation>, RepresentationError> {
    super::validate_id("run_id", run_id).map_err(|_| RepresentationError::InvalidAdmission)?;
    let tx = db.begin_transaction().await?;
    verified(&*tx).await?;
    let result = identity_in(&*tx, run_id).await?;
    tx.commit().await?;
    Ok(result)
}

/// Persist a validated canonical admission without creating executable work.
///
/// The caller must authorize admission first. Supplied initial authority is persisted atomically.
/// Definition and input are stored here, never written into legacy-readable authored records.
///
/// # Errors
/// Rejects invalid/big input, conflicting identity, missing guards, incompatible storage, or
/// database-enforced binding/reservation conflicts. Identical requests are idempotent.
pub async fn admit(
    db: &dyn Database,
    run: &super::NewWorkflowRun,
    definition: &bcode_workflow::WorkflowDefinition,
) -> Result<RepresentationAdmission, RepresentationError> {
    admit_with_policy(db, run, definition, GraphAdmissionPolicy::default()).await
}
/// Admit with an explicit caller-selected whole-candidate work budget.
/// # Errors
/// Rejects policy-exceeding snapshots or any ordinary admission failure.
pub async fn admit_with_policy(
    db: &dyn Database,
    run: &super::NewWorkflowRun,
    definition: &bcode_workflow::WorkflowDefinition,
    policy: GraphAdmissionPolicy,
) -> Result<RepresentationAdmission, RepresentationError> {
    policy.check(
        serde_json::to_vec(definition)
            .map_err(|_| RepresentationError::InvalidAdmission)?
            .len(),
    )?;
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    let result = admit_in(&*tx, run, definition).await?;
    if !result.already_present {
        hierarchy::register_root(&*tx, run).await?;
    }
    tx.commit().await?;
    Ok(result)
}

pub(super) async fn admit_in(
    tx: &dyn switchy_database::DatabaseTransaction,
    run: &super::NewWorkflowRun,
    definition: &bcode_workflow::WorkflowDefinition,
) -> Result<RepresentationAdmission, RepresentationError> {
    validate_admission(run, definition)?;
    let graph_definition = definition;
    let admission =
        serde_json::to_string(run).map_err(|_| RepresentationError::InvalidAdmission)?;
    let definition =
        serde_json::to_string(definition).map_err(|_| RepresentationError::InvalidAdmission)?;
    let definition_sha256 = crate::sha256_hex(definition.as_bytes());
    if admission.len() > MAX_ADMISSION_BYTES {
        return Err(RepresentationError::InvalidAdmission);
    }
    verified(tx).await?;
    let location = identity_in(tx, &run.run_id).await?;
    if location == Some(RunRepresentation::Legacy) {
        return Err(RepresentationError::InvalidAdmission);
    }
    let already_present = location.is_some();
    if already_present {
        let rows = tx
            .select(RUNS)
            .columns(&["admission_json", "definition_json", "definition_sha256"])
            .where_eq("run_id", run.run_id.as_str())
            .limit(1)
            .execute(tx)
            .await?;
        if rows.first().and_then(|r| r.get("admission_json"))
            != Some(DatabaseValue::String(admission))
            || rows.first().and_then(|r| r.get("definition_json"))
                != Some(DatabaseValue::String(definition))
            || rows.first().and_then(|r| r.get("definition_sha256"))
                != Some(DatabaseValue::String(definition_sha256))
        {
            return Err(RepresentationError::InvalidAdmission);
        }
    } else {
        let text = |value: Option<&str>| {
            value.map_or(DatabaseValue::Null, |s| DatabaseValue::String(s.into()))
        };
        tx.insert(RUNS)
            .value("run_id", run.run_id.as_str())
            .value(
                "owner_plugin_id",
                text(run.binding.as_ref().map(|b| b.owner_plugin_id.as_str())),
            )
            .value(
                "workflow_kind",
                text(run.binding.as_ref().map(|b| b.workflow_kind.as_str())),
            )
            .value(
                "scope_key",
                text(run.binding.as_ref().map(|b| b.scope_key.as_str())),
            )
            .value(
                "single_active",
                run.binding.as_ref().is_some_and(|b| b.single_active),
            )
            .value("status", "paused")
            .value(
                "updated_at_ms",
                i64::try_from(run.created_at_ms)
                    .map_err(|_| RepresentationError::InvalidAdmission)?,
            )
            .value(
                "association_session_id",
                text(run.parent_session_id.as_deref()),
            )
            .value(
                "authority_json",
                run.execution_authority
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|_| RepresentationError::InvalidAdmission)?
                    .map_or(DatabaseValue::Null, DatabaseValue::String),
            )
            .value("consumed", 0_i64)
            .value("active_attempts", 0_i64)
            .value("admission_json", admission)
            .value("definition_sha256", definition_sha256)
            .value("definition_json", definition)
            .execute(tx)
            .await?;
        graph::initialize(tx, run, graph_definition).await?;
    }
    Ok(RepresentationAdmission {
        run_id: run.run_id.clone(),
        already_present,
        dispatch: DispatchAvailability::UnsupportedLifecycle,
    })
}

fn validate_admission(
    run: &super::NewWorkflowRun,
    definition: &bcode_workflow::WorkflowDefinition,
) -> Result<(), RepresentationError> {
    super::validate_run(run).map_err(|_| RepresentationError::InvalidAdmission)?;

    definition
        .validate()
        .map_err(|_| RepresentationError::InvalidAdmission)?;
    // Explicit persisted IDs are a supported contract. The application verifies their
    // registry binding; this boundary retains exact content rather than inventing one.
    if let Some((kind, _)) = run.definition_id.rsplit_once('@') {
        let exact = bcode_workflow::WorkflowDefinitionIdentity::for_definition(kind, definition)
            .map_err(|_| RepresentationError::InvalidAdmission)?;
        if exact.definition_id != run.definition_id
            || exact.definition_version != run.definition_version
        {
            return Err(RepresentationError::InvalidAdmission);
        }
    }
    super::validate_run_input(
        definition,
        run.input.as_ref().unwrap_or(&serde_json::Value::Null),
    )
    .map_err(|_| RepresentationError::InvalidAdmission)?;
    graph::validate(definition)?;
    Ok(())
}

/// Storage family selection does not grant execution readiness or perform installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionFamily {
    /// Compatible legacy baseline with no extension installed.
    Baseline42Installable,
    /// Exact extension and guards verified; dispatch capability is still separate.
    Installed,
    /// Historical nullable representation, preserved without conversion.
    Historical43,
}

/// Select a recognized storage family without mutating it.
/// # Errors
/// Rejects unknown global contracts, partial extension or unverifiable guard definitions.
pub async fn admission_family(db: &dyn Database) -> Result<AdmissionFamily, RepresentationError> {
    let tx = db.begin_transaction().await?;
    let rows = tx
        .select("workflow_storage_compatibility")
        .columns(&["compatibility"])
        .where_eq("contract_id", 1_i64)
        .limit(1)
        .execute(&*tx)
        .await?;
    match rows.first().and_then(|row| row.get("compatibility")) {
        Some(DatabaseValue::Int64(3)) => {
            let versions = tx
                .select("workflow_store_contract")
                .columns(&["schema_version"])
                .where_eq("contract_id", 1_i64)
                .limit(1)
                .execute(&*tx)
                .await?;
            if !matches!(versions.first().and_then(|row|row.get("schema_version")),Some(DatabaseValue::Int64(n)) if n>=43)
            {
                return Err(RepresentationError::UnsupportedStore);
            }
            if tx.table_exists(CONTRACT).await? || tx.table_exists(RUNS).await? {
                return Err(RepresentationError::MaintenanceRequired);
            }
            tx.commit().await?;
            return Ok(AdmissionFamily::Historical43);
        }
        Some(DatabaseValue::Int64(2)) => {}
        _ => return Err(RepresentationError::UnsupportedStore),
    }
    compatible(&*tx).await?;
    let contract = tx.table_exists(CONTRACT).await?;
    let runs = tx.table_exists(RUNS).await?;
    if !contract && !runs {
        tx.commit().await?;
        return Ok(AdmissionFamily::Baseline42Installable);
    }
    if !contract || !runs {
        return Err(RepresentationError::MaintenanceRequired);
    }
    verified(&*tx).await?;
    tx.commit().await?;
    Ok(AdmissionFamily::Installed)
}

/// Compatibility predicate for installed storage, not authorization to dispatch.
/// # Errors
/// Propagates unsupported/damaged storage-family errors.
pub async fn supports_admission(db: &dyn Database) -> Result<bool, RepresentationError> {
    Ok(admission_family(db).await? == AdmissionFamily::Installed)
}

/// Current executable capability, independent of storage installation or family.
#[must_use]
pub const fn dispatch_availability() -> DispatchAvailability {
    DispatchAvailability::UnsupportedLifecycle
}

/// Validate the tested Agent/control dispatch subset; storage installation alone is insufficient.
///
/// Supports exact Definition calls and wait-all fan-out with Agent/read-only block members.
/// Package/authored calls, mutating blocks and non-wait-all controllers remain excluded.
/// The application still authorizes every operation and verifies current execution ownership.
/// # Errors
/// Rejects malformed or unsupported graph semantics rather than silently broadening support.
pub fn definition_dispatch_availability(
    definition: &bcode_workflow::WorkflowDefinition,
) -> Result<DispatchAvailability, RepresentationError> {
    graph::validate(definition)?;
    if definition.nodes.values().any(|node| {
        if node.kind == bcode_workflow::NodeKind::Parallel {
            return !parallel_supported(node, definition);
        }
        if node.kind == bcode_workflow::NodeKind::FanOut {
            return !fan_out_supported(node);
        }
        if node.kind == bcode_workflow::NodeKind::WorkflowCall {
            return !definition_call_supported(node);
        }
        if node.kind == bcode_workflow::NodeKind::PluginBlock {
            return !read_only_block_supported(node);
        }
        !matches!(
            node.kind,
            bcode_workflow::NodeKind::Agent
                | bcode_workflow::NodeKind::Branch
                | bcode_workflow::NodeKind::Repeat
                | bcode_workflow::NodeKind::Input
                | bcode_workflow::NodeKind::Approval
        )
    }) {
        return Ok(DispatchAvailability::UnsupportedLifecycle);
    }
    Ok(DispatchAvailability::AgentControlLifecycle)
}

fn parallel_supported(
    node: &bcode_workflow::NodeDefinition,
    definition: &bcode_workflow::WorkflowDefinition,
) -> bool {
    let policy = match node.configuration.get("failure_policy") {
        None => bcode_workflow::ParallelFailurePolicy::WaitAll,
        Some(value) => {
            match serde_json::from_value::<bcode_workflow::ParallelFailurePolicy>(value.clone()) {
                Ok(policy) => policy,
                Err(_) => return false,
            }
        }
    };
    if policy == bcode_workflow::ParallelFailurePolicy::WaitAll {
        return true;
    }
    let Ok(members) = bcode_workflow::parallel_join_member_ids(node) else {
        return false;
    };
    !members.is_empty()
        && members.iter().all(|id| {
            definition
                .nodes
                .get(*id)
                .is_some_and(|member| member.kind == bcode_workflow::NodeKind::Agent)
        })
}

fn fan_out_supported(node: &bcode_workflow::NodeDefinition) -> bool {
    let Ok(config) = serde_json::from_value::<bcode_workflow::WorkflowFanOutConfiguration>(
        node.configuration.clone(),
    ) else {
        return false;
    };
    config.validate().is_ok()
        && config.failure_policy == bcode_workflow::ParallelFailurePolicy::WaitAll
        && match config.member_node.kind {
            bcode_workflow::NodeKind::Agent => true,
            bcode_workflow::NodeKind::PluginBlock => read_only_block_supported(&config.member_node),
            _ => false,
        }
}
fn definition_call_supported(node: &bcode_workflow::NodeDefinition) -> bool {
    let Ok(config) = serde_json::from_value::<bcode_workflow::WorkflowCallConfiguration>(
        node.configuration.clone(),
    ) else {
        return false;
    };
    config.validate().is_ok()
        && matches!(
            config.target,
            bcode_workflow::WorkflowCallTarget::Definition { .. }
        )
}

fn read_only_block_supported(node: &bcode_workflow::NodeDefinition) -> bool {
    let Ok(block) = serde_json::from_value::<bcode_workflow::WorkflowBlockDefinition>(
        node.configuration.clone(),
    ) else {
        return false;
    };
    block.validate().is_ok()
        && block.effect == bcode_workflow::WorkflowBlockEffect::ReadOnly
        && matches!(
            block.authorization.capability,
            bcode_workflow::WorkflowToolCapability::Disabled
                | bcode_workflow::WorkflowToolCapability::ReadOnly
        )
        && !block.authorization.explicit_grant_required
        && !block.preparation_required
}
