//! Canonical root allowance counters; all mutation composes into caller-owned transactions.
use super::{Database, DatabaseValue, RUNS, RepresentationError as Error, column};
use crate::NewWorkflowRun;
mod discovery;
pub use discovery::{ChildPage, ParentContext, children, children_in, parent_context};
use switchy_database::{
    DatabaseTransaction, Row,
    query::FilterableQuery,
    schema::{DataType, create_table},
};
const LINEAGE: &str = "workflow_lineage_v2";
const ROOTS: &str = "workflow_root_allowances_v2";

pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(LINEAGE)
        .column(column("run_id", DataType::Text, false))
        .primary_key("run_id")
        .column(column("root_run_id", DataType::Text, false))
        .column(column("parent_run_id", DataType::Text, true))
        .column(column("depth", DataType::BigInt, false))
        .column(column("link_json", DataType::Text, true))
        .column(column("parent_node_id", DataType::Text, true))
        .column(column("parent_activation_id", DataType::Text, true))
        .column(column("parent_attempt", DataType::BigInt, true))
        .unique_columns([
            "parent_run_id",
            "parent_node_id",
            "parent_activation_id",
            "parent_attempt",
        ])
        .execute(db)
        .await?;
    switchy_database::schema::create_index("idx_workflow_lineage_parent_run")
        .table(LINEAGE)
        .columns(vec!["parent_run_id", "run_id"])
        .execute(db)
        .await?;
    create_table(ROOTS)
        .column(column("root_run_id", DataType::Text, false))
        .primary_key("root_run_id")
        .column(column("consumed", DataType::BigInt, false))
        .column(column("descendants", DataType::BigInt, false))
        .execute(db)
        .await?;
    Ok(())
}
fn text(row: &Row, key: &str) -> Result<String, Error> {
    match row.get(key) {
        Some(DatabaseValue::String(s)) => Ok(s),
        _ => Err(Error::MaintenanceRequired),
    }
}
fn integer(row: &Row, key: &str) -> Result<i64, Error> {
    match row.get(key) {
        Some(DatabaseValue::Int64(n)) if n >= 0 => Ok(n),
        _ => Err(Error::MaintenanceRequired),
    }
}
async fn exact(db: &dyn Database, table: &str, key: &str, id: &str) -> Result<Row, Error> {
    db.select(table)
        .where_eq(key, id)
        .limit(1)
        .execute(db)
        .await?
        .into_iter()
        .next()
        .ok_or(Error::MaintenanceRequired)
}
/// Register a newly admitted root in the same immediate admission transaction.
/// Missing lineage is never inferred at execution time.
pub(super) async fn register_root(
    tx: &dyn DatabaseTransaction,
    run: &NewWorkflowRun,
) -> Result<(), Error> {
    if !tx
        .select(LINEAGE)
        .where_eq("run_id", run.run_id.as_str())
        .limit(1)
        .execute(tx)
        .await?
        .is_empty()
    {
        return Err(Error::InvalidAdmission);
    }
    tx.insert(LINEAGE)
        .value("run_id", run.run_id.as_str())
        .value("root_run_id", run.run_id.as_str())
        .value("parent_run_id", DatabaseValue::Null)
        .value("link_json", DatabaseValue::Null)
        .value("depth", 1_i64)
        .execute(tx)
        .await?;
    tx.insert(ROOTS)
        .value("root_run_id", run.run_id.as_str())
        .value("consumed", 0_i64)
        .value("descendants", 0_i64)
        .execute(tx)
        .await?;
    Ok(())
}
/// Resolve a call against one exact immutable publication; never searches publications by member.
/// # Errors
/// Rejects unsupported targets, absent/unknown bindings and mismatched locked identities.
pub async fn resolve_call(
    db: &dyn Database,
    binding: Option<&crate::WorkflowRunPackageBinding>,
    target: &bcode_workflow::WorkflowCallTarget,
) -> Result<bcode_workflow::WorkflowDefinitionIdentity, Error> {
    use bcode_workflow::WorkflowCallTarget;
    if binding.is_some_and(|b| b.version != 1) {
        return Err(Error::UnsupportedLifecycle);
    }
    if let WorkflowCallTarget::Definition { identity } = target {
        return Ok(identity.clone());
    }
    let binding = binding.ok_or(Error::InvalidAdmission)?;
    let lock = super::admission_projection::publication(db, binding).await?;
    let member_id = match target {
        WorkflowCallTarget::PackageLocal { reference } => {
            reference.validate().map_err(|_| Error::InvalidAdmission)?;
            &reference.member_id
        }
        WorkflowCallTarget::PackageMember { member_id, .. } => member_id,
        _ => return Err(Error::UnsupportedLifecycle),
    };
    let member = lock
        .members
        .iter()
        .find(|m| &m.member_id == member_id)
        .ok_or(Error::InvalidAdmission)?;
    if let WorkflowCallTarget::PackageMember {
        definition_identity,
        ..
    } = target
        && definition_identity != &member.definition_identity
    {
        return Err(Error::InvalidAdmission);
    }
    Ok(member.definition_identity.clone())
}

