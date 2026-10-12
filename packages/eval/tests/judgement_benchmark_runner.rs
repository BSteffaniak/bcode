//! Offline capture fault/calibration tests plus an explicitly opt-in public consumer driver.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_eval::judgement_benchmark::{Arm, EvidenceSource, TerminalOutcome};
use bcode_eval::judgement_capture::{
    CAPTURE_SCHEMA_VERSION, CacheCondition, CallMeasurement, CaptureRequest, CaptureScope,
    CaptureStatus, CaptureTelemetry, ConsumerCapture, ConsumerRequest, DecisionCapture,
    MAX_CAPTURE_BYTES, SubjectIdentity, capture_bounded,
};
use bcode_model::judgement::Feature;
use std::{fs::File, io::Read, path::Path};

fn request() -> CaptureRequest {
    CaptureRequest {
        preregistration_id: "independent-v1".into(),
        task_id: "held-out-case".into(),
        repetition: 0,
        feature: Feature::ContextReranking,
        arm: Arm::Off,
        source: EvidenceSource::ScorerCalibration,
        capture_id: "calibration".into(),
        pairing_id: "immutable-input-settings".into(),
        isolated_feature: Some(true),
        cache_condition: CacheCondition::Unknown,
        timeout_ms: 100,
        subject: SubjectIdentity {
            executable: "/explicit/subject/bcode".into(),
            executable_sha256: "a".repeat(64),
            artifact_id: "exact-artifact".into(),
            configuration_sha256: "b".repeat(64),
            configuration_complete: true,
            provider: Some("normalized-provider".into()),
            model: Some("concrete-model".into()),
            auth_profile: None,
            model_profile: None,
            state_id: "isolated-state".into(),
        },
    }
}

fn capture() -> ConsumerCapture {
    ConsumerCapture {
        schema_version: CAPTURE_SCHEMA_VERSION,
        scope: CaptureScope::PublicOperation,
        request: request(),
        operation: "calibration".into(),
        status: CaptureStatus::Captured,
        output: None,
        consumer_executed: true,
        outcome: TerminalOutcome::Succeeded,
        operation_latency_ms: Some(1.0),
        trial_latency_ms: None,
        authority_preserved: Some(true),
        request_only: Some(true),
        telemetry: CaptureTelemetry::default(),
    }
}

#[test]
fn unknown_telemetry_is_not_free_or_zero() {
    let capture = capture();
    assert_eq!(capture.telemetry.total_cost_usd(), None);
    assert!(capture.benchmark_observation().is_err());
}

#[test]
fn normal_turn_zero_counts_require_explicit_closed_coverage() {
    let mut capture = capture();
    capture.scope = CaptureScope::NormalTurn;
    capture.telemetry.discovery_calls = Some(0);
    capture.telemetry.verification_calls = Some(0);
    capture.telemetry.inference_calls = Some(0);
    assert_eq!(
        capture.benchmark_observation().unwrap_err(),
        "normal-turn optional observation coverage unknown"
    );
    capture.telemetry.optional_operations_complete = true;
    assert!(capture.benchmark_observation().is_ok());
    assert_eq!(capture.telemetry.total_cost_usd(), None);

    // A later operation without facts reopens coverage and permanently invalidates totals.
    capture
        .telemetry
        .retain_optional_response(&bcode_model::judgement::OptionalResponse {
            response: None,
            observation: None,
        });
    assert!(!capture.telemetry.optional_operations_complete);
    assert_eq!(capture.telemetry.discovery_calls, None);
    assert_eq!(capture.telemetry.verification_calls, None);
    assert_eq!(capture.telemetry.inference_calls, None);
    // Neither a caller-supplied completeness flag nor counters can erase the known gap.
    capture.telemetry.optional_operations_complete = true;
    capture.telemetry.discovery_calls = Some(0);
    capture.telemetry.verification_calls = Some(0);
    capture.telemetry.inference_calls = Some(0);
    assert!(capture.benchmark_observation().is_err());
}

