//! Actual single-attempt workflow evaluation, not a supplied diagnosis microbenchmark.
//! The CLI coordinator authorizes execution and calls this blocking adapter outside async IO.
use super::{CaptureScope, CaptureStatus, ConsumerCapture};
use crate::{EvalRunOptions, diagnosis::EvalAttemptJudgementObservation};
use bcode_model::judgement::{
    ApplicationDisposition, Feature, ObservationTerminal, TriggerDisposition,
};
use sha2::Digest as _;
use std::{fs, io::Read, path::Path, time::Instant};

const MAX_ARTIFACT_BYTES: u64 = 256 * 1024;

fn confined(path: &Path, authorized_root: &Path) -> Result<std::path::PathBuf, ()> {
    if !path.is_absolute() || path.is_symlink() {
        return Err(());
    }
    let canonical = path.canonicalize().map_err(|_| ())?;
    if !authorized_root.is_absolute() || !authorized_root.is_dir() {
        return Err(());
    }
    let root = authorized_root.canonicalize().map_err(|_| ())?;
    if !canonical.starts_with(root) {
        return Err(());
    }
    Ok(canonical)
}

fn bounded_json(path: &Path) -> Result<serde_json::Value, ()> {
    if path.is_symlink() {
        return Err(());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|_| ())?
        .take(MAX_ARTIFACT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
        return Err(());
    }
    serde_json::from_slice(&bytes).map_err(|_| ())
}

fn diagnosis_label(diagnosis: &serde_json::Value) -> Option<String> {
    diagnosis
        .pointer("/advisory_model_labels/failure_category")
        .and_then(serde_json::Value::as_str)
        .filter(|label| {
            matches!(
                *label,
                "deadline"
                    | "preservation_failure"
                    | "completion_disagreement"
                    | "coordination"
                    | "execution_failure"
                    | "uncertain"
                    | "artifact_failure"
                    | "evidence_gap"
            )
        })
        .map(str::to_owned)
}

fn attempt_observable(
    observation: &EvalAttemptJudgementObservation,
    arm: crate::judgement_benchmark::Arm,
) -> Result<(), ()> {
    observation.validate().map_err(|_| ())?;
    if !observation.complete
        || observation.terminal.is_none()
        || observation.terminal == Some(ObservationTerminal::Cancelled)
    {
        return Err(());
    }
    let operation = &observation.operation;
    match arm {
        crate::judgement_benchmark::Arm::Off => {
            let closed_disabled =
                operation.trigger == TriggerDisposition::Disabled && operation.inference.is_empty();
            let disabled_call = operation.trigger == TriggerDisposition::Triggered
                && operation.inference.len() == 1
                && operation.inference.iter().all(|fact| {
                    fact.outcome == bcode_model::judgement::OptionalOperationOutcome::Disabled
                        && fact.discovery_invocations == 0
                        && fact.model_listing_invocations == 0
                        && fact.verification_invocations == 0
                        && fact.inference_invocations == 0
                        && fact.effective_provider.is_none()
                        && fact.effective_model.is_none()
                        && fact.inference_usage.is_none()
                        && fact.verification_usage.is_none()
                        && fact.verification_cost_usd.is_none()
                        && !fact.verification_cache_hit
                        && fact.transport_attempts.is_none_or(|count| count == 0)
                });
            if !(closed_disabled || disabled_call)
                || operation.application != Some(ApplicationDisposition::BaselineRetained)
            {
                return Err(());
            }
        }
        crate::judgement_benchmark::Arm::On => {
            if operation.trigger != TriggerDisposition::Triggered {
                return Err(());
            }
        }
    }
    Ok(())
}

fn apply_attempt(
    observation: &EvalAttemptJudgementObservation,
    diagnosis: &serde_json::Value,
    capture: &mut ConsumerCapture,
) -> Result<(), ()> {
    capture.scope = CaptureScope::NormalEvalAttempt;
    capture.operation = "eval.observation_coverage".into();
    attempt_observable(observation, capture.request.arm)?;
    let operation = &observation.operation;
    let label = diagnosis_label(diagnosis);
    capture.output = Some(crate::judgement_benchmark::ConsumerOutput::Classification {
        label: label.clone(),
    });
    capture.operation = "eval.diagnosis_label".into();
    if operation.application == Some(ApplicationDisposition::Applied) && label.is_none() {
        return Err(());
    }
    capture.consumer_executed = true;
    capture.status = CaptureStatus::Captured;
    capture.outcome = match observation.terminal {
        Some(ObservationTerminal::Completed) => {
            crate::judgement_benchmark::TerminalOutcome::Succeeded
        }
        Some(ObservationTerminal::Failed) => crate::judgement_benchmark::TerminalOutcome::Failed,
        _ => return Err(()),
    };
    capture.operation = "eval.run_loaded_suite+workflow.oracle+optional_eval_categorization".into();
    Ok(())
}