async fn stable_child_retry(
    tx: &dyn DatabaseTransaction,
    child: &NewWorkflowRun,
    link: &crate::WorkflowRunLink,
    original: Option<&crate::RepresentedWorkflowAdmission>,
) -> Result<(NewWorkflowRun, crate::WorkflowRunLink), Error> {
    let mut stable_child = child.clone();
    let mut stable_link = link.clone();
    if let Some(original) = original {
        let lineage = exact(tx, LINEAGE, "run_id", &child.run_id).await?;
        let prior_link: crate::WorkflowRunLink =
            serde_json::from_str(&text(&lineage, "link_json")?)
                .map_err(|_| Error::MaintenanceRequired)?;
        stable_child.created_at_ms = original.admission.created_at_ms;
        stable_link.created_at_ms = prior_link.created_at_ms;
        let mut comparison = stable_child.clone();
        // Original admission authority is historical; current parent authority is checked below.
        // Returning an existing child neither transfers its ownership nor authorizes dispatch.
        comparison
            .execution_authority
            .clone_from(&original.admission.execution_authority);
        if comparison != original.admission || stable_link != prior_link {
            return Err(Error::InvalidAdmission);
        }
    }
    Ok((stable_child, stable_link))
}

/// Admit an exact compiled-definition child and its lineage atomically.
///
/// Package targets resolve only through the parent's immutable canonical lineage binding.
/// This never dispatches child work or infers authority from caller-supplied lineage.
/// # Errors
/// Rejects stale parent ownership, mismatched call target, invalid lineage, policy widening,
/// exhausted root allowance, conflicting admission or unsupported target resolution.
pub async fn admit_child(
    db: &dyn Database,
    owner: &crate::WorkflowExecutionAuthority,
    link: &crate::WorkflowRunLink,
    child: &NewWorkflowRun,
    definition: &bcode_workflow::WorkflowDefinition,
) -> Result<super::RepresentationAdmission, Error> {
    let tx = db
        .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
        .await?;
    super::verified(&*tx).await?;
    let revision = super::graph_pages::revision(&*tx, &link.parent_run_id).await?;
    let node = super::graph_pages::node(&*tx, &link.parent_run_id, revision, &link.parent_node_id)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    if node.kind != bcode_workflow::NodeKind::WorkflowCall {
        return Err(Error::InvalidAdmission);
    }
    let call: bcode_workflow::WorkflowCallConfiguration =
        serde_json::from_value(node.configuration.clone()).map_err(|_| Error::InvalidAdmission)?;
    call.validate().map_err(|_| Error::InvalidAdmission)?;
    if call.target != link.target {
        return Err(Error::InvalidAdmission);
    }
    let binding = super::admission_projection::package_binding(&*tx, &link.parent_run_id).await?;
    let existing = tx
        .select(RUNS)
        .columns(&["admission_json"])
        .where_eq("run_id", child.run_id.as_str())
        .limit(1)
        .execute(&*tx)
        .await?;
    let original = existing
        .first()
        .map(|row| {
            let json = text(row, "admission_json")?;
            super::admission_projection::admission(&json)
        })
        .transpose()?;
    let (stable_child, stable_link) =
        stable_child_retry(&*tx, child, link, original.as_ref()).await?;
    let child = &stable_child;
    let link = &stable_link;
    let identity = resolve_call(&*tx, binding.as_ref(), &call.target).await?;
    if identity.definition_id != child.definition_id
        || identity.definition_version != child.definition_version
    {
        return Err(Error::InvalidAdmission);
    }
    if super::graph::activation_id(&*tx, &link.parent_run_id, &link.parent_node_id)
        .await?
        .as_deref()
        != Some(link.parent_activation_id.as_str())
    {
        return Err(Error::InvalidAdmission);
    }
    let actual = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        identity.kind.clone(),
        definition,
    )
    .map_err(|_| Error::InvalidAdmission)?;
    if actual != identity {
        return Err(Error::InvalidAdmission);
    }
    if let bcode_workflow::WorkflowCallTarget::PackageLocal { reference } = &call.target {
        let lock = super::admission_projection::publication(
            &*tx,
            binding.as_ref().ok_or(Error::InvalidAdmission)?,
        )
        .await?;
        reference
            .resolve(&lock, definition)
            .map_err(|_| Error::InvalidAdmission)?;
    }
    register_child(&*tx, owner, link, child).await?;
    let result = if original.is_some() {
        if super::admission_projection::package_binding(&*tx, &child.run_id).await? != binding {
            return Err(Error::InvalidAdmission);
        }
        let row = exact(&*tx, RUNS, "run_id", &child.run_id).await?;
        let json = serde_json::to_string(definition).map_err(|_| Error::InvalidAdmission)?;
        if text(&row, "definition_json")? != json
            || text(&row, "definition_sha256")? != crate::sha256_hex(json.as_bytes())
        {
            return Err(Error::MaintenanceRequired);
        }
        super::RepresentationAdmission {
            run_id: child.run_id.clone(),
            already_present: true,
            dispatch: super::DispatchAvailability::UnsupportedLifecycle,
        }
    } else {
        super::admit_package_in(&*tx, child, definition, binding.as_ref()).await?
    };
    tx.commit().await?;
    Ok(result)
}

