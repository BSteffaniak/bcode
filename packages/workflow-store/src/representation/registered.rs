//! Admission from the exact immutable registered definition, checked in the admission transaction.
use super::{
    Database, DatabaseValue, FilterableQuery, GraphAdmissionPolicy, RepresentationAdmission,
    RepresentationError, TransactionMode, admit_in, hierarchy, verified,
};
/// Admit a run only when its supplied definition exactly matches the registered ID/version.
/// Authorization and executable capability validation remain application responsibilities.
/// # Errors
/// Rejects absent/mismatched registry content/checksum and ordinary admission failures.
pub async fn admit_registered(
    db: &dyn Database,
    run: &crate::NewWorkflowRun,
    definition: &bcode_workflow::WorkflowDefinition,
) -> Result<RepresentationAdmission, RepresentationError> {
    let json =
        serde_json::to_string(definition).map_err(|_| RepresentationError::InvalidAdmission)?;
    GraphAdmissionPolicy::default().check(json.len())?;
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    verified(&*tx).await?;
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
    let result = admit_in(&*tx, run, definition).await?;
    if !result.already_present {
        hierarchy::register_root(&*tx, run).await?;
    }
    tx.commit().await?;
    Ok(result)
}
