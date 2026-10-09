//! Opt-in real normal-turn context + independent artifact oracle checkpoint.
//! The matching subject CLI owns verified `BcodeClient` admission; this test never
//! starts a daemon, edits provider selection, seeds private storage or grants permissions.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_eval::judgement_benchmark::{Arm, EvidenceSource};
use bcode_eval::judgement_capture::{
    CaptureJob, CaptureScope, ConsumerCapture, ConsumerRequest, MAX_CAPTURE_BYTES,
    decode_capture_job,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Experiment {
    schema_version: u32,
    /// Existing isolated state, served by this exact subject; never the worker state.
    subject_binary: PathBuf,
    subject_sha256: String,
    config_file: PathBuf,
    state_root: PathBuf,
    profile: String,
    context: Option<String>,
    /// Existing coordinator-prepared normal-turn capture job with exact history references.
    capture_job: PathBuf,
    workspace: PathBuf,
    /// Independent oracle module, not copied to the consumer workspace.
    oracle_module: PathBuf,
    oracle_sha256: String,
    /// Exact companion scenario source used by the oracle; included in identity verification.
    scenarios_sha256: String,
    python_binary: PathBuf,
    scenario_id: String,
    artifact: PathBuf,
    /// New external report directory; must already exist and be empty.
    output_directory: PathBuf,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    evidence_scope: &'static str,
    subject_sha256: String,
    oracle_sha256: String,
    scenarios_sha256: String,
    capture_job_sha256: String,
    artifact_sha256: Option<String>,
    capture: Option<ConsumerCapture>,
    oracle: Option<serde_json::Value>,
    gap: Option<&'static str>,
}

fn bytes(path: &Path, limit: usize) -> Result<Vec<u8>, &'static str> {
    let mut result = Vec::new();
    File::open(path)
        .map_err(|_| "file unavailable")?
        .take(limit as u64 + 1)
        .read_to_end(&mut result)
        .map_err(|_| "file unreadable")?;
    if result.len() > limit {
        return Err("file exceeds bound");
    }
    Ok(result)
}
fn digest(path: &Path) -> Result<String, &'static str> {
    let mut file = File::open(path).map_err(|_| "identity artifact unavailable")?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| "identity artifact unreadable")?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hex::encode(hash.finalize()))
}
fn checked_path(path: &Path) -> Result<PathBuf, &'static str> {
    if !path.is_absolute()
        || path
            .symlink_metadata()
            .map_err(|_| "path unavailable")?
            .file_type()
            .is_symlink()
    {
        return Err("absolute non-symlink path required");
    }
    path.canonicalize().map_err(|_| "path unavailable")
}
fn validate(job: &Experiment, capture: &CaptureJob) -> Result<(), &'static str> {
    if job.schema_version != 1 || job.profile.is_empty() || job.scenario_id.is_empty() {
        return Err("unsupported or incomplete experiment job");
    }
    for path in [
        &job.subject_binary,
        &job.config_file,
        &job.state_root,
        &job.workspace,
        &job.oracle_module,
        &job.python_binary,
        &job.output_directory,
        &job.capture_job,
    ] {
        checked_path(path)?;
    }
    let external = Path::new("/Volumes/ehdd/bcode-eval-artifacts")
        .canonicalize()
        .map_err(|_| "external artifact root unavailable")?;
    if !checked_path(&job.state_root)?.starts_with(&external)
        || !checked_path(&job.output_directory)?.starts_with(&external)
        || !checked_path(&job.workspace)?.starts_with(&external)
        || checked_path(&job.oracle_module)?.starts_with(checked_path(&job.workspace)?)
    {
        return Err("isolated external state/workspace/output and external oracle required");
    }
    if job.artifact.is_absolute()
        || job.artifact.components().count() != 1
        || !matches!(
            job.artifact.components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err("single relative artifact filename required");
    }
    if job
        .output_directory
        .read_dir()
        .map_err(|_| "output unavailable")?
        .next()
        .is_some()
    {
        return Err("fresh empty output directory required");
    }
    if digest(&job.subject_binary)? != job.subject_sha256
        || digest(&job.oracle_module)? != job.oracle_sha256
        || digest(&job.oracle_module.with_file_name("context_scenarios.py"))?
            != job.scenarios_sha256
        || capture.request.subject.executable_sha256 != job.subject_sha256
        || Path::new(&capture.request.subject.executable) != job.subject_binary
    {
        return Err("subject or independent oracle identity mismatch");
    }
    if !capture.allow_live
        || capture.request.source != EvidenceSource::LiveConsumer
        || capture.request.feature != bcode_model::judgement::Feature::ContextReranking
    {
        return Err("explicit live context job required");
    }
    let ConsumerRequest::NormalTurn { request } = &capture.consumer else {
        return Err("ordinary turn required");
    };
    if request.admission.execution.schema_version != 8
        || request.admission.execution.historical_context.is_empty()
        || request
            .admission
            .execution
            .historical_context
            .iter()
            .any(|reference| reference.session_id != request.session_id)
        || request.admission.execution.provider_plugin_id.as_ref()
            != capture.request.subject.provider.as_ref()
        || request.admission.execution.model_id.as_ref() != capture.request.subject.model.as_ref()
        || request.admission.execution.provider_plugin_id.is_none()
        || request.admission.execution.model_id.is_none()
        || request.admission.execution.permission_mode
            == bcode_session_models::TurnPermissionMode::Bypass
    {
        return Err("exact ordinary history/model admission without permission bypass required");
    }
    Ok(())
}