/// Validate and reserve a child lineage in the caller's authority-fenced admission transaction.
/// Child root/graph insertion must commit in this same transaction; this does not admit the child.
pub(super) async fn register_child(
    tx: &dyn DatabaseTransaction,
    owner: &crate::WorkflowExecutionAuthority,
    link: &crate::WorkflowRunLink,
    child: &NewWorkflowRun,
) -> Result<(), Error> {
    crate::validate_run_link(link, child).map_err(|_| Error::InvalidAdmission)?;
    let parent = exact(tx, RUNS, "run_id", &link.parent_run_id).await?;
    validate_child_authority(&parent, owner, child)?;
    super::safety::ensure_admission_allowed(tx, &link.parent_run_id).await?;
    let lineage = exact(tx, LINEAGE, "run_id", &link.parent_run_id).await?;
    if text(&lineage, "root_run_id")? != link.root_run_id
        || integer(&lineage, "depth")?.checked_add(1) != Some(i64::from(link.depth))
    {
        return Err(Error::InvalidAdmission);
    }
    let parent_request: NewWorkflowRun = serde_json::from_str(&text(&parent, "admission_json")?)
        .map_err(|_| Error::MaintenanceRequired)?;
    let parent_limits =
        super::allowance::effective_limits(tx, &link.parent_run_id, &parent_request.limits).await?;
    validate_inherited_limits(&parent_limits, &child.limits)?;
    let dispatch = crate::dispatch_identity(
        &link.parent_run_id,
        &link.parent_node_id,
        &link.parent_activation_id,
        link.parent_attempt,
    );
    let attempt = exact(tx, "workflow_attempts_v2", "dispatch_identity", &dispatch).await?;
    if attempt.get("outcome_json") != Some(DatabaseValue::Null) {
        return Err(Error::InvalidAdmission);
    }
    let prepared: crate::PreparedAttempt = serde_json::from_str(&text(&attempt, "prepared_json")?)
        .map_err(|_| Error::MaintenanceRequired)?;
    if prepared.dispatch_identity() != dispatch
        || prepared.run_id != link.parent_run_id
        || prepared.node_id != link.parent_node_id
        || prepared.activation_id != link.parent_activation_id
        || prepared.attempt != link.parent_attempt
    {
        return Err(Error::MaintenanceRequired);
    }
    let existing = tx
        .select(LINEAGE)
        .where_eq("run_id", child.run_id.as_str())
        .limit(1)
        .execute(tx)
        .await?;
    if let Some(existing) = existing.first() {
        if text(existing, "root_run_id")? != link.root_run_id
            || text(existing, "link_json")?
                != serde_json::to_string(link).map_err(|_| Error::InvalidAdmission)?
            || text(existing, "parent_run_id")? != link.parent_run_id
            || integer(existing, "depth")? != i64::from(link.depth)
        {
            return Err(Error::InvalidAdmission);
        }
        return Ok(());
    }
    let root = exact(tx, RUNS, "run_id", &link.root_run_id).await?;
    let request: NewWorkflowRun = serde_json::from_str(&text(&root, "admission_json")?)
        .map_err(|_| Error::MaintenanceRequired)?;
    let counters = exact(tx, ROOTS, "root_run_id", &link.root_run_id).await?;
    let descendants = integer(&counters, "descendants")?;
    let root_limits =
        super::allowance::effective_limits(tx, &link.root_run_id, &request.limits).await?;
    if descendants.cast_unsigned() >= u64::from(root_limits.descendant_cap)
        || integer(&counters, "consumed")?.cast_unsigned() >= root_limits.node_execution_cap
    {
        return Err(Error::CapacityUnavailable);
    }
    tx.insert(LINEAGE)
        .value("run_id", child.run_id.as_str())
        .value("root_run_id", link.root_run_id.as_str())
        .value("parent_run_id", link.parent_run_id.as_str())
        .value("parent_node_id", link.parent_node_id.as_str())
        .value("parent_activation_id", link.parent_activation_id.as_str())
        .value("parent_attempt", i64::from(link.parent_attempt))
        .value("depth", i64::from(link.depth))
        .value(
            "link_json",
            serde_json::to_string(link).map_err(|_| Error::InvalidAdmission)?,
        )
        .execute(tx)
        .await?;
    tx.update(ROOTS)
        .value(
            "descendants",
            descendants
                .checked_add(1)
                .ok_or(Error::CapacityUnavailable)?,
        )
        .where_eq("root_run_id", link.root_run_id.as_str())
        .execute(tx)
        .await?;
    Ok(())
}

