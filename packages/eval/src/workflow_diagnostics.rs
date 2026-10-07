//! Bounded receipt-backed diagnostic capture, independent of workflow success.
use super::{EvalError, Path, write_json};
use bcode_workflow::{WorkflowDiagnosticArtifactRange, WorkflowDiagnosticArtifacts};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::time::Duration;

#[cfg(test)]
mod tests;

pub fn capture(
    run_id: &str,
    dispatch_identity: &str,
    directory: &Path,
    invoke: &impl Fn(&[String], &str, Duration) -> Result<Value, EvalError>,
) -> Result<(), EvalError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let remaining = || deadline.saturating_duration_since(std::time::Instant::now());
    let value = invoke(
        &[
            "workflow".into(),
            "diagnostic-artifacts".into(),
            "--run-id".into(),
            run_id.into(),
            "--dispatch-identity".into(),
            dispatch_identity.into(),
        ],
        "diagnostic-discovery",
        remaining().min(Duration::from_secs(15)),
    )?;
    let artifacts: WorkflowDiagnosticArtifacts = serde_json::from_value(value)
        .map_err(|_| EvalError::Validation("invalid diagnostic discovery".into()))?;
    let partial = !artifacts.unavailable.is_empty() || artifacts.omitted_artifacts != Some(0);
    if artifacts.version != 1
        || artifacts.run_id != run_id
        || artifacts.dispatch_identity != dispatch_identity
        || artifacts.artifacts.len() > 100
    {
        return Err(EvalError::Validation(
            "diagnostic discovery identity mismatch".into(),
        ));
    }
    std::fs::create_dir_all(directory)?;
    write_json(directory.join("index.json"), &artifacts)?;
    let mut seen = std::collections::BTreeSet::new();
    for (index, artifact) in artifacts.artifacts.iter().enumerate() {
        if !seen.insert(&artifact.artifact_id) {
            return Err(EvalError::Validation(
                "duplicate diagnostic identity".into(),
            ));
        }
        if artifact.byte_len > 524_288 {
            return Err(EvalError::Validation(
                "diagnostic size exceeds bound".into(),
            ));
        }
        let mut bytes = Vec::new();
        let mut pages = 0;
        loop {
            if remaining().is_zero() || pages >= 64 {
                return Err(EvalError::Validation(
                    "diagnostic export allowance exhausted".into(),
                ));
            }
            pages += 1;
            let offset = bytes.len() as u64;
            let value = invoke(
                &[
                    "workflow".into(),
                    "read-diagnostic-artifact".into(),
                    "--run-id".into(),
                    run_id.into(),
                    "--dispatch-identity".into(),
                    dispatch_identity.into(),
                    "--artifact-id".into(),
                    artifact.artifact_id.clone(),
                    "--offset".into(),
                    offset.to_string(),
                    "--length".into(),
                    "32768".into(),
                ],
                "diagnostic-range",
                remaining().min(Duration::from_secs(15)),
            )?;
            let range: WorkflowDiagnosticArtifactRange = serde_json::from_value(value)
                .map_err(|_| EvalError::Validation("invalid diagnostic range".into()))?;
            let end = offset.checked_add(range.bytes.len() as u64);
            if range.version != 1
                || range.run_id != run_id
                || range.dispatch_identity != dispatch_identity
                || range.artifact_id != artifact.artifact_id
                || range.offset != offset
                || range.byte_len != artifact.byte_len
                || range.sha256 != artifact.sha256
                || range.bytes.len() > 32768
                || !end.is_some_and(|end| {
                    end <= artifact.byte_len && range.eof == (end == artifact.byte_len)
                })
                || (!range.eof && range.bytes.is_empty())
            {
                return Err(EvalError::Validation(
                    "diagnostic range verification failed".into(),
                ));
            }
            bytes.extend(range.bytes);
            if range.eof {
                break;
            }
        }
        if hex::encode(Sha256::digest(&bytes)) != artifact.sha256 {
            return Err(EvalError::Validation("diagnostic digest mismatch".into()));
        }
        std::fs::write(directory.join(format!("artifact-{index:03}.json")), bytes)?;
    }
    if partial {
        Err(EvalError::Validation(
            "some recorded diagnostic artifacts are unavailable; verified entries retained".into(),
        ))
    } else {
        Ok(())
    }
}
