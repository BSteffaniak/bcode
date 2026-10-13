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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptCoverageFailure {
    ModelEvidence(crate::workflow::ModelCoverageFailure),
    CanonicalAttemptIncomplete,
}

impl AttemptCoverageFailure {
    const fn operation(self) -> &'static str {
        use crate::workflow::ModelCoverageFailure;
        match self {
            Self::ModelEvidence(ModelCoverageFailure::MissingRequestBoundary) => {
                "eval.coverage.missing_request_boundary"
            }
            Self::ModelEvidence(ModelCoverageFailure::IncompleteHistory) => {
                "eval.coverage.incomplete_session_history"
            }
            Self::ModelEvidence(ModelCoverageFailure::MissingOrUnverifiedModelInputExport) => {
                "eval.coverage.missing_or_unverified_model_input_export"
            }
            Self::ModelEvidence(ModelCoverageFailure::UnknownModelApplicability) => {
                "eval.coverage.unknown_model_applicability"
            }
            Self::ModelEvidence(ModelCoverageFailure::NoObservedModelSession) => {
                "eval.coverage.no_observed_model_session"
            }
            Self::CanonicalAttemptIncomplete => "eval.canonical_attempt_coverage",
        }
    }
}

fn attempt_coverage_failure(completeness: &serde_json::Value) -> AttemptCoverageFailure {
    // New producers retain a finite typed reason set. Old/unknown representations stay
    // generic and incomplete; diagnostics never confer evidence authority.
    completeness
        .pointer("/boundaries/model_exports/failure_reasons")
        .and_then(|value| {
            serde_json::from_value::<Vec<crate::workflow::ModelCoverageFailure>>(value.clone()).ok()
        })
        .filter(|reasons| reasons.len() <= 5)
        .and_then(|reasons| reasons.first().copied())
        .map_or(
            AttemptCoverageFailure::CanonicalAttemptIncomplete,
            AttemptCoverageFailure::ModelEvidence,
        )
}