fn wait(child: &mut std::process::Child, timeout: Duration) -> Result<bool, &'static str> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|_| "child status unavailable")? {
            return Ok(status.success());
        }
        if start.elapsed() >= timeout {
            // Terminating the local adapter does not cancel daemon-side execution.
            child.kill().map_err(|_| "adapter termination failed")?;
            child.wait().map_err(|_| "adapter reap failed")?;
            return Err(
                "adapter timed out; daemon effects unresolved; coordinator cancellation required",
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn output_file(path: &Path) -> File {
    File::options()
        .write(true)
        .create_new(true)
        .open(path)
        .expect("new output file")
}
fn launch(job: &Experiment) -> Result<bool, &'static str> {
    let mut command = Command::new(&job.subject_binary);
    command
        .env("BCODE_CONFIG", &job.config_file)
        .args(["--state-root"])
        .arg(&job.state_root)
        .args(["--profile", &job.profile]);
    if let Some(context) = &job.context {
        command.args(["--context", context]);
    }
    let mut child = command
        .args(["judgement", "benchmark-capture", "--file"])
        .arg(&job.capture_job)
        .current_dir(&job.workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output_file(
            &job.output_directory.join("capture.json"),
        )))
        // Subject error bodies may contain private data; never import them into the report.
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "subject adapter unavailable")?;
    wait(&mut child, Duration::from_secs(330))
}
fn cancel_prepared_turn(job: &Experiment, capture: &CaptureJob) -> Result<(), &'static str> {
    let ConsumerRequest::NormalTurn { request } = &capture.consumer else {
        return Err("ordinary turn required");
    };
    let mut command = Command::new(&job.subject_binary);
    command
        .env("BCODE_CONFIG", &job.config_file)
        .arg("--state-root")
        .arg(&job.state_root)
        .args(["--profile", &job.profile]);
    if let Some(context) = &job.context {
        command.args(["--context", context]);
    }
    let mut child = command
        .arg("cancel")
        .arg(request.session_id.to_string())
        .arg("--json")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "cancellation adapter unavailable")?;
    if wait(&mut child, Duration::from_secs(30))? {
        Ok(())
    } else {
        Err("cancellation request failed")
    }
}

fn oracle(job: &Experiment) -> Result<serde_json::Value, &'static str> {
    let mut child = Command::new(&job.python_binary).args(["-B", "-c",
        "import importlib.util,json,sys; spec=importlib.util.spec_from_file_location('independent_context_oracle',sys.argv[1]); module=importlib.util.module_from_spec(spec); sys.path.insert(0,str(__import__('pathlib').Path(sys.argv[1]).parent)); spec.loader.exec_module(module); print(json.dumps(module.grade(sys.argv[2],sys.argv[3])))"])
        .arg(&job.oracle_module).arg(&job.scenario_id).arg(&job.workspace)
        .env("PYTHONDONTWRITEBYTECODE", "1").stdin(Stdio::null())
        .stdout(Stdio::from(output_file(&job.output_directory.join("oracle.json"))))
        .stderr(Stdio::null()).spawn().map_err(|_| "oracle adapter unavailable")?;
    if !wait(&mut child, Duration::from_secs(10))? {
        return Err("oracle failed");
    }
    serde_json::from_slice(&bytes(&job.output_directory.join("oracle.json"), 16384)?)
        .map_err(|_| "invalid oracle output")
}