#[test]
fn optional_operation_retention_is_bounded_without_false_totals() {
    let mut telemetry = CaptureTelemetry::default();
    let response = bcode_model::judgement::OptionalResponse {
        response: None,
        observation: Some(bcode_model::judgement::OptionalOperationObservation {
            inference_invocations: 1,
            ..Default::default()
        }),
    };
    for _ in 0..1025 {
        telemetry.retain_optional_response(&response);
    }
    assert_eq!(telemetry.optional_operations.len(), 1024);
    assert!(telemetry.optional_counts_incomplete);
    assert!(!telemetry.optional_operations_complete);
    assert_eq!(telemetry.inference_calls, None);
    assert_eq!(telemetry.total_cost_usd(), None);
}

#[test]
fn legacy_capture_does_not_claim_normal_turn_coverage() {
    let mut value = serde_json::to_value(capture()).unwrap();
    value["telemetry"]
        .as_object_mut()
        .unwrap()
        .remove("optional_operations_complete");
    let legacy: ConsumerCapture = serde_json::from_value(value).unwrap();
    assert!(!legacy.telemetry.optional_operations_complete);
    assert!(legacy.telemetry.aggregated_turn_observation.is_none());
}

#[test]
fn mixed_v2_aggregate_and_detailed_operation_round_trip_without_legacy_guessing() {
    use bcode_model::judgement::{
        ApplicationDisposition, ConsumerOperationObservation, ObservationScope,
        ObservationTerminal, OperationAccounting, PassiveOperationAggregate, SelectionObservation,
        TURN_OBSERVATION_VERSION, TriggerDisposition, TurnJudgementObservation,
    };
    let detailed = ConsumerOperationObservation {
        operation_id: 100,
        feature: Feature::ContextReranking,
        trigger: TriggerDisposition::Disabled,
        inference: vec![],
        inference_complete: true,
        application: Some(ApplicationDisposition::BaselineRetained),
        selection: Some(SelectionObservation {
            candidate_count: 1,
            ordered: vec![0],
            admitted: vec![0],
        }),
        recommendation: None,
    };
    let snapshot = TurnJudgementObservation {
        schema_version: TURN_OBSERVATION_VERSION,
        scope: ObservationScope::NormalTurn,
        session_id: "session".into(),
        turn_id: "turn".into(),
        operations: vec![detailed],
        accounting: Some(OperationAccounting {
            begun: 201,
            omitted_operations: 0,
        }),
        passive_aggregates: vec![PassiveOperationAggregate {
            feature: Feature::ProgressMonitoring,
            trigger: TriggerDisposition::Disabled,
            application: ApplicationDisposition::BaselineRetained,
            count: 200,
            first_operation_id: 0,
            last_operation_id: 200,
        }],
        covered_features: vec![Feature::ContextReranking, Feature::ProgressMonitoring],
        terminal: Some(ObservationTerminal::Completed),
        omitted: 0,
        // Other required hooks are absent: accounting alone does not certify coverage.
        complete: false,
    };
    assert!(snapshot.validate().is_ok());
    let mut telemetry = CaptureTelemetry {
        aggregated_turn_observation: Some(snapshot),
        ..CaptureTelemetry::default()
    };
    let decoded: CaptureTelemetry =
        serde_json::from_slice(&serde_json::to_vec(&telemetry).unwrap()).unwrap();
    let retained = decoded.aggregated_turn_observation.unwrap();
    assert!(retained.validate().is_ok());
    assert_eq!(retained.accounting.as_ref().unwrap().begun, 201);
    assert_eq!(retained.passive_aggregates[0].count, 200);
    assert!(retained.operations[0].selection.is_some());
    assert!(!decoded.optional_operations_complete);
    assert_eq!(decoded.inference_calls, None);
    telemetry.aggregated_turn_observation = None;
    let legacy_value = serde_json::to_value(telemetry).unwrap();
    assert!(legacy_value.get("aggregated_turn_observation").is_none());
    let legacy: CaptureTelemetry = serde_json::from_value(legacy_value).unwrap();
    assert!(legacy.aggregated_turn_observation.is_none());
    let mut invalid = retained;
    invalid.complete = true;
    assert!(invalid.validate().is_err());
    invalid.complete = false;
    invalid.accounting.as_mut().unwrap().begun += 1;
    assert!(invalid.validate().is_err());
}