fn validate_child_authority(
    parent: &Row,
    owner: &crate::WorkflowExecutionAuthority,
    child: &NewWorkflowRun,
) -> Result<(), Error> {
    if parent.get("authority_json")
        != Some(DatabaseValue::String(
            serde_json::to_string(owner).map_err(|_| Error::InvalidAdmission)?,
        ))
    {
        return Err(Error::AuthorityConflict);
    }
    let request: NewWorkflowRun = serde_json::from_str(&text(parent, "admission_json")?)
        .map_err(|_| Error::MaintenanceRequired)?;
    if text(parent, "status")? != "paused"
        || child.authorization_ceiling > request.authorization_ceiling
        || child.authorization_profile != request.authorization_profile
        || child.execution_authority.as_ref() != Some(owner)
    {
        return Err(Error::InvalidAdmission);
    }
    Ok(())
}

fn validate_inherited_limits(
    p: &crate::WorkflowRunLimits,
    c: &crate::WorkflowRunLimits,
) -> Result<(), Error> {
    if c.node_execution_cap > p.node_execution_cap
        || c.cycle_cap > p.cycle_cap
        || c.retry_cap > p.retry_cap
        || c.recursion_depth_cap > p.recursion_depth_cap
        || c.descendant_cap > p.descendant_cap
        || p.concurrency_cap
            .is_some_and(|cap| c.concurrency_cap.is_none_or(|v| v > cap))
        || p.deadline_at_ms
            .is_some_and(|deadline| c.deadline_at_ms.is_none_or(|v| v > deadline))
    {
        return Err(Error::InvalidAdmission);
    }
    Ok(())
}

/// Exact child observation; pending or failed children never supply a successful output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildResult {
    Pending,
    Completed(serde_json::Value),
    Failed,
    Cancelled,
}
/// Read a child result under parent ownership, without settling the parent or changing effects.
/// # Errors
/// Rejects stale authority, ambiguous lineage and malformed child terminal output.
pub async fn child_result(
    db: &dyn Database,
    owner: &crate::WorkflowExecutionAuthority,
    parent_run: &str,
    node: &str,
    activation: &str,
    attempt: u32,
) -> Result<Option<ChildResult>, Error> {
    let tx = db.begin_transaction().await?;
    let result = child_result_in(&*tx, owner, parent_run, node, activation, attempt).await?;
    tx.commit().await?;
    Ok(result)
}

