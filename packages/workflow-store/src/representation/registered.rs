//! Admission from the exact immutable registered definition, checked in the admission transaction.
use super::{
    Database, DatabaseValue, FilterableQuery, GraphAdmissionPolicy, RepresentationAdmission,
    RepresentationError, TransactionMode, hierarchy, verified,
};
/// Admit a run only when its supplied definition exactly matches the registered ID/version.
///
/// Authorization and executable capability validation remain application responsibilities.
/// # Errors
/// Rejects absent/mismatched registry content/checksum and ordinary admission failures.
pub async fn admit_registered(
    db: &dyn Database,
    run: &crate::NewWorkflowRun,
    definition: &bcode_workflow::WorkflowDefinition,
) -> Result<RepresentationAdmission, RepresentationError> {
    admit_registered_observed(db, run, definition, false).await
}

/// Admit an exact registered definition with explicit new-run observation consent.
///
/// Capture is enabled atomically before admission; existing runs are never backfilled or enabled.
/// Current-inventory observers are installed or verified in that same writer transaction,
/// preserving conservative installation gaps without claiming historical completeness.
/// Authorization and executable capability validation remain application responsibilities.
/// # Errors
/// Rejects registry mismatches, unavailable journal state when capture is requested, and admission failures.
pub async fn admit_registered_observed(
    db: &dyn Database,
    run: &crate::NewWorkflowRun,
    definition: &bcode_workflow::WorkflowDefinition,
    capture: bool,
) -> Result<RepresentationAdmission, RepresentationError> {
    admit_registered_package_observed(db, run, definition, capture, None).await
}

/// Admit against the exact publication and registered definition in the same transaction.
/// # Errors
/// Rejects unsupported binding versions, publication/member drift and ordinary admission failures.
pub async fn admit_registered_package_observed(
    db: &dyn Database,
    run: &crate::NewWorkflowRun,
    definition: &bcode_workflow::WorkflowDefinition,
    capture: bool,
    package: Option<&crate::WorkflowRunPackageBinding>,
) -> Result<RepresentationAdmission, RepresentationError> {
    let json =
        serde_json::to_string(definition).map_err(|_| RepresentationError::InvalidAdmission)?;
    GraphAdmissionPolicy::default().check(json.len())?;
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    verified(&*tx).await?;
    if definition.nodes.values().any(|node| {
        node.kind == bcode_workflow::NodeKind::WorkflowCall
            && serde_json::from_value::<bcode_workflow::WorkflowCallConfiguration>(
                node.configuration.clone(),
            )
            .is_ok_and(|call| {
                matches!(
                    call.target,
                    bcode_workflow::WorkflowCallTarget::PackageLocal { .. }
                        | bcode_workflow::WorkflowCallTarget::PackageMember { .. }
                )
            })
    }) && package.is_none()
    {
        return Err(RepresentationError::InvalidAdmission);
    }
    let rows = tx
        .select("workflow_definitions")
        .columns(&["definition_json", "checksum_sha256"])
        .where_eq("definition_id", run.definition_id.as_str())
        .where_eq("version", i64::from(run.definition_version))
        .limit(1)
        .execute(&*tx)
        .await?;
    let row = rows.first().ok_or(RepresentationError::InvalidAdmission)?;
    if row.get("definition_json") != Some(DatabaseValue::String(json.clone()))
        || row.get("checksum_sha256")
            != Some(DatabaseValue::String(crate::sha256_hex(json.as_bytes())))
    {
        return Err(RepresentationError::InvalidAdmission);
    }
    if capture
        && tx
            .select(super::RUNS)
            .columns(&["run_id"])
            .where_eq("run_id", run.run_id.as_str())
            .limit(1)
            .execute(&*tx)
            .await?
            .is_empty()
    {
        super::journal::enable_admission(&*tx, &run.run_id).await?;
    }
    // Install/verify membership observers under the same writer transaction before any
    // admission facts. This is current-inventory fencing, not historical backfill.
    Box::pin(super::inventory::install_in(&*tx)).await?;
    if let Some(binding) = package {
        super::admission_projection::verify_member(&*tx, binding, run, definition).await?;
    }
    let result = Box::pin(super::admit_package_in(&*tx, run, definition, package)).await?;
    if !result.already_present {
        hierarchy::register_root(&*tx, run).await?;
    }
    tx.commit().await?;
    Ok(result)
}