#[test]
fn off_work_future_versions_and_unverified_provenance_fail_closed() {
    let mut capture = capture();
    capture.telemetry = CaptureTelemetry {
        // Synthetic calibration supplies no decision-stage evidence.
        decisions: DecisionCapture::default(),
        calls_complete: true,
        discovery_calls: Some(0),
        verification_calls: Some(0),
        inference_calls: Some(0),
        verification_cache_hit: None,
        optional_operation: None,
        optional_operations: vec![],
        aggregated_turn_observation: None,
        optional_counts_incomplete: false,
        optional_operations_complete: false,
        calls: vec![],
    };
    assert!(capture.telemetry.decisions.stages.is_empty());
    assert_eq!(capture.telemetry.decisions.omitted_stages, 0);
    let observation = capture.benchmark_observation().unwrap();
    assert_eq!(observation.latency_ms, None);
    assert_eq!(observation.cost_usd, Some(0.0)); // Explicitly complete, empty ledger only.
    assert_eq!(
        observation.evidence.source,
        EvidenceSource::ScorerCalibration
    );
    capture.telemetry.inference_calls = Some(1);
    assert!(capture.benchmark_observation().is_err());
    capture.telemetry.inference_calls = None;
    assert!(capture.benchmark_observation().is_err());
    capture.telemetry.inference_calls = Some(0);
    capture.schema_version = CAPTURE_SCHEMA_VERSION.checked_add(1).unwrap();
    assert!(capture.benchmark_observation().is_err());
    capture.schema_version = CAPTURE_SCHEMA_VERSION;
    assert!(capture.benchmark_observation().is_ok());
    capture.request.subject.configuration_complete = false;
    assert!(capture.benchmark_observation().is_err());
}

#[test]
fn verification_retries_and_missing_pricing_are_retained() {
    let call = CallMeasurement {
        operation: "verification".into(),
        attempt: 0,
        provider: Some("provider".into()),
        model: Some("model".into()),
        duration_ms: Some(4.0),
        outcome: TerminalOutcome::Failed,
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        pricing_id: Some("price-v1".into()),
        cost_usd: Some(0.01),
    };
    let mut telemetry = CaptureTelemetry {
        calls_complete: true,
        calls: vec![call.clone(), CallMeasurement { attempt: 1, ..call }],
        ..CaptureTelemetry::default()
    };
    assert_eq!(telemetry.total_cost_usd(), Some(0.02));
    telemetry.calls[1].cost_usd = None;
    assert_eq!(telemetry.total_cost_usd(), None);
    telemetry.calls[1].cost_usd = Some(0.0);
    telemetry.calls[1].pricing_id = None;
    assert_eq!(telemetry.total_cost_usd(), None);
    telemetry.calls[1].pricing_id = Some("price-v1".into());
    telemetry.calls[1].cost_usd = Some(-1.0);
    assert_eq!(telemetry.total_cost_usd(), None);
}

#[test]
fn corpus_materializer_outputs_actual_typed_consumer_variants() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = root.join("packages/eval/src/judgement_capture/materialize.py");
    let code = r"
import json, pathlib, runpy, sys
m = runpy.run_path(sys.argv[1])
result = []
for split in ('development', 'held-out'):
    corpus = json.loads((pathlib.Path(sys.argv[2]) / 'fixtures/evals/judgement' / (split + '.json')).read_text())
    for case in corpus['cases']:
        try:
            result.append(m['adapt'](case, 'task'))
        except ValueError:
            result.append({'kind':'unavailable'})
