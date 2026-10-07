//! Exact receipt-backed sensitive diagnostic export. Never discovers unrecorded files.
use crate::ServerState;
use bcode_session_models::SessionId;
use bcode_workflow::{
    MAX_WORKFLOW_DIAGNOSTIC_ARTIFACT_BYTES, MAX_WORKFLOW_DIAGNOSTIC_RANGE_BYTES,
    WORKFLOW_DIAGNOSTIC_ARTIFACT_VERSION, WorkflowDiagnosticArtifactError as Error,
    WorkflowDiagnosticArtifactRange, WorkflowDiagnosticArtifactRequest,
    WorkflowDiagnosticCompleteness,
};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use switchy_database_connection::{SqliteAccess, SqliteOpenOptions, open_sqlite_rusqlite};

#[cfg(test)]
mod confinement_tests;
#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod tests;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Diagnostics {
    version: u32,
    parent_session_id: SessionId,
    artifacts: Vec<RecordedReference>,
    #[serde(default)]
    omitted_artifacts: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedReference {
    artifact_id: String,
    byte_len: u64,
    reference: serde_json::Value,
}

struct Selected {
    session_id: SessionId,
    uri: String,
    sha256: String,
    byte_len: u64,
}

fn validate(request: &WorkflowDiagnosticArtifactRequest) -> Result<(), Error> {
    if request.version != WORKFLOW_DIAGNOSTIC_ARTIFACT_VERSION {
        return Err(Error::UnsupportedVersion);
    }
    if request.length == 0
        || request.length > MAX_WORKFLOW_DIAGNOSTIC_RANGE_BYTES
        || [
            &request.run_id,
            &request.dispatch_identity,
            &request.artifact_id,
        ]
        .into_iter()
        .any(|id| id.is_empty() || id.len() > 1024 || id.chars().any(char::is_control))
    {
        return Err(Error::InvalidRequest);
    }
    Ok(())
}

fn recorded(
    receipt: &bcode_workflow_store::DispatchReceipt,
    run: &str,
    dispatch: &str,
) -> Result<Diagnostics, Error> {
    if receipt.run_id != run || receipt.dispatch_identity != dispatch {
        return Err(Error::VerificationFailed);
    }
    let value = receipt
        .receipt
        .get("diagnostics")
        .ok_or(Error::Unavailable)?;
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err(Error::UnsupportedVersion);
    }
    if value
        .get("artifacts")
        .and_then(serde_json::Value::as_array)
        .is_none_or(|items| items.len() > 100)
    {
        return Err(Error::VerificationFailed);
    }
    let diagnostics: Diagnostics =
        serde_json::from_value(value.clone()).map_err(|_| Error::VerificationFailed)?;
    if diagnostics.version != 1 {
        return Err(Error::UnsupportedVersion);
    }
    if diagnostics.omitted_artifacts.is_some_and(|count| count > 0)
        && diagnostics.artifacts.len() != 100
    {
        return Err(Error::VerificationFailed);
    }
    let mut ids = std::collections::BTreeSet::new();
    for artifact in &diagnostics.artifacts {
        if !ids.insert(&artifact.artifact_id) {
            return Err(Error::VerificationFailed);
        }
    }
    Ok(diagnostics)
}