#[test]
#[ignore = "coordinator-only real provider experiment; needs isolated prepared subject/session"]
fn live_context_artifact_checkpoint() {
    let path = std::env::var_os("BCODE_LIVE_CONTEXT_JOB").expect("explicit experiment job path");
    let job: Experiment =
        serde_json::from_slice(&bytes(Path::new(&path), MAX_CAPTURE_BYTES).unwrap()).unwrap();
    let raw_job = bytes(&job.capture_job, MAX_CAPTURE_BYTES).unwrap();
    let capture_job = decode_capture_job(&raw_job).unwrap();
    validate(&job, &capture_job).unwrap();
    let mut report = Report {
        schema_version: 1,
        evidence_scope: "subject_cli_capture_plus_external_oracle_not_authenticated_effectiveness",
        subject_sha256: job.subject_sha256.clone(),
        oracle_sha256: job.oracle_sha256.clone(),
        scenarios_sha256: job.scenarios_sha256.clone(),
        capture_job_sha256: hex::encode(Sha256::digest(&raw_job)),
        artifact_sha256: None,
        capture: None,
        oracle: None,
        gap: None,
    };
    match launch(&job) {
        Ok(true) => {
            report.capture = serde_json::from_slice(
                &bytes(
                    &job.output_directory.join("capture.json"),
                    MAX_CAPTURE_BYTES,
                )
                .unwrap(),
            )
            .ok();
            if report.capture.as_ref().is_none_or(|capture| {
                capture.scope != CaptureScope::NormalTurn
                    || !capture.consumer_executed
                    || capture.output.is_none()
                    || !capture.telemetry.optional_operations_complete
                    || capture.telemetry.optional_counts_incomplete
                    || capture.request.capture_id != capture_job.request.capture_id
                    || capture.request.pairing_id != capture_job.request.pairing_id
            }) {
                report.gap = Some("missing or mismatched ordinary capture");
            }
            let artifact = job.workspace.join(&job.artifact);
            if artifact
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
            {
                report.artifact_sha256 = bytes(&artifact, 16384)
                    .ok()
                    .map(|bytes| hex::encode(Sha256::digest(bytes)));
            }
            match oracle(&job) {
                Ok(value) => report.oracle = Some(value),
                Err(gap) => report.gap = Some(gap),
            }
            if capture_job.request.arm == Arm::On
                && report.capture.as_ref().is_some_and(|capture| {
                    capture
                        .telemetry
                        .inference_calls
                        .is_none_or(|calls| calls == 0)
                        || !capture
                            .telemetry
                            .decisions
                            .stages
                            .iter()
                            .any(|stage| stage.applied == Some(true))
                })
            {
                report.gap =
                    Some("on inference/application evidence unavailable; no benefit attribution");
            }
            if capture_job.request.arm == Arm::Off
                && report.capture.as_ref().is_some_and(|capture| {
                    capture.telemetry.discovery_calls != Some(0)
                        || capture.telemetry.verification_calls != Some(0)
                        || capture.telemetry.inference_calls != Some(0)
                })
            {
                report.gap = Some("off optional-call coverage not verified zero");
            }
        }
        Ok(false) => report.gap = Some("subject CLI failed; no quality attribution"),
        Err(gap) => report.gap = Some(gap),
    }
    if report.gap.is_some() {
        // Request normal application cancellation, without claiming terminal closure
        // or absence of effects merely because the adapter was stopped.
        if cancel_prepared_turn(&job, &capture_job).is_err() {
            report.gap = Some(
                "checkpoint failed and cancellation unconfirmed; coordinator reconciliation required",
            );
        }
    }
    output_file(&job.output_directory.join("checkpoint.json"))
        .write_all(&serde_json::to_vec_pretty(&report).unwrap())
        .unwrap();
    assert!(
        report.gap.is_none(),
        "checkpoint unavailable; consult bounded checkpoint.json"
    );
    // A correctly captured oracle loss is a result, not harness failure or promotion evidence.
}

#[test]
fn subprocess_timeout_does_not_claim_cancellation_or_success() {
    let mut child = Command::new("/bin/sleep")
        .arg("1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(
        wait(&mut child, Duration::from_millis(1))
            .unwrap_err()
            .contains("effects unresolved")
    );
}