print(json.dumps(result))
";
    let output = std::process::Command::new("python3")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .args(["-c", code])
        .arg(script)
        .arg(&root)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.len() <= MAX_CAPTURE_BYTES);
    let consumers: Vec<ConsumerRequest> = serde_json::from_slice(&output.stdout).unwrap();
    let expected_count: usize = ["development", "held-out"]
        .into_iter()
        .map(|split| {
            let corpus: serde_json::Value = serde_json::from_slice(
                &std::fs::read(root.join(format!("fixtures/evals/judgement/{split}.json")))
                    .unwrap(),
            )
            .unwrap();
            corpus["cases"].as_array().unwrap().len()
        })
        .sum();
    assert_eq!(consumers.len(), expected_count);
    assert!(
        consumers
            .iter()
            .all(|consumer| !matches!(consumer, ConsumerRequest::NormalTurn { .. }))
    );
}

#[test]
fn bounded_jobs_reject_future_versions_unknown_fields_and_oversize_without_execution() {
    use bcode_eval::judgement_capture::{CaptureJob, decode_capture_job};
    let mut job = CaptureJob {
        schema_version: 1,
        request: request(),
        consumer: ConsumerRequest::Unavailable,
        allow_live: false,
    };
    let encoded = serde_json::to_vec(&job).unwrap();
    assert!(decode_capture_job(&encoded).is_ok());
    job.schema_version = 2;
    assert!(decode_capture_job(&serde_json::to_vec(&job).unwrap()).is_err());
    let mut value = serde_json::from_slice::<serde_json::Value>(&encoded).unwrap();
    value["unexpected"] = true.into();
    assert!(decode_capture_job(&serde_json::to_vec(&value).unwrap()).is_err());
    assert!(decode_capture_job(&vec![b' '; MAX_CAPTURE_BYTES + 1]).is_err());
}

#[test]
fn microbench_does_not_claim_whole_task_efficiency() {
    let mut capture = capture();
    capture.scope = CaptureScope::ServiceMicrobenchmark;
    capture.telemetry.discovery_calls = Some(0);
    capture.telemetry.verification_calls = Some(0);
    capture.telemetry.inference_calls = Some(0);
    let observation = capture.benchmark_observation().unwrap();
    assert_eq!(observation.cost_usd, None);
    assert_eq!(observation.latency_ms, None);
    capture.trial_latency_ms = Some(1.0);
    assert!(capture.benchmark_observation().is_err());
}

#[test]
fn opt_in_bounds_and_unsupported_consumers_need_no_service() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = bcode_client::BcodeClient::new(bcode_ipc::IpcEndpoint::unix_socket(
        "/nonexistent/judgement-test.sock",
    ));
    let mut live = request();
    live.source = EvidenceSource::LiveConsumer;
    assert!(
        runtime
            .block_on(capture_bounded(
                &client,
                live,
                ConsumerRequest::Unavailable,
                false
            ))
            .is_err()
    );
    let unavailable = runtime
        .block_on(capture_bounded(
            &client,
            request(),
            ConsumerRequest::Unavailable,
            false,
        ))
        .unwrap();
    assert_eq!(unavailable.status, CaptureStatus::ConsumerUnavailable);
    assert!(!unavailable.consumer_executed);
    assert!(unavailable.benchmark_observation().is_err());
    let mut invalid = request();
    invalid.timeout_ms = 0;
    assert!(
        runtime
            .block_on(capture_bounded(
                &client,
                invalid,
                ConsumerRequest::Unavailable,
                false
            ))
            .is_err()
    );
}