/// Run the actual eval-owned producer with its independent oracle and retained observation.
///
/// The caller must supply its explicitly authorized artifact/state root. Confinement checks
/// enforce that boundary but do not confer authorization. Suite inputs may launch
/// normal provider/tool/workflow work; no private storage or alternate inference is used.
///
/// # Errors
/// Rejects foreign binaries, non-external output, multiple attempts, unsupported executors,
/// missing attempt artifacts, incomplete terminal evidence, and unknown diagnosis labels.
pub(super) fn execute(
    suite_path: &Path,
    output_dir: &Path,
    subject_binary: &Path,
    authorized_root: &Path,
    capture: &mut ConsumerCapture,
) -> Result<(), ()> {
    capture.scope = CaptureScope::NormalEvalAttempt;
    capture.operation = "eval.precondition".into();
    if capture.request.feature != Feature::EvalCategorization
        || subject_binary != Path::new(&capture.request.subject.executable)
        || !subject_binary.is_absolute()
    {
        return Err(());
    }
    let mut binary = fs::File::open(subject_binary).map_err(|_| ())?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = binary.read(&mut buffer).map_err(|_| ())?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    if hex::encode(hasher.finalize()) != capture.request.subject.executable_sha256 {
        return Err(());
    }
    let output = confined(output_dir, authorized_root)?;
    if fs::read_dir(&output).map_err(|_| ())?.next().is_some() {
        return Err(());
    }
    let suite_path = confined(suite_path, authorized_root)?;
    let suite = crate::load_suite(&suite_path).map_err(|_| ())?;
    if suite.variants.len() != 1
        || suite.cases.len() != 1
        || suite.run.repetitions != 1
        || suite.variants[0].executor != crate::EvalExecutorKind::Workflow
    {
        return Err(());
    }
    let variant = &suite.variants[0];
    let config: bcode_eval_models::WorkflowEvalConfig =
        serde_json::from_value(variant.metadata.get("workflow").cloned().ok_or(())?)
            .map_err(|_| ())?;
    if config.binary != subject_binary
        || variant.profile.is_some()
        || crate::agent_daemon_isolation_mode(variant) != "shared"
        || !suite.cases[0].env.is_empty()
    {
        return Err(());
    }
    // Do not inherit arbitrary auth/config overrides or certify config completeness.
    // The suite's variant explicitly supplies the selected config, state and temp roots.
    for key in [
        "BCODE_CONFIG",
        "BCODE_CONFIG_TOML",
        "BCODE_STATE_DIR",
        "BCODE_SESSION_STORE_DIR",
        "TMPDIR",
    ] {
        if !variant.env.contains_key(key) {
            return Err(());
        }
    }
    confined(Path::new(&variant.env["BCODE_STATE_DIR"]), authorized_root)?;
    confined(
        Path::new(&variant.env["BCODE_SESSION_STORE_DIR"]),
        authorized_root,
    )?;
    confined(Path::new(&variant.env["TMPDIR"]), authorized_root)?;
    let started = Instant::now();
    capture.operation = "eval.execution".into();
    let result = crate::run_loaded_suite(
        &EvalRunOptions {
            suite_path,
            output_root: output,
            run_id: Some("normal-eval-attempt".into()),
        },
        suite,
    )
    .map_err(|_| ())?;
    capture.trial_latency_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
    capture.operation = "eval.attempt_artifacts".into();
    let repetition = result
        .variants
        .first()
        .and_then(|value| value.cases.first())
        .and_then(|value| value.repetitions.first())
        .ok_or(())?;
    let rep_dir = result
        .manifest
        .output_dir
        .join("cases")
        .join(&repetition.case_id)
        .join("variants")
        .join(&repetition.variant_id)
        .join("repetitions")
        .join(format!("{:04}", repetition.repetition));
    let rep_dir = confined(&rep_dir, authorized_root)?;
    if !repetition.artifacts.iter().any(|artifact| {
        artifact.kind == "eval_judgement_observation"
            && artifact.path == Path::new("eval-judgement-observation.json")
    }) {
        return Err(());
    }
    capture.operation = "eval.observation_decode".into();
    let observation: EvalAttemptJudgementObservation = serde_json::from_value(bounded_json(
        &rep_dir.join("eval-judgement-observation.json"),
    )?)
    .map_err(|_| ())?;
    capture.operation = "eval.observation_contract".into();
    // Retain valid inference facts immediately, before later artifact/correlation reads.
    capture
        .telemetry
        .retain_eval_observation(&observation)
        .map_err(|_| ())?;
    capture.operation = "eval.admission_correlation".into();
    let admission = bounded_json(&rep_dir.join("admission.json"))?;
    if admission.get("run_id").and_then(serde_json::Value::as_str)
        != Some(observation.run_id.as_str())
    {
        return Err(());
    }
    capture.operation = "eval.diagnosis_artifact".into();
    let diagnosis = bounded_json(&rep_dir.join("diagnosis.json"))?;
    capture.request.subject.configuration_complete = false;
    capture.trial_latency_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
    apply_attempt(&observation, &diagnosis, capture)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_or_unbounded_attempt_json_is_not_evidence() {
        assert!(bounded_json(Path::new("/path-that-does-not-exist/observation.json")).is_err());
        assert!(confined(Path::new("relative-output"), Path::new("relative-root")).is_err());
    }

    #[test]
    fn confinement_rejects_outside_symlink_escape_and_replacement() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let source = root.path().join("suite.json");
        let foreign = outside.path().join("suite.json");
        fs::write(&source, b"{}").unwrap();
        fs::write(&foreign, b"{}").unwrap();
        assert!(confined(&source, root.path()).is_ok());
        assert!(confined(&foreign, root.path()).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
            assert!(confined(&root.path().join("escape/suite.json"), root.path()).is_err());
            fs::remove_file(&source).unwrap();
            std::os::unix::fs::symlink(&foreign, &source).unwrap();
            assert!(confined(&source, root.path()).is_err());
        }
    }

    #[test]
    fn disabled_failed_eval_is_observed_but_partial_cancelled_future_are_not() {
        use crate::judgement_benchmark::Arm;
        let operation = bcode_model::judgement::ConsumerOperationObservation {
            operation_id: 0,
            feature: Feature::EvalCategorization,
            trigger: TriggerDisposition::Disabled,
            inference: Vec::new(),
            inference_complete: true,
            application: Some(ApplicationDisposition::BaselineRetained),
            selection: None,
            recommendation: None,
        };
        let mut observation =
            EvalAttemptJudgementObservation::new("actual-run".into(), "failed", operation);
        assert!(attempt_observable(&observation, Arm::Off).is_ok());
        assert!(attempt_observable(&observation, Arm::On).is_err());
        observation.complete = false;
        assert!(attempt_observable(&observation, Arm::Off).is_err());
        observation.complete = true;
        observation.terminal = Some(ObservationTerminal::Cancelled);
        assert!(attempt_observable(&observation, Arm::Off).is_err());
        observation.terminal = Some(ObservationTerminal::Failed);
        observation.schema_version += 1;
        assert!(attempt_observable(&observation, Arm::Off).is_err());
    }

    #[test]
    fn triggered_disabled_optional_response_proves_off_only_without_dispatch() {
        use bcode_model::judgement::{
            ConsumerOperationObservation, OptionalOperationObservation, OptionalOperationOutcome,
        };
        let fact = OptionalOperationObservation {
            outcome: OptionalOperationOutcome::Disabled,
            discovery_invocations: 0,
            model_listing_invocations: 0,
            verification_invocations: 0,
            inference_invocations: 0,
            elapsed_ms: 0,
            verification_cache_hit: false,
            effective_provider: None,
            effective_model: None,
            inference_usage: None,
            verification_usage: None,
            verification_cost_usd: None,
            transport_attempts: None,
            decision: None,
        };
        let mut observation = EvalAttemptJudgementObservation::new(
            "off-run".into(),
            "failed",
            ConsumerOperationObservation {
                operation_id: 0,
                feature: Feature::EvalCategorization,
                trigger: TriggerDisposition::Triggered,
                inference: vec![fact],
                inference_complete: true,
                application: Some(ApplicationDisposition::BaselineRetained),
                selection: None,
                recommendation: None,
            },
        );
        assert!(attempt_observable(&observation, crate::judgement_benchmark::Arm::Off).is_ok());
        observation.operation.inference[0].inference_invocations = 1;
        assert!(attempt_observable(&observation, crate::judgement_benchmark::Arm::Off).is_err());
        observation.operation.inference[0].inference_invocations = 0;
        observation.operation.inference[0].outcome = OptionalOperationOutcome::Unavailable;
        assert!(attempt_observable(&observation, crate::judgement_benchmark::Arm::Off).is_err());
        observation.operation.inference[0].outcome = OptionalOperationOutcome::Disabled;
        observation.operation.inference_complete = false;
        observation.complete = false;
        assert!(attempt_observable(&observation, crate::judgement_benchmark::Arm::Off).is_err());
    }

    #[test]
    fn partial_triggered_eval_retains_unknown_counts_not_disabled_zero() {
        let operation = bcode_model::judgement::ConsumerOperationObservation {
            operation_id: 0,
            feature: Feature::EvalCategorization,
            trigger: TriggerDisposition::Triggered,
            inference: Vec::new(),
            inference_complete: false,
            application: Some(ApplicationDisposition::BaselineRetained),
            selection: None,
            recommendation: None,
        };
        let observation =
            EvalAttemptJudgementObservation::new("partial-run".into(), "completed", operation);
        let mut telemetry = super::super::CaptureTelemetry::default();
        telemetry.retain_eval_observation(&observation).unwrap();
        assert!(!telemetry.optional_operations_complete);
        assert_eq!(telemetry.inference_calls, None);
        assert!(attempt_observable(&observation, crate::judgement_benchmark::Arm::Off).is_err());
    }

    #[test]
    fn label_projection_does_not_accept_arbitrary_provider_prose() {
        let diagnosis = serde_json::json!({"advisory_model_labels":{"failure_category":"private unexpected prose"}});
        assert_eq!(diagnosis_label(&diagnosis), None);
        assert_eq!(
            diagnosis_label(
                &serde_json::json!({"advisory_model_labels":{"failure_category":"artifact_failure"}})
            ),
            Some("artifact_failure".into())
        );
    }
}