fn canonical_attempt_complete(completeness: &serde_json::Value) -> Result<(), ()> {
    if completeness.pointer("/boundaries/canonical_attempt/complete")
        == Some(&serde_json::Value::Bool(true))
    {
        Ok(())
    } else {
        Err(())
    }
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
    if operation.trigger == TriggerDisposition::NotApplicable {
        // Successful attempts have no failure to categorize, independently of arm.
        // This proves the baseline outcome, never categorization benefit.
        return if observation.terminal == Some(ObservationTerminal::Completed)
            && operation.application == Some(ApplicationDisposition::BaselineRetained)
            && operation.inference_complete
            && operation.inference.is_empty()
            && operation.selection.is_none()
            && operation.recommendation.is_none()
        {
            Ok(())
        } else {
            Err(())
        };
    }
    match arm {
        crate::judgement_benchmark::Arm::Off => {
            // No failure prerequisite means the real consumer retained baseline without
            // calling optional judgement, even when the configured arm is off.
            let closed_baseline = matches!(
                operation.trigger,
                TriggerDisposition::Disabled | TriggerDisposition::NotApplicable
            ) && operation.inference.is_empty();
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
            if !(closed_baseline || disabled_call)
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
    load_attempt_artifacts(&rep_dir, authorized_root, &repetition.artifacts, capture)?;
    capture.trial_latency_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
    Ok(())
}

// Shared with the execution path: bounded public artifacts only, never replay or inference.
fn load_attempt_artifacts(
    rep_dir: &Path,
    authorized_root: &Path,
    artifacts: &[bcode_eval_models::EvalArtifactRef],
    capture: &mut ConsumerCapture,
) -> Result<(), ()> {
    let rep_dir = confined(rep_dir, authorized_root)?;
    if !artifacts.iter().any(|artifact| {
        artifact.kind == "eval_judgement_observation"
            && artifact.path == Path::new("eval-judgement-observation.json")
    }) {
        return Err(());
    }
    capture.operation = "eval.canonical_attempt_coverage".into();
    let completeness = bounded_json(&rep_dir.join("capture-completeness.json"))?;
    canonical_attempt_complete(&completeness).map_err(|()| {
        capture.operation = attempt_coverage_failure(&completeness).operation().into();
    })?;
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
    apply_attempt(&observation, &diagnosis, capture)
}

#[cfg(test)]
mod tests {
    #[test]
    fn typed_coverage_reason_never_overrides_incomplete_attempt() {
        let incomplete = serde_json::json!({"boundaries": {
            "canonical_attempt": {"complete": false},
            "model_exports": {"failure_reasons": ["missing_or_unverified_model_input_export"]}
        }});
        assert!(super::canonical_attempt_complete(&incomplete).is_err());
        assert_eq!(
            super::attempt_coverage_failure(&incomplete).operation(),
            "eval.coverage.missing_or_unverified_model_input_export"
        );
        let future = serde_json::json!({"boundaries": {
            "canonical_attempt": {"complete": false},
            "model_exports": {"failure_reasons": ["future_reason"]}
        }});
        assert!(super::canonical_attempt_complete(&future).is_err());
        assert_eq!(
            super::attempt_coverage_failure(&future),
            super::AttemptCoverageFailure::CanonicalAttemptIncomplete
        );
    }

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
    fn canonical_capture_missing_or_unverified_is_not_a_successful_baseline() {
        assert!(
            canonical_attempt_complete(&serde_json::json!({
                "boundaries": {"canonical_attempt": {"complete": true}}
            }))
            .is_ok()
        );
        for incomplete in [
            serde_json::json!({}),
            serde_json::json!({
                "boundaries": {"canonical_attempt": {"complete": false}}
            }),
            serde_json::json!({
                "boundaries": {"canonical_attempt": {"complete": "true"}}
            }),
        ] {
            assert!(canonical_attempt_complete(&incomplete).is_err());
        }
    }

    #[test]
    fn completed_no_trigger_is_valid_for_either_arm_but_not_failed_or_incomplete() {
        use crate::judgement_benchmark::Arm;
        let observation = EvalAttemptJudgementObservation::new(
            "actual-run".into(),
            "completed",
            bcode_model::judgement::ConsumerOperationObservation {
                operation_id: 0,
                feature: Feature::EvalCategorization,
                trigger: TriggerDisposition::NotApplicable,
                inference: Vec::new(),
                inference_complete: true,
                application: Some(ApplicationDisposition::BaselineRetained),
                selection: None,
                recommendation: None,
            },
        );
        for arm in [Arm::Off, Arm::On] {
            assert!(attempt_observable(&observation, arm).is_ok());
            let mut failed = observation.clone();
            failed.terminal = Some(ObservationTerminal::Failed);
            assert!(attempt_observable(&failed, arm).is_err());
            let mut incomplete = observation.clone();
            incomplete.complete = false;
            assert!(attempt_observable(&incomplete, arm).is_err());
            let mut missing_trace = observation.clone();
            missing_trace.operation.inference_complete = false;
            assert!(attempt_observable(&missing_trace, arm).is_err());
            let mut malformed = observation.clone();
            malformed.operation.application = Some(ApplicationDisposition::Applied);
            assert!(attempt_observable(&malformed, arm).is_err());
            let mut future = observation.clone();
            future.schema_version += 1;
            assert!(attempt_observable(&future, arm).is_err());
        }
    }

    #[test]
    fn off_not_applicable_requires_complete_terminal_baseline_without_inference() {
        use crate::judgement_benchmark::Arm;
        let observation = EvalAttemptJudgementObservation::new(
            "actual-run".into(),
            "completed",
            bcode_model::judgement::ConsumerOperationObservation {
                operation_id: 0,
                feature: Feature::EvalCategorization,
                trigger: TriggerDisposition::NotApplicable,
                inference: Vec::new(),
                inference_complete: true,
                application: Some(ApplicationDisposition::BaselineRetained),
                selection: None,
                recommendation: None,
            },
        );
        assert!(attempt_observable(&observation, Arm::Off).is_ok());
        let request = serde_json::from_value(serde_json::json!({
            "preregistration_id": "development", "task_id": "actual-task", "repetition": 0,
            "feature": "eval_categorization", "arm": "off", "source": "live_consumer",
            "capture_id": "capture", "pairing_id": "pair", "isolated_feature": null,
            "subject": {"executable": "/isolated/bcode", "executable_sha256": "a".repeat(64),
                "artifact_id": "test-artifact", "configuration_sha256": "b".repeat(64),
                "configuration_complete": false, "provider": null, "model": null,
                "auth_profile": null, "model_profile": "sol", "state_id": "verified-test-state"},
            "cache_condition": "unknown", "timeout_ms": 1000
        }))
        .unwrap();
        let mut capture = ConsumerCapture {
            schema_version: super::super::CAPTURE_SCHEMA_VERSION,
            scope: CaptureScope::NormalEvalAttempt,
            request,
            operation: "test".into(),
            status: CaptureStatus::ConsumerUnavailable,
            output: None,
            consumer_executed: false,
            outcome: crate::judgement_benchmark::TerminalOutcome::InfrastructureFailure,
            operation_latency_ms: None,
            trial_latency_ms: None,
            authority_preserved: None,
            request_only: None,
            telemetry: super::super::CaptureTelemetry::default(),
        };
        let root = tempfile::tempdir().unwrap();
        let rep_dir = root.path().join("attempt");
        fs::create_dir(&rep_dir).unwrap();
        let artifacts = vec![bcode_eval_models::EvalArtifactRef {
            kind: "eval_judgement_observation".into(),
            path: "eval-judgement-observation.json".into(),
        }];
        let write = |name: &str, value: &serde_json::Value| {
            fs::write(rep_dir.join(name), serde_json::to_vec(value).unwrap()).unwrap();
        };
        let completeness =
            serde_json::json!({"boundaries":{"canonical_attempt":{"complete":true}}});
        let observation_json = serde_json::to_value(&observation).unwrap();
        write("capture-completeness.json", &completeness);
        write("eval-judgement-observation.json", &observation_json);
        write(
            "admission.json",
            &serde_json::json!({"run_id":"actual-run"}),
        );
        write("diagnosis.json", &serde_json::json!({}));
        let initial_capture = capture.clone();
        let original_subject = capture.request.subject.clone();
        assert!(load_attempt_artifacts(&rep_dir, root.path(), &artifacts, &mut capture).is_ok());
        assert_eq!(capture.request.subject, original_subject);
        // Exercise the same file-loading/correlation path execute calls, without launching a run.
        for (name, malformed) in [
            ("capture-completeness.json", serde_json::json!({})),
            (
                "admission.json",
                serde_json::json!({"run_id":"foreign-run"}),
            ),
            ("eval-judgement-observation.json", {
                let mut value = observation_json.clone();
                value["schema_version"] = serde_json::json!(2);
                value
            }),
            ("eval-judgement-observation.json", {
                let mut value = observation_json;
                value["terminal"] = serde_json::json!("future_terminal");
                value
            }),
        ] {
            let path = rep_dir.join(name);
            let original = fs::read(&path).unwrap();
            write(name, &malformed);
            let mut rejected = initial_capture.clone();
            assert!(
                load_attempt_artifacts(&rep_dir, root.path(), &artifacts, &mut rejected).is_err()
            );
            assert!(!rejected.consumer_executed);
            assert_eq!(rejected.request.subject, original_subject);
            fs::write(path, original).unwrap();
        }
        for name in [
            "capture-completeness.json",
            "admission.json",
            "eval-judgement-observation.json",
            "diagnosis.json",
        ] {
            let path = rep_dir.join(name);
            let original = fs::read(&path).unwrap();
            fs::remove_file(&path).unwrap();
            let mut rejected = initial_capture.clone();
            assert!(
                load_attempt_artifacts(&rep_dir, root.path(), &artifacts, &mut rejected).is_err()
            );
            assert!(!rejected.consumer_executed);
            fs::write(path, original).unwrap();
        }
        assert!(load_attempt_artifacts(&rep_dir, root.path(), &[], &mut capture).is_err());
        assert!(capture.consumer_executed);
        assert_eq!(capture.status, CaptureStatus::Captured);
        assert_eq!(
            capture.outcome,
            crate::judgement_benchmark::TerminalOutcome::Succeeded
        );
        assert_eq!(capture.request.subject.state_id, "verified-test-state");
        assert!(matches!(
            capture.output,
            Some(crate::judgement_benchmark::ConsumerOutput::Classification { label: None })
        ));
        // Either arm can observe a successful baseline without a categorization trigger.
        // Acceptance preserves that outcome, not proof of optional inference or benefit.
        assert!(attempt_observable(&observation, Arm::On).is_ok());
        assert_eq!(
            observation.operation.trigger,
            TriggerDisposition::NotApplicable
        );
        assert!(observation.operation.inference.is_empty());
        let mut invalid = observation.clone();
        invalid.complete = false;
        assert!(attempt_observable(&invalid, Arm::Off).is_err());
        invalid = observation.clone();
        invalid.terminal = None;
        assert!(attempt_observable(&invalid, Arm::Off).is_err());
        invalid = observation.clone();
        invalid.operation.inference_complete = false;
        assert!(attempt_observable(&invalid, Arm::Off).is_err());
        invalid = observation.clone();
        invalid.operation.application = Some(ApplicationDisposition::Applied);
        assert!(attempt_observable(&invalid, Arm::Off).is_err());
        invalid = observation.clone();
        invalid
            .operation
            .inference
            .push(bcode_model::judgement::OptionalOperationObservation {
                outcome: bcode_model::judgement::OptionalOperationOutcome::Disabled,
                discovery_invocations: 0,
                model_listing_invocations: 0,
                verification_invocations: 0,
                inference_invocations: 0,
                ..bcode_model::judgement::OptionalOperationObservation::default()
            });
        assert!(attempt_observable(&invalid, Arm::Off).is_err());
        let mut encoded = serde_json::to_value(&observation).unwrap();
        encoded["operation"]["trigger"] = serde_json::json!("future_trigger");
        assert!(serde_json::from_value::<EvalAttemptJudgementObservation>(encoded).is_err());
        invalid = observation;
        invalid.schema_version += 1;
        assert!(attempt_observable(&invalid, Arm::Off).is_err());
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