#[test]
fn context_product_status_and_snapshot_shape_do_not_guess_future_evidence() {
    use bcode_model::judgement::{
        ApplicationDisposition, SelectionObservation, TriggerDisposition,
    };
    let disabled: TriggerDisposition = serde_json::from_str("\"disabled\"").unwrap();
    assert_eq!(disabled, TriggerDisposition::Disabled);
    let retained: ApplicationDisposition = serde_json::from_str("\"baseline_retained\"").unwrap();
    assert_eq!(retained, ApplicationDisposition::BaselineRetained);
    assert!(serde_json::from_str::<TriggerDisposition>("\"future_trigger\"").is_err());
    assert!(serde_json::from_str::<ApplicationDisposition>("\"future_application\"").is_err());
    assert!(
        serde_json::from_value::<SelectionObservation>(serde_json::json!({
            "candidate_count": 2, "ordered": [1, 0], "admitted": [1], "task_correct": true
        }))
        .is_err()
    );
}

fn bounded_json<T: serde::de::DeserializeOwned>(path: &Path) -> T {
    let mut bytes = Vec::new();
    File::open(path)
        .unwrap()
        .take(u64::try_from(MAX_CAPTURE_BYTES).unwrap() + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(
        bytes.len() <= MAX_CAPTURE_BYTES,
        "input exceeds bounded capture contract"
    );
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
#[ignore = "explicit bounded captured-artifact report; no provider execution"]
fn report_captured_benchmark() {
    let plan: bcode_eval::judgement_benchmark::BenchmarkPlan =
        bounded_json(Path::new(&std::env::var("BCODE_JUDGEMENT_PLAN").unwrap()));
    let captures: Vec<ConsumerCapture> = bounded_json(Path::new(
        &std::env::var("BCODE_JUDGEMENT_CAPTURES").unwrap(),
    ));
    bcode_eval::judgement_benchmark::validate_service_oracle_vocabulary(&plan)
        .expect("service oracle vocabulary mapping gap; not a model quality failure");
    let (report, gaps) = bcode_eval::judgement_capture::compare_captures(&plan, &captures).unwrap();
    let artifact = match std::env::var("BCODE_JUDGEMENT_REPORT_MODE").as_deref() {
        Ok("quality_only") => {
            let quality = bcode_eval::judgement_benchmark::compare_benchmark_quality_only(
                &plan,
                &report.observations,
            )
            .unwrap();
            serde_json::json!({"quality_report":quality,"capture_gaps":gaps})
        }
        Ok("full_readiness") | Err(std::env::VarError::NotPresent) => {
            serde_json::json!({"report":report,"capture_gaps":gaps})
        }
        _ => panic!("unsupported BCODE_JUDGEMENT_REPORT_MODE"),
    };
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(std::env::var("BCODE_JUDGEMENT_REPORT_OUTPUT").unwrap())
        .unwrap();
    std::io::Write::write_all(&mut output, &serde_json::to_vec_pretty(&artifact).unwrap()).unwrap();
}

/// Hosting coordinator supplies a verified require-running exact-artifact endpoint and treatment.
/// No model discovery, daemon startup, credential inspection, or silent executable substitution.
#[test]
#[ignore = "explicit live public consumer capture; may incur provider cost"]
fn live_public_consumer_capture() {
    assert_eq!(
        std::env::var("BCODE_JUDGEMENT_ALLOW_LIVE").as_deref(),
        Ok("1")
    );
    let binary = std::env::var("BCODE_JUDGEMENT_SUBJECT_BINARY").unwrap();
    assert!(Path::new(&binary).is_absolute());
    let job_path = std::env::var("BCODE_JUDGEMENT_JOB").unwrap();
    let job: bcode_eval::judgement_capture::CaptureJob = bounded_json(Path::new(&job_path));
    assert!(job.allow_live);
    assert_eq!(job.request.source, EvidenceSource::LiveConsumer);
    let output = std::env::var("BCODE_JUDGEMENT_CAPTURE_OUTPUT").unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)
        .unwrap();
    let mut child = std::process::Command::new(binary)
        .args(["judgement", "benchmark-capture", "--file", &job_path])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(file))
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if started.elapsed().as_secs() >= 310 {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("subject CLI capture timed out; daemon-side effects remain unknown");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let capture: ConsumerCapture = bounded_json(Path::new(&output));
    assert_eq!(capture.request.source, EvidenceSource::LiveConsumer);
}