/// Read exact child result within caller-owned transaction; does not commit.
/// # Errors
/// Rejects stale authority, ambiguous lineage or malformed child state.
pub async fn child_result_in(
    tx: &dyn DatabaseTransaction,
    owner: &crate::WorkflowExecutionAuthority,
    parent_run: &str,
    node: &str,
    activation: &str,
    attempt: u32,
) -> Result<Option<ChildResult>, Error> {
    super::verified(tx).await?;
    let parent = exact(tx, RUNS, "run_id", parent_run).await?;
    if text(&parent, "authority_json")?
        != serde_json::to_string(owner).map_err(|_| Error::InvalidAdmission)?
    {
        return Err(Error::AuthorityConflict);
    }
    let links = tx
        .select(LINEAGE)
        .where_eq("parent_run_id", parent_run)
        .where_eq("parent_node_id", node)
        .where_eq("parent_activation_id", activation)
        .where_eq("parent_attempt", i64::from(attempt))
        .limit(2)
        .execute(tx)
        .await?;
    if links.len() > 1 {
        return Err(Error::MaintenanceRequired);
    }
    let Some(row) = links.first() else {
        return Ok(None);
    };
    let link: crate::WorkflowRunLink =
        serde_json::from_str(&text(row, "link_json")?).map_err(|_| Error::MaintenanceRequired)?;
    if link.parent_run_id != parent_run
        || link.parent_node_id != node
        || link.parent_activation_id != activation
        || link.parent_attempt != attempt
        || link.child_run_id != text(row, "run_id")?
    {
        return Err(Error::MaintenanceRequired);
    }
    let child = exact(tx, RUNS, "run_id", &link.child_run_id).await?;
    let result = match text(&child, "status")?.as_str() {
        "paused" => ChildResult::Pending,
        "failed" => ChildResult::Failed,
        "cancelled" => ChildResult::Cancelled,
        "completed" => {
            let graph = exact(tx, "workflow_graphs_v2", "run_id", &link.child_run_id).await?;
            ChildResult::Completed(
                serde_json::from_str(&text(&graph, "terminal_json")?)
                    .map_err(|_| Error::MaintenanceRequired)?,
            )
        }
        _ => return Err(Error::MaintenanceRequired),
    };
    Ok(Some(result))
}