fn selected_artifact(
    diagnostics: &Diagnostics,
    artifact: &RecordedReference,
    dispatch: &str,
) -> Result<Selected, Error> {
    if artifact.artifact_id.is_empty()
        || artifact.artifact_id.len() > 1024
        || artifact.artifact_id.chars().any(char::is_control)
    {
        return Err(Error::VerificationFailed);
    }
    let reference = &artifact.reference;
    let sha256 = reference
        .get("sha256")
        .and_then(serde_json::Value::as_str)
        .ok_or(Error::Unavailable)?;
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || artifact.byte_len > MAX_WORKFLOW_DIAGNOSTIC_ARTIFACT_BYTES
        || reference
            .get("byte_len")
            .and_then(serde_json::Value::as_u64)
            != Some(artifact.byte_len)
    {
        return Err(Error::VerificationFailed);
    }
    let uri = reference
        .get("uri")
        .and_then(serde_json::Value::as_str)
        .ok_or(Error::Unavailable)?;
    let expected = format!(
        "bcode-artifact://invocation/{}/{}",
        hex::encode(Sha256::digest(dispatch.as_bytes())),
        hex::encode(Sha256::digest(artifact.artifact_id.as_bytes()))
    );
    // Validate the recorded capability binding; never supply a missing reference.
    if uri != expected {
        return Err(Error::VerificationFailed);
    }
    Ok(Selected {
        session_id: diagnostics.parent_session_id,
        uri: uri.into(),
        sha256: sha256.into(),
        byte_len: artifact.byte_len,
    })
}

fn select(
    receipt: &bcode_workflow_store::DispatchReceipt,
    request: &WorkflowDiagnosticArtifactRequest,
) -> Result<Selected, Error> {
    validate(request)?;
    let diagnostics = recorded(receipt, &request.run_id, &request.dispatch_identity)?;
    let artifact = diagnostics
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_id == request.artifact_id)
        .ok_or(Error::Unavailable)?;
    selected_artifact(&diagnostics, artifact, &request.dispatch_identity)
}

fn verify_bytes(selected: &Selected, bytes: &[u8]) -> Result<(), Error> {
    if bytes.len() as u64 != selected.byte_len
        || hex::encode(Sha256::digest(bytes)) != selected.sha256
    {
        return Err(Error::VerificationFailed);
    }
    Ok(())
}

async fn exact_receipt(
    state: &ServerState,
    run: &str,
    dispatch: &str,
) -> Result<bcode_workflow_store::DispatchReceipt, Error> {
    if [run, dispatch]
        .into_iter()
        .any(|id| id.is_empty() || id.len() > 1024 || id.chars().any(char::is_control))
    {
        return Err(Error::InvalidRequest);
    }
    state
        .poll_workflow_initialization(false)
        .map_err(|_| Error::StorageUnavailable)?;
    let retained = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retained_handle()
        .map_err(|_| Error::StorageUnavailable)?;
    let db = open_sqlite_rusqlite(
        retained.path(),
        &SqliteOpenOptions {
            access: SqliteAccess::ReadOnly,
            allow_uri: false,
            busy_timeout: std::time::Duration::ZERO,
            foreign_keys: true,
            connections: std::num::NonZeroUsize::MIN,
        },
    )
    .map_err(|_| Error::StorageUnavailable)?;
    bcode_workflow_store::representation::inspection_attempts::diagnostic_receipt(
        &db, run, dispatch,
    )
    .await
    .map_err(|_| Error::StorageUnavailable)?
    .ok_or(Error::Unavailable)
}

fn summaries(
    receipt: &bcode_workflow_store::DispatchReceipt,
    run: &str,
    dispatch: &str,
) -> Result<bcode_workflow::WorkflowDiagnosticArtifacts, Error> {
    let diagnostics = recorded(receipt, run, dispatch)?;
    let mut artifacts = Vec::new();
    let mut unavailable = Vec::new();
    for artifact in &diagnostics.artifacts {
        let selected = match selected_artifact(&diagnostics, artifact, dispatch) {
            Ok(value) => value,
            Err(reason) => {
                unavailable.push(bcode_workflow::WorkflowDiagnosticUnavailableArtifact {
                    artifact_id: artifact.artifact_id.clone(),
                    reason,
                });
                continue;
            }
        };
        artifacts.push(bcode_workflow::WorkflowDiagnosticArtifactSummary {
            artifact_id: artifact.artifact_id.clone(),
            sha256: selected.sha256,
            byte_len: selected.byte_len,
            completeness: WorkflowDiagnosticCompleteness::Retained,
        });
    }
    Ok(bcode_workflow::WorkflowDiagnosticArtifacts {
        version: WORKFLOW_DIAGNOSTIC_ARTIFACT_VERSION,
        run_id: run.into(),
        dispatch_identity: dispatch.into(),
        artifacts,
        unavailable,
        omitted_artifacts: diagnostics.omitted_artifacts,
    })
}

/// Discover every recorded diagnostic reference for one exact dispatch, bounded to 100.
/// This validates receipt metadata, not current physical byte availability.
/// # Errors
/// Rejects missing receipts, unknown versions, duplicate/invalid references and unavailable storage.
pub async fn list(
    state: &ServerState,
    run_id: &str,
    dispatch_identity: &str,
) -> Result<bcode_workflow::WorkflowDiagnosticArtifacts, Error> {
    let receipt = exact_receipt(state, run_id, dispatch_identity).await?;
    summaries(&receipt, run_id, dispatch_identity)
}

/// Export a bounded range selected only from an exact canonical dispatch receipt.
/// # Errors
/// Returns normalized version, identity, availability, integrity or storage admission failures.
/// Missing historical metadata and interruption before receipt settlement are unavailable.
pub async fn read(
    state: &ServerState,
    request: &WorkflowDiagnosticArtifactRequest,
) -> Result<WorkflowDiagnosticArtifactRange, Error> {
    validate(request)?;
    let receipt = exact_receipt(state, &request.run_id, &request.dispatch_identity).await?;
    let selected = select(&receipt, request)?;
    if request.offset > selected.byte_len {
        return Err(Error::InvalidRequest);
    }
    let ownership = state
        .sessions
        .acquire_session_ownership(
            selected.session_id,
            bcode_session::SessionOwnershipKind::RuntimeWork,
        )
        .await
        .map_err(|_| Error::StorageUnavailable)?;
    let root = crate::session_artifact_dir(state, selected.session_id)
        .map_err(|_| Error::StorageUnavailable)?;
    let path = crate::artifact_reference_path(&selected.uri, &root)
        .map_err(|_| Error::VerificationFailed)?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let mut admission = crate::storage_read_admission::RegisteredStorageRead::for_session(
        state,
        selected.session_id,
    )
    .await
    .map_err(|_| Error::StorageUnavailable)?;
    let read_ownership = ownership.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _ownership = read_ownership;
        crate::read_artifact_file_range_at_length(&path, &root, 0, 524_288, None)
            .map_err(|_| Error::Unavailable)
    })
    .await
    .map_err(|_| Error::StorageUnavailable)
    .and_then(|result| result);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let result =
        crate::storage_read_admission::RegisteredStorageRead::finish_failed(&mut admission, result)
            .await;
    let (length, bytes) = result?;
    let verification = if length == selected.byte_len {
        verify_bytes(&selected, &bytes)
    } else {
        Err(Error::VerificationFailed)
    };
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if let Some(admission) = admission {
        admission
            .finish_consumption(
                state,
                selected.session_id,
                bcode_session::storage_access::StorageAccessKind::Artifact,
            )
            .await;
    }
    verification?;
    drop(ownership);
    let start = usize::try_from(request.offset).map_err(|_| Error::InvalidRequest)?;
    let end = start
        .saturating_add(request.length as usize)
        .min(bytes.len());
    Ok(WorkflowDiagnosticArtifactRange {
        version: WORKFLOW_DIAGNOSTIC_ARTIFACT_VERSION,
        run_id: request.run_id.clone(),
        dispatch_identity: request.dispatch_identity.clone(),
        artifact_id: request.artifact_id.clone(),
        sha256: selected.sha256,
        byte_len: selected.byte_len,
        completeness: WorkflowDiagnosticCompleteness::Retained,
        offset: request.offset,
        bytes: bytes[start..end].to_vec(),
        eof: end == bytes.len(),
    })
}