/// Consume one lifetime root execution allowance, after exact attempt deduplication.
/// Caller must roll back on any error and commit this with the attempt reservation.
pub(super) async fn consume_attempt(
    tx: &dyn DatabaseTransaction,
    run_id: &str,
) -> Result<(), Error> {
    let (root_id, consumed) = available_root_budget(tx, run_id).await?;
    tx.update(ROOTS)
        .value(
            "consumed",
            consumed.checked_add(1).ok_or(Error::CapacityUnavailable)?,
        )
        .where_eq("root_run_id", root_id)
        .where_eq("consumed", consumed)
        .execute(tx)
        .await?;
    Ok(())
}
/// Read the exact root allowance predicate shared by attempt consumption.
/// # Errors
/// Returns capacity unavailable, malformed lineage or persistence errors.
pub(super) async fn available_root_budget(
    tx: &dyn DatabaseTransaction,
    run_id: &str,
) -> Result<(String, i64), Error> {
    let lineage = exact(tx, LINEAGE, "run_id", run_id).await?;
    let root_id = text(&lineage, "root_run_id")?;
    let root = exact(tx, RUNS, "run_id", &root_id).await?;
    let baseline = super::admission_projection::limits(&root)?;
    let counters = exact(tx, ROOTS, "root_run_id", &root_id).await?;
    let consumed = integer(&counters, "consumed")?;
    let limits = super::allowance::effective_limits(tx, &root_id, &baseline).await?;
    if consumed.cast_unsigned() >= limits.node_execution_cap {
        return Err(Error::CapacityUnavailable);
    }
    Ok((root_id, consumed))
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn prepared_call(
        db: &dyn Database,
        run_id: &str,
        node: &str,
        owner: &crate::WorkflowExecutionAuthority,
    ) -> (String, String) {
        super::super::attempts::claim_authority(db, run_id, owner)
            .await
            .unwrap();
        let activation = super::super::graph::activation_id(db, run_id, node)
            .await
            .unwrap()
            .unwrap();
        let prepared = crate::PreparedAttempt {
            run_id: run_id.into(),
            node_id: node.into(),
            activation_id: activation.clone(),
            attempt: 1,
            side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({}),
            prepared_at_ms: 2,
        };
        let dispatch = super::super::attempts::prepare(db, owner, &prepared)
            .await
            .unwrap();
        super::super::attempts::attach_receipt(
            db,
            owner,
            &crate::DispatchReceipt {
                run_id: run_id.into(),
                node_id: node.into(),
                activation_id: activation.clone(),
                attempt: 1,
                dispatch_identity: dispatch.clone(),
                receipt: serde_json::json!({}),
                admitted_at_ms: 3,
            },
        )
        .await
        .unwrap();
        (activation, dispatch)
    }

    async fn child_fixture(
        db: &dyn Database,
    ) -> (
        bcode_workflow::WorkflowDefinition,
        bcode_workflow::WorkflowDefinitionIdentity,
        bcode_workflow::WorkflowCallTarget,
        NewWorkflowRun,
    ) {
        child_fixture_with_binding(db, None).await
    }

    async fn child_fixture_with_binding(
        db: &dyn Database,
        binding: Option<&crate::WorkflowRunPackageBinding>,
    ) -> (
        bcode_workflow::WorkflowDefinition,
        bcode_workflow::WorkflowDefinitionIdentity,
        bcode_workflow::WorkflowCallTarget,
        NewWorkflowRun,
    ) {
        let child_definition = bcode_workflow::WorkflowBuilder::new(
            "child",
            bcode_workflow::Step::task("work", |v: u32, _| async move { Ok(v) }),
        )
        .build()
        .unwrap()
        .definition()
        .clone();
        let identity =
            bcode_workflow::WorkflowDefinitionIdentity::for_definition("child", &child_definition)
                .unwrap();
        let target = bcode_workflow::WorkflowCallTarget::Definition {
            identity: identity.clone(),
        };
        let mut parent_definition = child_definition.clone();
        let node = parent_definition.nodes.get_mut("work").unwrap();
        node.kind = bcode_workflow::NodeKind::WorkflowCall;
        node.configuration = serde_json::to_value(bcode_workflow::WorkflowCallConfiguration {
            version: bcode_workflow::WORKFLOW_CALL_VERSION,
            target: target.clone(),
            input: None,
            output: None,
        })
        .unwrap();
        let parent_identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
            "parent",
            &parent_definition,
        )
        .unwrap();
        let mut parent = super::super::tests::run("parent", false);
        parent.definition_id = parent_identity.definition_id;
        parent.definition_version = parent_identity.definition_version;
        parent.limits.node_execution_cap = 2;
        let tx = db.begin_transaction().await.unwrap();
        super::super::admit_package_in(&*tx, &parent, &parent_definition, binding)
            .await
            .unwrap();
        register_root(&*tx, &parent).await.unwrap();
        tx.commit().await.unwrap();
        (child_definition, identity, target, parent)
    }

    struct EndedOwner;
    impl super::super::safety::AuthorityHandoffResolver for EndedOwner {
        fn verify<'a>(
            &'a self,
            transition: &'a super::super::safety::AuthorityHandoff,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<super::super::safety::HandoffEvidence, Error>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                Ok(super::super::safety::HandoffEvidence::Verified {
                    transition: Box::new(transition.clone()),
                    receipt_id: "ended".into(),
                    verified_at_ms: 100,
                })
            })
        }
    }

    async fn assert_child_recovery_fence(
        db: &dyn Database,
        stale: &crate::WorkflowExecutionAuthority,
        link: &crate::WorkflowRunLink,
        child: &mut NewWorkflowRun,
        definition: &bcode_workflow::WorkflowDefinition,
        child_id: &str,
    ) {
        let mut replacement = stale.clone();
        replacement.daemon_instance_id = "replacement".into();
        replacement.fencing_token = "replacement-fence".into();
        let transition = super::super::safety::AuthorityHandoff {
            run_id: "parent".into(),
            expected: crate::WorkflowExecutionAuthority {
                target_artifact_id: "a".into(),
                daemon_instance_id: "d".into(),
                generation: 1,
                fencing_token: "f".into(),
            },
            replacement: replacement.clone(),
        };
        super::super::safety::handoff_for_recovery(db, &transition, &EndedOwner)
            .await
            .unwrap();
        child.execution_authority = Some(replacement.clone());
        assert!(
            admit_child(db, stale, link, child, definition)
                .await
                .is_err()
        );
        // Recovery barrier is intentional: duplicate child discovery is allowed only after
        // reconciliation, never as a way around recovery or to transfer the existing child.
        assert!(
            admit_child(db, &replacement, link, child, definition)
                .await
                .is_err()
        );
        assert_eq!(
            super::super::admission_projection::package_binding(db, child_id)
                .await
                .unwrap()
                .unwrap()
                .package_id,
            "test/mixed"
        );
    }

    #[tokio::test]
    async fn old_definition_child_omission_inherits_root_binding_and_retries_do_not_rewrite() {
        let db = super::super::tests::database().await;
        super::super::install(&*db).await.unwrap();
        let binding = crate::WorkflowRunPackageBinding {
            version: 1,
            package_id: "test/mixed".into(),
            lock_digest: "a".repeat(64),
        };
        let (definition, identity, target, parent) =
            child_fixture_with_binding(&*db, Some(&binding)).await;
        let owner = crate::WorkflowExecutionAuthority {
            target_artifact_id: "a".into(),
            daemon_instance_id: "d".into(),
            generation: 1,
            fencing_token: "f".into(),
        };
        let (activation, _) = prepared_call(&*db, "parent", "work", &owner).await;
        let child_id = crate::workflow_child_run_id("parent", "parent", &activation, 1, &target);
        let mut child = super::super::tests::run(&child_id, false);
        child.definition_id = identity.definition_id;
        child.definition_version = identity.definition_version;
        child.parent_session_id = Some("parent-session".into());
        child.execution_authority = Some(owner.clone());
        child.limits = parent.limits;
        let link = crate::WorkflowRunLink {
            version: crate::WORKFLOW_RUN_LINK_VERSION,
            root_run_id: "parent".into(),
            parent_run_id: "parent".into(),
            parent_node_id: "work".into(),
            parent_activation_id: activation,
            parent_attempt: 1,
            child_run_id: child_id.clone(),
            target,
            depth: 2,
            created_at_ms: 4,
        };
        // Model the old Definition-only transaction: bare admission plus canonical lineage.
        let tx = db.begin_transaction().await.unwrap();
        register_child(&*tx, &owner, &link, &child).await.unwrap();
        super::super::admit_in(&*tx, &child, &definition)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            super::super::admission_projection::package_binding(&*db, &child_id)
                .await
                .unwrap(),
            Some(binding)
        );
        let mut later_child = child.clone();
        later_child.created_at_ms += 100;
        let mut later_link = link.clone();
        later_link.created_at_ms += 100;
        assert!(
            admit_child(&*db, &owner, &later_link, &later_child, &definition)
                .await
                .unwrap()
                .already_present
        );
        let mut stale = owner;
        stale.generation += 1;
        assert!(
            admit_child(&*db, &stale, &later_link, &later_child, &definition)
                .await
                .is_err()
        );
        assert_child_recovery_fence(
            &*db,
            &stale,
            &later_link,
            &mut later_child,
            &definition,
            &child_id,
        )
        .await;
        let row = exact(&*db, RUNS, "run_id", &child_id).await.unwrap();
        let stored =
            super::super::admission_projection::admission(&text(&row, "admission_json").unwrap())
                .unwrap();
        assert_eq!(stored.admission, child);
        assert!(
            stored.package_binding.is_none(),
            "lineage recovery must not rewrite old admissions"
        );
    }

    async fn assert_child_discovery(
        db: &dyn Database,
        child_id: &str,
        owner: &crate::WorkflowExecutionAuthority,
    ) {
        let page = children(db, "parent", None, 1).await.unwrap();
        assert_eq!(page.children.len(), 1);
        assert_eq!(page.children[0].child_run_id, child_id);
        assert!(page.next_child_run_id.is_none());
        assert!(
            children(db, "parent", Some(child_id), 1)
                .await
                .unwrap()
                .children
                .is_empty()
        );
        assert!(children(db, "parent", None, 0).await.is_err());
        let context = parent_context(db, owner, "parent").await.unwrap();
        assert_eq!(context.depth, 1);
        assert!(context.parent_link.is_none());
        assert_eq!(context.root_run_id, "parent");
    }
    #[tokio::test]
    async fn exact_child_admission_completion_and_root_allowance_share_canonical_state() {
        let db = super::super::tests::database().await;
        super::super::install(&*db).await.unwrap();
        let (child_definition, identity, target, parent) = child_fixture(&*db).await;
        let owner = crate::WorkflowExecutionAuthority {
            target_artifact_id: "a".into(),
            daemon_instance_id: "d".into(),
            generation: 1,
            fencing_token: "f".into(),
        };
        let (activation, _) = prepared_call(&*db, "parent", "work", &owner).await;
        let child_id = crate::workflow_child_run_id("parent", "parent", &activation, 1, &target);
        let mut child = super::super::tests::run(&child_id, false);
        child.definition_id = identity.definition_id;
        child.definition_version = identity.definition_version;
        child.parent_session_id = Some("parent-session".into());
        child.execution_authority = Some(owner.clone());
        child.limits = parent.limits.clone();
        let link = crate::WorkflowRunLink {
            version: crate::WORKFLOW_RUN_LINK_VERSION,
            root_run_id: "parent".into(),
            parent_run_id: "parent".into(),
            parent_node_id: "work".into(),
            parent_activation_id: activation.clone(),
            parent_attempt: 1,
            child_run_id: child_id.clone(),
            target,
            depth: 2,
            created_at_ms: 4,
        };
        assert!(
            !admit_child(&*db, &owner, &link, &child, &child_definition)
                .await
                .unwrap()
                .already_present
        );
        assert!(
            admit_child(&*db, &owner, &link, &child, &child_definition)
                .await
                .unwrap()
                .already_present
        );
        assert_eq!(
            child_result(&*db, &owner, "parent", "work", &activation, 1)
                .await
                .unwrap(),
            Some(ChildResult::Pending)
        );
        assert_child_discovery(&*db, &child_id, &owner).await;
        let (child_activation, dispatch) = prepared_call(&*db, &child_id, "work", &owner).await;
        let tx = db
            .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
            .await
            .unwrap();
        assert!(consume_attempt(&*tx, &child_id).await.is_err());
        assert_eq!(
            integer(
                &exact(&*tx, ROOTS, "root_run_id", "parent").await.unwrap(),
                "descendants"
            )
            .unwrap(),
            1
        );
        tx.rollback().await.unwrap();
        complete_child(
            &*db,
            &owner,
            &child_id,
            &child_definition,
            child_activation,
            &dispatch,
        )
        .await;
        super::super::graph::completion::reconcile_child(
            &*db,
            &owner,
            "parent",
            "work",
            &activation,
            1,
            6,
        )
        .await
        .unwrap();
        assert_eq!(
            child_result(&*db, &owner, "parent", "work", &activation, 1)
                .await
                .unwrap(),
            Some(ChildResult::Completed(serde_json::json!(9)))
        );
        let stale = crate::WorkflowExecutionAuthority {
            generation: 2,
            ..owner
        };
        assert!(
            child_result(&*db, &stale, "parent", "work", &activation, 1)
                .await
                .is_err()
        );
    }

    async fn complete_child(
        db: &dyn Database,
        owner: &crate::WorkflowExecutionAuthority,
        child_id: &str,
        definition: &bcode_workflow::WorkflowDefinition,
        activation: String,
        dispatch: &str,
    ) {
        let output = crate::ValidatedOutput {
            output_id: "child-output".into(),
            run_id: child_id.into(),
            node_id: "work".into(),
            activation_id: activation,
            schema_id: definition.nodes["work"].output.type_name.clone(),
            schema_version: 1,
            value: serde_json::json!(9),
            artifact_reference: None,
            created_at_ms: 5,
        };
        super::super::graph::completion::complete(db, owner, 1, dispatch, &output)
            .await
            .unwrap();
        super::super::runs::terminalize(db, owner, child_id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn root_counter_is_atomic_bounded_and_never_infers_missing_lineage() {
        let db = super::super::tests::database().await;
        super::super::install(&*db).await.unwrap();
        let definition = bcode_workflow::WorkflowBuilder::new(
            "root",
            bcode_workflow::Step::task("work", |v: u32, _| async move { Ok(v) }),
        )
        .build()
        .unwrap()
        .definition()
        .clone();
        let identity =
            bcode_workflow::WorkflowDefinitionIdentity::for_definition("root", &definition)
                .unwrap();
        let mut run = super::super::tests::run("root", false);
        run.definition_id = identity.definition_id;
        run.definition_version = identity.definition_version;
        run.limits.node_execution_cap = 1;
        super::super::admit(&*db, &run, &definition).await.unwrap();
        let tx = db
            .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
            .await
            .unwrap();
        assert!(consume_attempt(&*tx, "missing").await.is_err());
        consume_attempt(&*tx, "root").await.unwrap();
        assert!(consume_attempt(&*tx, "root").await.is_err());
        tx.rollback().await.unwrap();
        let tx = db
            .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
            .await
            .unwrap();
        consume_attempt(&*tx, "root").await.unwrap();
        tx.commit().await.unwrap();
    }
}
