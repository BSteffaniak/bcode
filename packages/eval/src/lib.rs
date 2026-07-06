#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(
    clippy::cast_precision_loss,
    clippy::format_push_string,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::trivially_copy_pass_by_ref
)]

//! Leaf eval runner, judges, reports, comparisons, and baselines for Bcode.

use bcode_eval_models::{
    CURRENT_SCHEMA_VERSION, EvalArtifactRef, EvalBaseline, EvalCase, EvalCaseRunResult,
    EvalComparisonReport, EvalComparisonVariant, EvalDiagnostic, EvalDiagnosticSeverity,
    EvalExecutorKind, EvalIsolation, EvalJudgeConfig, EvalJudgeResult, EvalMeasurementSet,
    EvalMetricDirection, EvalObservation, EvalRegexTarget, EvalRegressionReport,
    EvalRepetitionResult, EvalRunManifest, EvalRunResult, EvalSuite, EvalVariant,
    EvalVariantRunResult,
};
use regex::Regex;
use serde::Serialize;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Errors produced by eval loading, execution, judging, and reporting.
#[derive(Debug, Error)]
pub enum EvalError {
    /// Filesystem operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization/deserialization failed.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// TOML serialization/deserialization failed.
    #[error("TOML error: {0}")]
    TomlDeserialize(#[from] toml::de::Error),
    /// TOML serialization failed.
    #[error("TOML serialization error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    /// Suite validation failed.
    #[error("suite validation failed: {0}")]
    Validation(String),
    /// Requested executor is not supported by the selected configuration.
    #[error("unsupported executor for variant {variant_id}: {reason}")]
    UnsupportedExecutor { variant_id: String, reason: String },
    /// A command failed before producing an exit status.
    #[error("command failed: {0}")]
    Command(String),
    /// Client operation failed.
    #[error("client error: {0}")]
    Client(String),
    /// Config override failed.
    #[error("config error: {0}")]
    Config(String),
    /// Regex compilation failed.
    #[error("regex error: {0}")]
    Regex(#[from] regex::Error),
}

/// Options for running a suite.
#[derive(Debug, Clone)]
pub struct EvalRunOptions {
    /// Suite manifest path.
    pub suite_path: PathBuf,
    /// Output root. Defaults to `target/bcode-evals/runs`.
    pub output_root: PathBuf,
    /// Optional explicit run id.
    pub run_id: Option<String>,
}

impl EvalRunOptions {
    /// Create run options for a suite with default output root.
    #[must_use]
    pub fn new(suite_path: impl Into<PathBuf>) -> Self {
        Self {
            suite_path: suite_path.into(),
            output_root: PathBuf::from("target/bcode-evals/runs"),
            run_id: None,
        }
    }
}

/// Load and validate a suite from a TOML file.
///
/// # Errors
///
/// Returns an error when the file cannot be read, parsed, or validated.
pub fn load_suite(path: impl AsRef<Path>) -> Result<EvalSuite, EvalError> {
    let contents = fs::read_to_string(path)?;
    let suite = toml::from_str::<EvalSuite>(&contents)?;
    validate_suite(&suite)?;
    Ok(suite)
}

/// Validate a suite.
///
/// # Errors
///
/// Returns an error when ids/configuration are missing or inconsistent.
pub fn validate_suite(suite: &EvalSuite) -> Result<(), EvalError> {
    if suite.schema_version != CURRENT_SCHEMA_VERSION {
        return Err(EvalError::Validation(format!(
            "unsupported schema_version {}; expected {CURRENT_SCHEMA_VERSION}",
            suite.schema_version
        )));
    }
    validate_id("suite", &suite.id)?;
    if suite.variants.is_empty() {
        return Err(EvalError::Validation(
            "suite must define at least one variant".into(),
        ));
    }
    if suite.run.repetitions == 0 {
        return Err(EvalError::Validation(
            "run.repetitions must be at least 1".into(),
        ));
    }
    if suite.cases.is_empty() {
        return Err(EvalError::Validation(
            "suite must define at least one case".into(),
        ));
    }
    let mut variant_ids = std::collections::BTreeSet::new();
    for variant in &suite.variants {
        validate_id("variant", &variant.id)?;
        if !variant_ids.insert(&variant.id) {
            return Err(EvalError::Validation(format!(
                "duplicate variant id {}",
                variant.id
            )));
        }
        if variant.executor == EvalExecutorKind::Command && variant.command.is_none() {
            return Err(EvalError::Validation(format!(
                "command variant {} must define command",
                variant.id
            )));
        }
    }
    let mut case_ids = std::collections::BTreeSet::new();
    for case in &suite.cases {
        validate_id("case", &case.id)?;
        if !case_ids.insert(&case.id) {
            return Err(EvalError::Validation(format!(
                "duplicate case id {}",
                case.id
            )));
        }
        if case.judges.is_empty() {
            return Err(EvalError::Validation(format!(
                "case {} must define at least one judge",
                case.id
            )));
        }
    }
    Ok(())
}

/// Run a suite and persist all artifacts.
///
/// # Errors
///
/// Returns an error when suite loading, execution, judging, or report writing fails.
pub fn run_suite(options: &EvalRunOptions) -> Result<EvalRunResult, EvalError> {
    let mut suite = load_suite(&options.suite_path)?;
    let suite_dir = options
        .suite_path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
        .canonicalize()?;
    suite.metadata.insert(
        "suite_dir".into(),
        serde_json::Value::String(suite_dir.display().to_string()),
    );
    let run_id = options
        .run_id
        .clone()
        .unwrap_or_else(|| default_run_id(&suite.id));
    let output_dir = options.output_root.join(&run_id);
    fs::create_dir_all(&output_dir)?;
    let manifest = EvalRunManifest {
        run_id,
        suite_id: suite.id.clone(),
        suite_name: suite.name.clone(),
        started_unix_ms: unix_ms(),
        output_dir: output_dir.clone(),
        suite_path: Some(options.suite_path.clone()),
        environment: capture_environment(),
    };
    write_json(output_dir.join("run.json"), &manifest)?;
    fs::write(
        output_dir.join("suite.snapshot.toml"),
        toml::to_string_pretty(&suite)?,
    )?;
    append_event(&output_dir, "run_started", &manifest)?;

    let mut variants = Vec::new();
    for variant in &suite.variants {
        let mut case_results = Vec::new();
        for case in &suite.cases {
            let result = run_case_variant(&suite, &suite_dir, &output_dir, case, variant)?;
            let failed = result.repetitions.iter().any(|rep| !rep.passed);
            case_results.push(result);
            if failed && suite.run.fail_fast {
                break;
            }
        }
        variants.push(aggregate_variant(&suite, variant, case_results));
    }
    let passed = variants.iter().all(|variant| {
        variant
            .cases
            .iter()
            .all(|case| case.repetitions.iter().all(|rep| rep.passed))
    });
    let result = EvalRunResult {
        manifest,
        variants,
        passed,
    };
    write_json(output_dir.join("summary.json"), &result)?;
    fs::write(
        output_dir.join("summary.md"),
        render_summary_markdown(&result),
    )?;
    append_event(&output_dir, "run_finished", &result)?;
    Ok(result)
}

fn run_case_variant(
    suite: &EvalSuite,
    suite_dir: &Path,
    output_dir: &Path,
    case: &EvalCase,
    variant: &EvalVariant,
) -> Result<EvalCaseRunResult, EvalError> {
    let mut repetitions = Vec::new();
    for repetition in 1..=suite.run.repetitions {
        let rep_dir = output_dir
            .join("cases")
            .join(&case.id)
            .join("variants")
            .join(&variant.id)
            .join("repetitions")
            .join(format!("{repetition:04}"));
        fs::create_dir_all(&rep_dir)?;
        let result = run_repetition(
            suite, suite_dir, output_dir, &rep_dir, case, variant, repetition,
        )?;
        write_json(rep_dir.join("result.json"), &result)?;
        write_json(rep_dir.join("metrics.json"), &result.measurements)?;
        append_observation(
            &rep_dir,
            &EvalObservation {
                unix_ms: unix_ms(),
                source: "repetition_finished".into(),
                payload: serde_json::to_value(&result)?,
            },
        )?;
        let failed = !result.passed;
        repetitions.push(result);
        if failed && suite.run.fail_fast {
            break;
        }
    }
    let pass_rate = pass_rate(repetitions.iter().map(|rep| rep.passed));
    Ok(EvalCaseRunResult {
        case_id: case.id.clone(),
        variant_id: variant.id.clone(),
        repetitions,
        pass_rate,
    })
}

fn run_repetition(
    suite: &EvalSuite,
    suite_dir: &Path,
    run_dir: &Path,
    rep_dir: &Path,
    case: &EvalCase,
    variant: &EvalVariant,
    repetition: u32,
) -> Result<EvalRepetitionResult, EvalError> {
    let workspace = prepare_workspace(suite, suite_dir, rep_dir, case)?;
    let start = Instant::now();
    let mut measurements = EvalMeasurementSet::new();
    let mut diagnostics = Vec::new();
    let mut artifacts = Vec::new();
    let execution = execute_variant(suite, case, variant, repetition, rep_dir, &workspace)?;
    measurements.extend(execution.measurements);
    diagnostics.extend(execution.diagnostics);
    artifacts.extend(execution.artifacts);
    let diff = git_diff(&workspace)?;
    let diff_path = rep_dir.join("diff.patch");
    fs::write(&diff_path, &diff)?;
    measurements.insert("diff_bytes".into(), diff.len() as f64);
    let stats = diff_stats(&diff);
    measurements.insert("diff_files_changed".into(), stats.files_changed as f64);
    measurements.insert("diff_additions".into(), stats.additions as f64);
    measurements.insert("diff_deletions".into(), stats.deletions as f64);
    artifacts.push(relative_artifact(run_dir, "diff", &diff_path));

    let mut judges = Vec::new();
    for judge in &case.judges {
        judges.push(run_judge(
            judge,
            suite_dir,
            rep_dir,
            &workspace,
            &diff,
            &measurements,
        )?);
    }
    for judge in &judges {
        for (key, value) in &judge.measurements {
            measurements.insert(key.clone(), *value);
        }
    }
    let required_judges_pass = judges
        .iter()
        .filter(|judge| judge.required)
        .all(|judge| judge.passed);
    let execution_passed = execution.exit_code.is_none_or(|code| code == 0);
    let wall_time_ms = start.elapsed().as_millis();
    measurements.insert("wall_time_ms".into(), wall_time_ms as f64);
    Ok(EvalRepetitionResult {
        case_id: case.id.clone(),
        variant_id: variant.id.clone(),
        repetition,
        passed: execution_passed && required_judges_pass,
        exit_code: execution.exit_code,
        wall_time_ms,
        measurements,
        judges,
        diagnostics,
        artifacts,
    })
}

#[derive(Debug, Default)]
struct ExecutionOutput {
    exit_code: Option<i32>,
    measurements: EvalMeasurementSet,
    diagnostics: Vec<EvalDiagnostic>,
    artifacts: Vec<EvalArtifactRef>,
}

fn execute_variant(
    suite: &EvalSuite,
    case: &EvalCase,
    variant: &EvalVariant,
    repetition: u32,
    rep_dir: &Path,
    workspace: &Path,
) -> Result<ExecutionOutput, EvalError> {
    match variant.executor {
        EvalExecutorKind::Command => {
            execute_command_variant(suite, case, variant, repetition, rep_dir, workspace)
        }
        EvalExecutorKind::Agent => {
            execute_agent_variant(suite, case, variant, repetition, rep_dir, workspace)
        }
        EvalExecutorKind::DirectTool => {
            execute_direct_tool_variant(suite, case, variant, repetition, rep_dir, workspace)
        }
        EvalExecutorKind::Replay => {
            execute_replay_variant(suite, case, variant, repetition, rep_dir, workspace)
        }
    }
}

fn execute_command_variant(
    suite: &EvalSuite,
    case: &EvalCase,
    variant: &EvalVariant,
    repetition: u32,
    rep_dir: &Path,
    workspace: &Path,
) -> Result<ExecutionOutput, EvalError> {
    let template = case
        .command
        .as_ref()
        .or(variant.command.as_ref())
        .ok_or_else(|| {
            EvalError::Validation(format!(
                "case {} and variant {} do not define a command",
                case.id, variant.id
            ))
        })?;
    let prompt = rendered_prompt(case, variant);
    let command = render_template(
        template, suite, case, variant, repetition, workspace, rep_dir, &prompt,
    );
    let stdout_path = rep_dir.join("stdout.log");
    let stderr_path = rep_dir.join("stderr.log");
    append_observation(
        rep_dir,
        &EvalObservation {
            unix_ms: unix_ms(),
            source: "command_started".into(),
            payload: serde_json::json!({"command": command}),
        },
    )?;
    let start = Instant::now();
    let output = shell_command(&command)
        .current_dir(workspace)
        .envs(&variant.env)
        .envs(&case.env)
        .env("BCODE_EVAL_RUN_ID", &suite.id)
        .env("BCODE_EVAL_SUITE_ID", &suite.id)
        .env("BCODE_EVAL_CASE_ID", &case.id)
        .env("BCODE_EVAL_VARIANT_ID", &variant.id)
        .env("BCODE_EVAL_REPETITION", repetition.to_string())
        .env("BCODE_EVAL_WORKSPACE", workspace)
        .env("BCODE_EVAL_PROMPT", &prompt)
        .env("BCODE_EVAL_ARTIFACT_DIR", rep_dir)
        .env(
            "BCODE_EVAL_SUITE_DIR",
            suite
                .metadata
                .get("suite_dir")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    fs::write(&stdout_path, &output.stdout)?;
    fs::write(&stderr_path, &output.stderr)?;
    let mut measurements = EvalMeasurementSet::new();
    measurements.insert(
        "command_wall_time_ms".into(),
        start.elapsed().as_millis() as f64,
    );
    measurements.insert("stdout_bytes".into(), output.stdout.len() as f64);
    measurements.insert("stderr_bytes".into(), output.stderr.len() as f64);
    if let Some(code) = output.status.code() {
        measurements.insert("command_exit_code".into(), f64::from(code));
    }
    let artifacts = vec![
        EvalArtifactRef {
            kind: "stdout".into(),
            path: PathBuf::from("stdout.log"),
        },
        EvalArtifactRef {
            kind: "stderr".into(),
            path: PathBuf::from("stderr.log"),
        },
    ];
    append_observation(
        rep_dir,
        &EvalObservation {
            unix_ms: unix_ms(),
            source: "command_finished".into(),
            payload: serde_json::json!({"exit_code": output.status.code()}),
        },
    )?;
    Ok(ExecutionOutput {
        exit_code: output.status.code(),
        measurements,
        diagnostics: Vec::new(),
        artifacts,
    })
}

#[derive(Debug)]
struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }
}

fn prepare_agent_policy_overlay(
    variant: &EvalVariant,
    rep_dir: &Path,
) -> Result<Option<PathBuf>, EvalError> {
    if variant.allowed_tools.is_empty() {
        return Ok(None);
    }
    let agent_id = variant
        .metadata
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("build");
    let path = rep_dir.join("permissions.toml");
    let allowed = variant
        .allowed_tools
        .iter()
        .map(|tool| format!("{tool} = true"))
        .collect::<Vec<_>>()
        .join("\n");
    let state = format!(
        "[agent.{agent_id}.tools]\n{allowed}\n\n[agent.{agent_id}.permission.command]\n\"*\" = \"ask\"\n\n[agent.{agent_id}.permission.read]\n\"*\" = \"allow\"\n\n[agent.{agent_id}.permission.write]\n\"*\" = \"ask\"\n\n[agent.{agent_id}.permission.edit]\n\"*\" = \"ask\"\n"
    );
    fs::write(&path, state)?;
    Ok(Some(path))
}

async fn validate_agent_policy_overlay(
    client: &bcode_client::BcodeClient,
    variant: &EvalVariant,
) -> Result<Vec<EvalDiagnostic>, EvalError> {
    if variant.allowed_tools.is_empty() {
        return Ok(Vec::new());
    }
    let status = client
        .agent_policy_status()
        .await
        .map_err(|error| EvalError::Client(error.to_string()))?;
    let agent_id = variant
        .metadata
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("build");
    let enabled = if agent_id == "plan" {
        &status.plan_enabled_tools
    } else {
        &status.build_enabled_tools
    };
    let missing = variant
        .allowed_tools
        .iter()
        .filter(|tool| !enabled.contains(tool))
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(vec![EvalDiagnostic {
            severity: EvalDiagnosticSeverity::Warning,
            message: format!(
                "agent policy overlay may not be active for variant {}; missing enabled tools {:?}; policy source: {}",
                variant.id, missing, status.source
            ),
        }])
    }
}

fn execute_direct_tool_variant(
    suite: &EvalSuite,
    case: &EvalCase,
    variant: &EvalVariant,
    _repetition: u32,
    rep_dir: &Path,
    workspace: &Path,
) -> Result<ExecutionOutput, EvalError> {
    let config = case
        .direct_tool
        .as_ref()
        .or(variant.direct_tool.as_ref())
        .ok_or_else(|| {
            EvalError::Validation(format!(
                "direct_tool executor requires direct_tool config for case {} or variant {}",
                case.id, variant.id
            ))
        })?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    runtime.block_on(async {
        let client = bcode_client::BcodeClient::default_endpoint();
        client
            .ensure_daemon_available()
            .await
            .map_err(|error| EvalError::Client(error.to_string()))?;
        let request = bcode_tool::ToolInvocationRequest {
            tool_call_id: format!("eval-{}-{}-{}", suite.id, case.id, variant.id),
            name: config.tool_name.clone(),
            arguments: config.arguments.clone(),
            cwd: Some(workspace.to_path_buf()),
            artifact_dir: Some(rep_dir.to_path_buf()),
            cancellation_path: None,
        };
        let payload = serde_json::to_vec(&request)?;
        let start = Instant::now();
        let response = if let Some(plugin_id) = &config.plugin_id {
            client
                .invoke_plugin_service(
                    plugin_id.clone(),
                    bcode_tool::TOOL_SERVICE_INTERFACE_ID.to_string(),
                    bcode_tool::OP_INVOKE_TOOL.to_string(),
                    payload,
                )
                .await
        } else {
            client
                .call_plugin_service(
                    bcode_tool::TOOL_SERVICE_INTERFACE_ID.to_string(),
                    bcode_tool::OP_INVOKE_TOOL.to_string(),
                    payload,
                )
                .await
        }
        .map_err(|error| EvalError::Client(error.to_string()))?;
        let stdout_path = rep_dir.join("direct-tool-response.json");
        let mut measurements = EvalMeasurementSet::new();
        measurements.insert(
            "direct_tool_wall_time_ms".into(),
            start.elapsed().as_millis() as f64,
        );
        measurements.insert(
            "direct_tool_response_bytes".into(),
            response.payload.len() as f64,
        );
        if let Some(error) = response.error {
            fs::write(&stdout_path, serde_json::to_string_pretty(&error)?)?;
            return Ok(ExecutionOutput {
                exit_code: Some(1),
                measurements,
                diagnostics: vec![EvalDiagnostic {
                    severity: EvalDiagnosticSeverity::Error,
                    message: format!("tool service error {}: {}", error.code, error.message),
                }],
                artifacts: vec![EvalArtifactRef {
                    kind: "direct_tool_response".into(),
                    path: PathBuf::from("direct-tool-response.json"),
                }],
            });
        }
        fs::write(&stdout_path, &response.payload)?;
        let tool_response =
            serde_json::from_slice::<bcode_tool::ToolInvocationResponse>(&response.payload)?;
        measurements.insert(
            "tool_output_bytes".into(),
            tool_response.output.len() as f64,
        );
        let is_error = tool_response.is_error;
        let diagnostics = if is_error {
            vec![EvalDiagnostic {
                severity: EvalDiagnosticSeverity::Error,
                message: tool_response.output,
            }]
        } else {
            Vec::new()
        };
        Ok(ExecutionOutput {
            exit_code: Some(i32::from(is_error)),
            measurements,
            diagnostics,
            artifacts: vec![EvalArtifactRef {
                kind: "direct_tool_response".into(),
                path: PathBuf::from("direct-tool-response.json"),
            }],
        })
    })
}

fn execute_replay_variant(
    suite: &EvalSuite,
    case: &EvalCase,
    variant: &EvalVariant,
    _repetition: u32,
    rep_dir: &Path,
    _workspace: &Path,
) -> Result<ExecutionOutput, EvalError> {
    let config = case
        .replay
        .as_ref()
        .or(variant.replay.as_ref())
        .ok_or_else(|| {
            EvalError::Validation(format!(
                "replay executor requires replay config for case {} or variant {}",
                case.id, variant.id
            ))
        })?;
    let suite_dir = suite
        .metadata
        .get("suite_dir")
        .and_then(serde_json::Value::as_str)
        .map_or_else(PathBuf::new, PathBuf::from);
    let transcript_path = if config.transcript.is_absolute() {
        config.transcript.clone()
    } else {
        suite_dir.join(&config.transcript)
    };
    let text = fs::read_to_string(&transcript_path)?;
    let mut events = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        events.push(serde_json::from_str::<bcode_session_models::SessionEvent>(
            line,
        )?);
    }
    let copied = rep_dir.join("transcript.jsonl");
    fs::write(&copied, text)?;
    let telemetry = session_telemetry(&events);
    Ok(ExecutionOutput {
        exit_code: Some(i32::from(telemetry.timed_out)),
        measurements: telemetry.measurements,
        diagnostics: telemetry.diagnostics,
        artifacts: vec![EvalArtifactRef {
            kind: "replay_transcript".into(),
            path: PathBuf::from("transcript.jsonl"),
        }],
    })
}

fn execute_agent_variant(
    suite: &EvalSuite,
    case: &EvalCase,
    variant: &EvalVariant,
    repetition: u32,
    rep_dir: &Path,
    workspace: &Path,
) -> Result<ExecutionOutput, EvalError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    runtime.block_on(execute_agent_variant_async(
        suite, case, variant, repetition, rep_dir, workspace,
    ))
}

async fn execute_agent_variant_async(
    suite: &EvalSuite,
    case: &EvalCase,
    variant: &EvalVariant,
    repetition: u32,
    rep_dir: &Path,
    workspace: &Path,
) -> Result<ExecutionOutput, EvalError> {
    let prompt = rendered_prompt(case, variant);
    let policy_overlay = prepare_agent_policy_overlay(variant, rep_dir)?;
    let _policy_env = policy_overlay
        .as_ref()
        .map(|path| EnvVarGuard::set("BCODE_PERMISSIONS_STATE", path));
    let _config_guard = variant.profile.as_deref().map(|profile| {
        bcode_config::push_process_config_overrides(
            bcode_config::ConfigLoadOverrides::from_env_with_cli(
                None,
                Some(bcode_config::model_profile_override_toml(profile)),
            ),
        )
    });
    let client = bcode_client::BcodeClient::default_endpoint();
    client
        .ensure_daemon_available()
        .await
        .map_err(|error| EvalError::Client(error.to_string()))?;
    let mut policy_diagnostics = validate_agent_policy_overlay(&client, variant).await?;
    let session = client
        .create_session_in_working_directory(
            Some(format!("eval:{}:{}:{}", suite.id, case.id, variant.id)),
            workspace.to_path_buf(),
        )
        .await
        .map_err(|error| EvalError::Client(error.to_string()))?;
    if let Some(agent_id) = variant
        .metadata
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
    {
        client
            .set_session_agent(session.id, agent_id.to_string())
            .await
            .map_err(|error| EvalError::Client(error.to_string()))?;
    }
    if let Some(model) = &variant.model {
        client
            .set_session_model(session.id, None, model.clone())
            .await
            .map_err(|error| EvalError::Client(error.to_string()))?;
    }
    let start_sequence = latest_session_sequence(&client, session.id).await?;
    append_observation(
        rep_dir,
        &EvalObservation {
            unix_ms: unix_ms(),
            source: "agent_session_started".into(),
            payload: serde_json::json!({
                "session_id": session.id,
                "start_sequence": start_sequence,
                "repetition": repetition,
                "allowed_tools": variant.allowed_tools,
            }),
        },
    )?;
    let start = Instant::now();
    client
        .send_user_message(session.id, prompt, bcode_ipc::PromptPlacement::FollowUp)
        .await
        .map_err(|error| EvalError::Client(error.to_string()))?;
    let timeout = std::time::Duration::from_millis(case.timeout_ms.unwrap_or(suite.run.timeout_ms));
    let events = wait_for_agent_turn(&client, session.id, start_sequence, timeout).await?;
    let transcript_path = rep_dir.join("transcript.jsonl");
    let tool_calls_path = rep_dir.join("tool-calls.jsonl");
    write_session_events_jsonl(&transcript_path, &events)?;
    write_tool_calls_jsonl(&tool_calls_path, &events)?;
    let telemetry = session_telemetry(&events);
    let mut measurements = telemetry.measurements;
    measurements.insert(
        "agent_wall_time_ms".into(),
        start.elapsed().as_millis() as f64,
    );
    let mut diagnostics = telemetry.diagnostics;
    diagnostics.append(&mut policy_diagnostics);
    if telemetry.timed_out {
        diagnostics.push(EvalDiagnostic {
            severity: EvalDiagnosticSeverity::Error,
            message: "agent turn timed out before ModelTurnFinished".into(),
        });
    }
    append_observation(
        rep_dir,
        &EvalObservation {
            unix_ms: unix_ms(),
            source: "agent_session_finished".into(),
            payload: serde_json::json!({
                "session_id": session.id,
                "event_count": events.len(),
                "timed_out": telemetry.timed_out,
            }),
        },
    )?;
    Ok(ExecutionOutput {
        exit_code: Some(if telemetry.timed_out { 124 } else { 0 }),
        measurements,
        diagnostics,
        artifacts: vec![
            EvalArtifactRef {
                kind: "transcript".into(),
                path: PathBuf::from("transcript.jsonl"),
            },
            EvalArtifactRef {
                kind: "tool_calls".into(),
                path: PathBuf::from("tool-calls.jsonl"),
            },
        ],
    })
}

async fn latest_session_sequence(
    client: &bcode_client::BcodeClient,
    session_id: bcode_session_models::SessionId,
) -> Result<u64, EvalError> {
    let page = client
        .session_history_page(
            session_id,
            bcode_session_models::SessionHistoryQuery {
                cursor: None,
                limit: 1,
                direction: bcode_session_models::SessionHistoryDirection::Backward,
            },
        )
        .await
        .map_err(|error| EvalError::Client(error.to_string()))?;
    Ok(page.events.first().map_or(0, |event| event.sequence))
}

async fn wait_for_agent_turn(
    client: &bcode_client::BcodeClient,
    session_id: bcode_session_models::SessionId,
    start_sequence: u64,
    timeout: std::time::Duration,
) -> Result<Vec<bcode_session_models::SessionEvent>, EvalError> {
    let start = Instant::now();
    let mut cursor = Some(bcode_session_models::SessionHistoryCursor {
        sequence: start_sequence,
    });
    let mut events = Vec::new();
    let mut saw_turn_started = false;
    loop {
        let page = client
            .session_history_page(
                session_id,
                bcode_session_models::SessionHistoryQuery {
                    cursor,
                    limit: 500,
                    direction: bcode_session_models::SessionHistoryDirection::Forward,
                },
            )
            .await
            .map_err(|error| EvalError::Client(error.to_string()))?;
        for event in page.events {
            cursor = Some(bcode_session_models::SessionHistoryCursor {
                sequence: event.sequence,
            });
            match &event.kind {
                bcode_session_models::SessionEventKind::ModelTurnStarted { .. } => {
                    saw_turn_started = true;
                }
                bcode_session_models::SessionEventKind::ModelTurnFinished { .. }
                    if saw_turn_started =>
                {
                    events.push(event);
                    return Ok(events);
                }
                _ => {}
            }
            events.push(event);
        }
        if start.elapsed() >= timeout {
            return Ok(events);
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn write_session_events_jsonl(
    path: &Path,
    events: &[bcode_session_models::SessionEvent],
) -> Result<(), EvalError> {
    let mut file = File::create(path)?;
    for event in events {
        writeln!(file, "{}", serde_json::to_string(event)?)?;
    }
    Ok(())
}

fn write_tool_calls_jsonl(
    path: &Path,
    events: &[bcode_session_models::SessionEvent],
) -> Result<(), EvalError> {
    let mut file = File::create(path)?;
    for event in events {
        if matches!(
            event.kind,
            bcode_session_models::SessionEventKind::ToolCallRequested { .. }
                | bcode_session_models::SessionEventKind::ToolCallFinished { .. }
                | bcode_session_models::SessionEventKind::PermissionRequested { .. }
                | bcode_session_models::SessionEventKind::PermissionResolved { .. }
        ) {
            writeln!(file, "{}", serde_json::to_string(event)?)?;
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct SessionTelemetry {
    measurements: EvalMeasurementSet,
    diagnostics: Vec<EvalDiagnostic>,
    timed_out: bool,
}

fn session_telemetry(events: &[bcode_session_models::SessionEvent]) -> SessionTelemetry {
    let mut telemetry = SessionTelemetry::default();
    let mut tool_counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut tool_errors = 0_u32;
    let mut permissions = 0_u32;
    let mut input_tokens = 0_u32;
    let mut output_tokens = 0_u32;
    let mut total_tokens = 0_u32;
    let mut cached_input_tokens = 0_u32;
    let mut cache_write_input_tokens = 0_u32;
    let mut reasoning_tokens = 0_u32;
    let mut turn_finished = false;
    for event in events {
        match &event.kind {
            bcode_session_models::SessionEventKind::ToolCallRequested { tool_name, .. } => {
                *tool_counts.entry(tool_name.clone()).or_default() += 1;
            }
            bcode_session_models::SessionEventKind::ToolCallFinished { is_error: true, .. } => {
                tool_errors += 1;
            }
            bcode_session_models::SessionEventKind::PermissionRequested { .. } => {
                permissions += 1;
            }
            bcode_session_models::SessionEventKind::ModelUsage { usage, .. } => {
                input_tokens = input_tokens.saturating_add(usage.input_tokens.unwrap_or_default());
                output_tokens =
                    output_tokens.saturating_add(usage.output_tokens.unwrap_or_default());
                total_tokens =
                    total_tokens.saturating_add(usage.metered_total_tokens().unwrap_or_default());
                cached_input_tokens = cached_input_tokens
                    .saturating_add(usage.cached_input_tokens.unwrap_or_default());
                cache_write_input_tokens = cache_write_input_tokens
                    .saturating_add(usage.cache_write_input_tokens.unwrap_or_default());
                reasoning_tokens =
                    reasoning_tokens.saturating_add(usage.reasoning_tokens.unwrap_or_default());
            }
            bcode_session_models::SessionEventKind::ModelTurnFinished {
                outcome, message, ..
            } => {
                turn_finished = true;
                if !matches!(outcome, bcode_session_models::ModelTurnOutcome::Completed) {
                    telemetry.diagnostics.push(EvalDiagnostic {
                        severity: EvalDiagnosticSeverity::Error,
                        message: format!(
                            "model turn finished with outcome {outcome:?}: {}",
                            message.clone().unwrap_or_default()
                        ),
                    });
                }
            }
            _ => {}
        }
    }
    telemetry.timed_out = !turn_finished;
    telemetry
        .measurements
        .insert("session_event_count".into(), events.len() as f64);
    telemetry.measurements.insert(
        "tool_call_count".into(),
        tool_counts.values().copied().sum::<u32>().into(),
    );
    for (tool, count) in tool_counts {
        telemetry
            .measurements
            .insert(format!("tool_call_count.{tool}"), f64::from(count));
    }
    telemetry
        .measurements
        .insert("tool_error_count".into(), f64::from(tool_errors));
    telemetry
        .measurements
        .insert("permission_prompt_count".into(), f64::from(permissions));
    telemetry
        .measurements
        .insert("input_tokens".into(), f64::from(input_tokens));
    telemetry
        .measurements
        .insert("output_tokens".into(), f64::from(output_tokens));
    telemetry
        .measurements
        .insert("total_tokens".into(), f64::from(total_tokens));
    telemetry
        .measurements
        .insert("cached_input_tokens".into(), f64::from(cached_input_tokens));
    telemetry.measurements.insert(
        "cache_write_input_tokens".into(),
        f64::from(cache_write_input_tokens),
    );
    telemetry
        .measurements
        .insert("reasoning_tokens".into(), f64::from(reasoning_tokens));
    telemetry
}

fn run_judge(
    judge: &EvalJudgeConfig,
    suite_dir: &Path,
    rep_dir: &Path,
    workspace: &Path,
    diff: &str,
    measurements: &EvalMeasurementSet,
) -> Result<EvalJudgeResult, EvalError> {
    let start = Instant::now();
    let mut judge_measurements = EvalMeasurementSet::new();
    let result = match judge {
        EvalJudgeConfig::ExactDiff {
            expected_patch,
            required,
        } => {
            let expected = fs::read_to_string(suite_dir.join(expected_patch))?;
            let passed =
                normalize_patch_for_compare(&expected) == normalize_patch_for_compare(diff);
            EvalJudgeResult {
                kind: "exact_diff".into(),
                passed,
                required: *required,
                score: Some(if passed { 1.0 } else { 0.0 }),
                measurements: EvalMeasurementSet::new(),
                diagnostics: diagnostic_if_failed(passed, "git diff did not match expected patch"),
                artifacts: Vec::new(),
            }
        }
        EvalJudgeConfig::Command {
            command,
            expected_exit_code,
            required,
        } => {
            let output = shell_command(command)
                .current_dir(workspace)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()?;
            let code = output.status.code().unwrap_or(-1);
            let stdout_path = rep_dir.join(format!(
                "judge-command-{}.stdout.log",
                start.elapsed().as_nanos()
            ));
            let stderr_path = rep_dir.join(format!(
                "judge-command-{}.stderr.log",
                start.elapsed().as_nanos()
            ));
            fs::write(&stdout_path, &output.stdout)?;
            fs::write(&stderr_path, &output.stderr)?;
            judge_measurements.insert("judge_command_exit_code".into(), f64::from(code));
            let passed = code == *expected_exit_code;
            EvalJudgeResult {
                kind: "command".into(),
                passed,
                required: *required,
                score: Some(if passed { 1.0 } else { 0.0 }),
                measurements: judge_measurements,
                diagnostics: diagnostic_if_failed(
                    passed,
                    &format!("command judge exited {code}, expected {expected_exit_code}"),
                ),
                artifacts: vec![
                    EvalArtifactRef {
                        kind: "judge_stdout".into(),
                        path: stdout_path,
                    },
                    EvalArtifactRef {
                        kind: "judge_stderr".into(),
                        path: stderr_path,
                    },
                ],
            }
        }
        EvalJudgeConfig::FileSnapshot {
            path,
            expected,
            required,
        } => {
            let actual = fs::read_to_string(workspace.join(path))?;
            let expected = fs::read_to_string(suite_dir.join(expected))?;
            let passed = normalize_newlines(&actual) == normalize_newlines(&expected);
            EvalJudgeResult {
                kind: "file_snapshot".into(),
                passed,
                required: *required,
                score: Some(if passed { 1.0 } else { 0.0 }),
                measurements: EvalMeasurementSet::new(),
                diagnostics: diagnostic_if_failed(
                    passed,
                    &format!("snapshot mismatch for {}", path.display()),
                ),
                artifacts: Vec::new(),
            }
        }
        EvalJudgeConfig::Regex {
            target,
            pattern,
            should_match,
            required,
        } => {
            let haystack = regex_target(*target, rep_dir, diff)?;
            let matched = Regex::new(pattern)?.is_match(&haystack);
            let passed = matched == *should_match;
            EvalJudgeResult {
                kind: "regex".into(),
                passed,
                required: *required,
                score: Some(if passed { 1.0 } else { 0.0 }),
                measurements: EvalMeasurementSet::new(),
                diagnostics: diagnostic_if_failed(passed, "regex judge expectation was not met"),
                artifacts: Vec::new(),
            }
        }
        EvalJudgeConfig::MetricThreshold {
            metric,
            min,
            max,
            required,
        } => {
            let value = measurements.get(metric).copied();
            let passed = value.is_some_and(|value| {
                min.is_none_or(|min| value >= min) && max.is_none_or(|max| value <= max)
            });
            EvalJudgeResult {
                kind: "metric_threshold".into(),
                passed,
                required: *required,
                score: Some(if passed { 1.0 } else { 0.0 }),
                measurements: EvalMeasurementSet::new(),
                diagnostics: diagnostic_if_failed(
                    passed,
                    &format!("metric {metric} was outside threshold"),
                ),
                artifacts: Vec::new(),
            }
        }
    };
    let mut result = result;
    result.measurements.insert(
        format!("judge_{}_wall_time_ms", result.kind),
        start.elapsed().as_millis() as f64,
    );
    Ok(result)
}

/// Create a useful eval suite skeleton on disk.
///
/// # Errors
///
/// Returns an error when the destination already contains conflicting files or writing fails.
pub fn init_suite(directory: impl AsRef<Path>, id: &str, name: &str) -> Result<(), EvalError> {
    validate_id("suite", id)?;
    let directory = directory.as_ref();
    fs::create_dir_all(directory.join("cases/example/input"))?;
    let suite_path = directory.join("suite.toml");
    if suite_path.exists() {
        return Err(EvalError::Validation(format!(
            "suite already exists: {}",
            suite_path.display()
        )));
    }
    let suite = format!(
        r#"schema_version = 1
id = "{id}"
name = "{name}"
description = "TODO: describe what this suite evaluates."

[run]
repetitions = 3
timeout_ms = 120000
isolation = "temp_copy"
randomize_case_order = false
fail_fast = false

[score]
correctness = 0.70
efficiency = 0.15
speed = 0.10
cost = 0.0
stability = 0.05
correctness_required = true

[[score.metrics]]
metric = "total_tokens"
direction = "lower_is_better"
weight = 2
target = 50000

[[score.metrics]]
metric = "wall_time_ms"
direction = "lower_is_better"
weight = 1
target = 60000

[regression]
min_pass_rate = 0.95
max_token_increase_percent = 15.0
max_latency_increase_percent = 25.0
fail_on_new_failure = true

[[variants]]
id = "baseline"
name = "Baseline command"
executor = "command"
command = "echo TODO: implement variant command using $BCODE_EVAL_PROMPT"

[[cases]]
id = "example"
name = "Example case"
fixture = "cases/example/input"
prompt = "TODO: describe the task."
timeout_ms = 60000

[[cases.judges]]
type = "regex"
target = "stdout"
pattern = "TODO"
should_match = true
required = true
"#
    );
    fs::write(suite_path, suite)?;
    fs::write(
        directory.join("cases/example/input/README.md"),
        "# Example eval fixture\n\nReplace this with a real fixture.\n",
    )?;
    Ok(())
}

/// Bless an expected patch by copying and normalizing a run diff.
///
/// # Errors
///
/// Returns an error when reading or writing fails.
pub fn bless_expected_patch(
    diff_path: impl AsRef<Path>,
    expected_patch_path: impl AsRef<Path>,
) -> Result<(), EvalError> {
    let diff = fs::read_to_string(diff_path)?;
    if let Some(parent) = expected_patch_path.as_ref().parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(expected_patch_path, normalize_patch_for_bless(&diff))?;
    Ok(())
}

fn normalize_patch_for_bless(value: &str) -> String {
    let mut normalized = normalize_newlines(value)
        .lines()
        .filter(|line| !line.starts_with("index "))
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    normalized.push('\n');
    normalized
}

/// Discover suite manifests below a root directory.
///
/// # Errors
///
/// Returns an error when directory traversal fails.
pub fn list_suites(root: impl AsRef<Path>) -> Result<Vec<PathBuf>, EvalError> {
    let mut suites = Vec::new();
    collect_suites(root.as_ref(), &mut suites)?;
    suites.sort();
    Ok(suites)
}

fn collect_suites(path: &Path, suites: &mut Vec<PathBuf>) -> Result<(), EvalError> {
    if !path.exists() {
        return Ok(());
    }
    if path.join("suite.toml").exists() {
        suites.push(path.join("suite.toml"));
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_suites(&entry.path(), suites)?;
        }
    }
    Ok(())
}

/// Add a case fixture skeleton to a suite directory.
///
/// # Errors
///
/// Returns an error when the case already exists or writing fails.
pub fn add_case(
    suite_dir: impl AsRef<Path>,
    case_id: &str,
    case_name: &str,
    from_dir: Option<&Path>,
) -> Result<(), EvalError> {
    validate_id("case", case_id)?;
    let suite_dir = suite_dir.as_ref();
    let case_dir = suite_dir.join("cases").join(case_id);
    if case_dir.exists() {
        return Err(EvalError::Validation(format!(
            "case already exists: {}",
            case_dir.display()
        )));
    }
    let input = case_dir.join("input");
    if let Some(from_dir) = from_dir {
        copy_dir_all(from_dir, &input)?;
    } else {
        fs::create_dir_all(&input)?;
        fs::write(input.join("README.md"), "# Eval fixture\n")?;
    }
    fs::write(case_dir.join("expected.patch"), "")?;
    let suite_path = suite_dir.join("suite.toml");
    let mut suite = fs::read_to_string(&suite_path)?;
    suite.push_str(&format!(
        "\n[[cases]]\nid = \"{case_id}\"\nname = \"{case_name}\"\nfixture = \"cases/{case_id}/input\"\nprompt = \"TODO: describe the task.\"\ntimeout_ms = 60000\n\n[[cases.judges]]\ntype = \"exact_diff\"\nexpected_patch = \"cases/{case_id}/expected.patch\"\nrequired = true\n"
    ));
    fs::write(suite_path, suite)?;
    Ok(())
}

/// Capture a case expected patch from a run diff.
///
/// # Errors
///
/// Returns an error when reading or writing fails.
pub fn capture_expected_patch(
    diff_path: impl AsRef<Path>,
    suite_dir: impl AsRef<Path>,
    case_id: &str,
) -> Result<(), EvalError> {
    bless_expected_patch(
        diff_path,
        suite_dir
            .as_ref()
            .join("cases")
            .join(case_id)
            .join("expected.patch"),
    )
}

/// Load an eval run result from `summary.json` or a run directory.
///
/// # Errors
///
/// Returns an error when the summary cannot be read or parsed.
pub fn load_run_result(path: impl AsRef<Path>) -> Result<EvalRunResult, EvalError> {
    let path = path.as_ref();
    let summary = if path.is_dir() {
        path.join("summary.json")
    } else {
        path.to_path_buf()
    };
    Ok(serde_json::from_str(&fs::read_to_string(summary)?)?)
}

/// Compare one or more run results.
#[must_use]
pub fn compare_runs(runs: &[EvalRunResult]) -> EvalComparisonReport {
    let mut by_variant: BTreeMap<String, EvalComparisonVariant> = BTreeMap::new();
    for run in runs {
        for variant in &run.variants {
            by_variant.insert(
                variant.variant_id.clone(),
                EvalComparisonVariant {
                    variant_id: variant.variant_id.clone(),
                    pass_rate: variant.pass_rate,
                    overall_score: variant.score.overall,
                    measurements: variant.measurements.clone(),
                },
            );
        }
    }
    let variants = by_variant.into_values().collect::<Vec<_>>();
    let winner = variants
        .iter()
        .max_by(|left, right| left.overall_score.total_cmp(&right.overall_score))
        .map(|variant| variant.variant_id.clone());
    EvalComparisonReport {
        run_ids: runs.iter().map(|run| run.manifest.run_id.clone()).collect(),
        winner,
        variants,
        diagnostics: Vec::new(),
    }
}

/// Render a comparison report as Markdown.
#[must_use]
pub fn render_comparison_markdown(report: &EvalComparisonReport) -> String {
    let mut out = String::from("# Eval Comparison\n\n");
    if let Some(winner) = &report.winner {
        out.push_str(&format!("Winner: `{winner}`\n\n"));
    }
    out.push_str("| Variant | Pass rate | Score | Key metrics |\n|---|---:|---:|---|\n");
    for variant in &report.variants {
        let metrics = variant
            .measurements
            .iter()
            .filter(|(key, _)| {
                matches!(
                    key.as_str(),
                    "total_tokens"
                        | "wall_time_ms"
                        | "tool_call_count"
                        | "diff_bytes"
                        | "tool_error_count"
                )
            })
            .map(|(key, value)| format!("{key}={value:.2}"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "| {} | {:.2}% | {:.3} | {} |\n",
            variant.variant_id,
            variant.pass_rate * 100.0,
            variant.overall_score,
            metrics
        ));
    }
    if !report.diagnostics.is_empty() {
        out.push_str("\n## Diagnostics\n\n");
        for diagnostic in &report.diagnostics {
            out.push_str(&format!(
                "* {:?}: {}\n",
                diagnostic.severity, diagnostic.message
            ));
        }
    }
    out
}

/// Render full run comparison Markdown with per-case details.
#[must_use]
pub fn render_runs_comparison_markdown(runs: &[EvalRunResult]) -> String {
    let report = compare_runs(runs);
    let mut out = render_comparison_markdown(&report);
    out.push_str("\n## Case Matrix\n\n");
    out.push_str("| Run | Variant | Case | Pass rate | Avg wall ms | Wall stddev | Avg tokens | Tool calls | Flaky |\n|---|---|---|---:|---:|---:|---:|---:|---|\n");
    for run in runs {
        for variant in &run.variants {
            for case in &variant.cases {
                let reps = case.repetitions.iter().collect::<Vec<_>>();
                let metrics = average_measurements(&reps);
                let wall_values = reps
                    .iter()
                    .filter_map(|rep| rep.measurements.get("wall_time_ms").copied())
                    .collect::<Vec<_>>();
                let wall_stddev = stddev(&wall_values);
                let flaky = case.pass_rate > 0.0 && case.pass_rate < 1.0;
                out.push_str(&format!(
                    "| {} | {} | {} | {:.2}% | {:.2} | {:.2} | {:.2} | {:.2} | {} |\n",
                    run.manifest.run_id,
                    variant.variant_id,
                    case.case_id,
                    case.pass_rate * 100.0,
                    metrics.get("wall_time_ms").copied().unwrap_or_default(),
                    wall_stddev,
                    metrics.get("total_tokens").copied().unwrap_or_default(),
                    metrics.get("tool_call_count").copied().unwrap_or_default(),
                    if flaky { "yes" } else { "no" },
                ));
            }
        }
    }
    out.push_str("\n## Artifact Roots\n\n");
    for run in runs {
        out.push_str(&format!(
            "* `{}`: `{}`\n",
            run.manifest.run_id,
            run.manifest.output_dir.display()
        ));
    }
    out
}

fn stddev(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|value| {
            let delta = value - mean;
            delta * delta
        })
        .sum::<f64>()
        / values.len() as f64;
    variance.sqrt()
}

/// Write a comparison report.
///
/// # Errors
///
/// Returns an error when writing fails.
pub fn write_comparison_report(
    report: &EvalComparisonReport,
    output: impl AsRef<Path>,
) -> Result<(), EvalError> {
    write_json(output, report)
}

/// Create or update a suite baseline sidecar.
///
/// # Errors
///
/// Returns an error when reading or writing fails.
pub fn set_baseline(
    output_root: impl AsRef<Path>,
    run_path: impl AsRef<Path>,
) -> Result<EvalBaseline, EvalError> {
    let run = load_run_result(&run_path)?;
    let baseline = EvalBaseline {
        suite_id: run.manifest.suite_id,
        run_id: run.manifest.run_id,
        run_path: run_path.as_ref().to_path_buf(),
    };
    let dir = output_root.as_ref().join("baselines");
    fs::create_dir_all(&dir)?;
    write_json(dir.join(format!("{}.json", baseline.suite_id)), &baseline)?;
    Ok(baseline)
}

/// Report regressions for a candidate run against a baseline file/run.
///
/// # Errors
///
/// Returns an error when inputs cannot be read.
pub fn regression_report(
    baseline_path: impl AsRef<Path>,
    candidate_path: impl AsRef<Path>,
) -> Result<EvalRegressionReport, EvalError> {
    let baseline_text = fs::read_to_string(&baseline_path)?;
    let baseline_run = match serde_json::from_str::<EvalBaseline>(&baseline_text) {
        Ok(baseline) => load_run_result(baseline.run_path)?,
        Err(_) => serde_json::from_str::<EvalRunResult>(&baseline_text)?,
    };
    let candidate = load_run_result(candidate_path)?;
    let regression_config = load_suite_snapshot_from_run(&candidate)
        .map(|suite| suite.regression)
        .unwrap_or_default();
    let mut diagnostics = Vec::new();
    for candidate_variant in &candidate.variants {
        if let Some(base_variant) = baseline_run
            .variants
            .iter()
            .find(|variant| variant.variant_id == candidate_variant.variant_id)
        {
            if let Some(min_pass_rate) = regression_config.min_pass_rate
                && candidate_variant.pass_rate < min_pass_rate
            {
                diagnostics.push(EvalDiagnostic {
                    severity: EvalDiagnosticSeverity::Error,
                    message: format!(
                        "variant {} pass rate {:.2}% is below threshold {:.2}%",
                        candidate_variant.variant_id,
                        candidate_variant.pass_rate * 100.0,
                        min_pass_rate * 100.0
                    ),
                });
            }
            if candidate_variant.pass_rate < base_variant.pass_rate
                && regression_config.fail_on_new_failure
            {
                diagnostics.push(EvalDiagnostic {
                    severity: EvalDiagnosticSeverity::Error,
                    message: format!(
                        "variant {} pass rate regressed from {:.2}% to {:.2}%",
                        candidate_variant.variant_id,
                        base_variant.pass_rate * 100.0,
                        candidate_variant.pass_rate * 100.0
                    ),
                });
            }
            if candidate_variant.score.overall < base_variant.score.overall {
                diagnostics.push(EvalDiagnostic {
                    severity: EvalDiagnosticSeverity::Warning,
                    message: format!(
                        "variant {} score decreased from {:.3} to {:.3}",
                        candidate_variant.variant_id,
                        base_variant.score.overall,
                        candidate_variant.score.overall
                    ),
                });
            }
            check_metric_percent_regression(
                &mut diagnostics,
                &candidate_variant.variant_id,
                "total_tokens",
                base_variant.measurements.get("total_tokens").copied(),
                candidate_variant.measurements.get("total_tokens").copied(),
                regression_config.max_token_increase_percent,
            );
            check_metric_percent_regression(
                &mut diagnostics,
                &candidate_variant.variant_id,
                "wall_time_ms",
                base_variant.measurements.get("wall_time_ms").copied(),
                candidate_variant.measurements.get("wall_time_ms").copied(),
                regression_config.max_latency_increase_percent,
            );
        }
    }
    Ok(EvalRegressionReport {
        baseline_run_id: baseline_run.manifest.run_id,
        candidate_run_id: candidate.manifest.run_id,
        regressed: diagnostics
            .iter()
            .any(|diag| diag.severity == EvalDiagnosticSeverity::Error),
        diagnostics,
    })
}

fn load_suite_snapshot_from_run(run: &EvalRunResult) -> Result<EvalSuite, EvalError> {
    let suite_path = run.manifest.output_dir.join("suite.snapshot.toml");
    let suite = toml::from_str::<EvalSuite>(&fs::read_to_string(suite_path)?)?;
    Ok(suite)
}

fn check_metric_percent_regression(
    diagnostics: &mut Vec<EvalDiagnostic>,
    variant_id: &str,
    metric: &str,
    baseline: Option<f64>,
    candidate: Option<f64>,
    max_increase_percent: Option<f64>,
) {
    let Some(limit) = max_increase_percent else {
        return;
    };
    let (Some(baseline), Some(candidate)) = (baseline, candidate) else {
        return;
    };
    if baseline <= 0.0 {
        return;
    }
    let increase = ((candidate - baseline) / baseline) * 100.0;
    if increase > limit {
        diagnostics.push(EvalDiagnostic {
            severity: EvalDiagnosticSeverity::Error,
            message: format!(
                "variant {variant_id} metric {metric} increased by {increase:.2}% from {baseline:.2} to {candidate:.2}; limit {limit:.2}%"
            ),
        });
    }
}

fn aggregate_variant(
    suite: &EvalSuite,
    variant: &EvalVariant,
    cases: Vec<EvalCaseRunResult>,
) -> EvalVariantRunResult {
    let repetitions = cases
        .iter()
        .flat_map(|case| case.repetitions.iter())
        .collect::<Vec<_>>();
    let pass_rate = pass_rate(repetitions.iter().map(|rep| rep.passed));
    let measurements = average_measurements(&repetitions);
    let correctness = pass_rate;
    let speed = inverse_score(
        measurements
            .get("wall_time_ms")
            .copied()
            .unwrap_or_default(),
        60_000.0,
    );
    let efficiency = metric_efficiency_score(suite, &measurements).unwrap_or_else(|| {
        inverse_score(
            measurements.get("diff_bytes").copied().unwrap_or_default(),
            50_000.0,
        )
    });
    let stability = 1.0 - pass_rate_variance(&cases);
    let cost = 1.0;
    let mut overall = suite.score.correctness.mul_add(
        correctness,
        suite.score.efficiency.mul_add(
            efficiency,
            suite.score.speed.mul_add(
                speed,
                suite
                    .score
                    .cost
                    .mul_add(cost, suite.score.stability * stability),
            ),
        ),
    );
    if suite.score.correctness_required && correctness < 1.0 {
        overall *= correctness;
    }
    EvalVariantRunResult {
        variant_id: variant.id.clone(),
        cases,
        pass_rate,
        measurements,
        score: bcode_eval_models::EvalScore {
            correctness,
            efficiency,
            speed,
            cost,
            stability,
            overall,
        },
    }
}

fn prepare_workspace(
    suite: &EvalSuite,
    suite_dir: &Path,
    rep_dir: &Path,
    case: &EvalCase,
) -> Result<PathBuf, EvalError> {
    let workspace = rep_dir.join("workspace");
    fs::create_dir_all(&workspace)?;
    match suite.run.isolation {
        EvalIsolation::EmptyTemp => {}
        EvalIsolation::TempCopy => {
            if let Some(fixture) = &case.fixture {
                copy_dir_all(&suite_dir.join(fixture), &workspace)?;
                initialize_workspace_git(&workspace);
            }
        }
        EvalIsolation::InPlace => {
            if let Some(fixture) = &case.fixture {
                return Ok(suite_dir.join(fixture));
            }
        }
    }
    Ok(workspace)
}

fn copy_dir_all(source: &Path, destination: &Path) -> Result<(), EvalError> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest = destination.join(entry.file_name());
        if ty.is_dir() {
            fs::create_dir_all(&dest)?;
            copy_dir_all(&entry.path(), &dest)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

fn initialize_workspace_git(workspace: &Path) {
    if workspace.join(".git").exists() {
        return;
    }
    let init = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(workspace)
        .status();
    let Ok(status) = init else {
        return;
    };
    if !status.success() {
        return;
    }
    let _ = Command::new("git")
        .args(["add", "."])
        .current_dir(workspace)
        .status();
    let _ = Command::new("git")
        .args(["commit", "--quiet", "--message", "eval fixture baseline"])
        .current_dir(workspace)
        .env("GIT_AUTHOR_NAME", "Bcode Eval")
        .env("GIT_AUTHOR_EMAIL", "eval@example.invalid")
        .env("GIT_COMMITTER_NAME", "Bcode Eval")
        .env("GIT_COMMITTER_EMAIL", "eval@example.invalid")
        .status();
}

fn git_diff(workspace: &Path) -> Result<String, EvalError> {
    let output = Command::new("git")
        .arg("diff")
        .arg("--no-ext-diff")
        .arg("--")
        .current_dir(workspace)
        .output();
    match output {
        Ok(output) if output.status.success() => {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        _ => directory_diff_snapshot(workspace),
    }
}

fn directory_diff_snapshot(workspace: &Path) -> Result<String, EvalError> {
    let mut text = String::new();
    collect_file_snapshot(workspace, workspace, &mut text)?;
    Ok(text)
}

fn collect_file_snapshot(root: &Path, current: &Path, text: &mut String) -> Result<(), EvalError> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name() == Some(OsStr::new(".git")) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            collect_file_snapshot(root, &path, text)?;
        } else if entry.file_type()?.is_file() {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            text.push_str(&format!("--- {}\n", relative.display()));
            let mut file = File::open(&path)?;
            let mut contents = String::new();
            let _ = file.read_to_string(&mut contents);
            text.push_str(&contents);
            if !contents.ends_with('\n') {
                text.push('\n');
            }
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct DiffStats {
    files_changed: usize,
    additions: usize,
    deletions: usize,
}

fn diff_stats(diff: &str) -> DiffStats {
    let mut stats = DiffStats::default();
    for line in diff.lines() {
        if line.starts_with("diff --git") {
            stats.files_changed += 1;
        } else if line.starts_with('+') && !line.starts_with("+++") {
            stats.additions += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            stats.deletions += 1;
        }
    }
    stats
}

fn regex_target(target: EvalRegexTarget, rep_dir: &Path, diff: &str) -> Result<String, EvalError> {
    Ok(match target {
        EvalRegexTarget::Stdout => fs::read_to_string(rep_dir.join("stdout.log"))?,
        EvalRegexTarget::Stderr => fs::read_to_string(rep_dir.join("stderr.log"))?,
        EvalRegexTarget::Diff => diff.to_string(),
        EvalRegexTarget::Output => {
            let mut output = fs::read_to_string(rep_dir.join("stdout.log"))?;
            output.push_str(&fs::read_to_string(rep_dir.join("stderr.log"))?);
            output
        }
    })
}

fn render_template(
    template: &str,
    suite: &EvalSuite,
    case: &EvalCase,
    variant: &EvalVariant,
    repetition: u32,
    workspace: &Path,
    artifact_dir: &Path,
    prompt: &str,
) -> String {
    template
        .replace("{{suite_id}}", &shell_escape(&suite.id))
        .replace("{{case_id}}", &shell_escape(&case.id))
        .replace("{{variant_id}}", &shell_escape(&variant.id))
        .replace("{{repetition}}", &repetition.to_string())
        .replace(
            "{{workspace}}",
            &shell_escape(&workspace.display().to_string()),
        )
        .replace(
            "{{artifact_dir}}",
            &shell_escape(&artifact_dir.display().to_string()),
        )
        .replace("{{prompt}}", &shell_escape(prompt))
}

fn rendered_prompt(case: &EvalCase, variant: &EvalVariant) -> String {
    let mut prompt = String::new();
    if let Some(prefix) = &variant.prompt_overlay.prefix {
        prompt.push_str(prefix);
        prompt.push('\n');
    }
    if let Some(case_prompt) = &case.prompt {
        prompt.push_str(case_prompt);
    }
    if let Some(suffix) = &variant.prompt_overlay.suffix {
        prompt.push('\n');
        prompt.push_str(suffix);
    }
    prompt
}

fn shell_command(command: &str) -> Command {
    let mut shell = Command::new("sh");
    shell.arg("-c").arg(command);
    shell
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn capture_environment() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if let Ok(output) = Command::new("git").args(["rev-parse", "HEAD"]).output()
        && output.status.success()
    {
        env.insert(
            "git_head".into(),
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        );
    }
    if let Ok(cwd) = std::env::current_dir() {
        env.insert("cwd".into(), cwd.display().to_string());
    }
    env
}

fn append_event<T: Serialize>(run_dir: &Path, source: &str, payload: &T) -> Result<(), EvalError> {
    append_jsonl(
        &run_dir.join("events.jsonl"),
        &EvalObservation {
            unix_ms: unix_ms(),
            source: source.into(),
            payload: serde_json::to_value(payload)?,
        },
    )
}

fn append_observation(rep_dir: &Path, observation: &EvalObservation) -> Result<(), EvalError> {
    append_jsonl(&rep_dir.join("observations.jsonl"), observation)
}

fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> Result<(), EvalError> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", serde_json::to_string(value)?)?;
    Ok(())
}

fn write_json(path: impl AsRef<Path>, value: &impl Serialize) -> Result<(), EvalError> {
    fs::write(path, serde_json::to_string_pretty(value)?)?;
    Ok(())
}

fn render_summary_markdown(result: &EvalRunResult) -> String {
    let mut out = format!("# Eval Run {}\n\n", result.manifest.run_id);
    out.push_str(&format!("Suite: `{}`\n\n", result.manifest.suite_id));
    out.push_str("| Variant | Pass rate | Score |\n|---|---:|---:|\n");
    for variant in &result.variants {
        out.push_str(&format!(
            "| {} | {:.2}% | {:.3} |\n",
            variant.variant_id,
            variant.pass_rate * 100.0,
            variant.score.overall
        ));
    }
    out
}

fn validate_id(kind: &str, id: &str) -> Result<(), EvalError> {
    if id.is_empty()
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(EvalError::Validation(format!(
            "{kind} id must be non-empty and contain only ascii alphanumeric, '-', '_', or '.': {id}"
        )));
    }
    Ok(())
}

fn pass_rate(values: impl IntoIterator<Item = bool>) -> f64 {
    let mut total = 0_u32;
    let mut passed = 0_u32;
    for value in values {
        total += 1;
        if value {
            passed += 1;
        }
    }
    if total == 0 {
        0.0
    } else {
        f64::from(passed) / f64::from(total)
    }
}

fn average_measurements(repetitions: &[&EvalRepetitionResult]) -> EvalMeasurementSet {
    let mut sums: BTreeMap<String, (f64, u32)> = BTreeMap::new();
    for repetition in repetitions {
        for (key, value) in &repetition.measurements {
            let entry = sums.entry(key.clone()).or_default();
            entry.0 += value;
            entry.1 += 1;
        }
    }
    sums.into_iter()
        .map(|(key, (sum, count))| (key, sum / f64::from(count)))
        .collect()
}

fn metric_efficiency_score(suite: &EvalSuite, measurements: &EvalMeasurementSet) -> Option<f64> {
    if suite.score.metrics.is_empty() {
        return None;
    }
    let mut weighted = 0.0;
    let mut total_weight = 0_u32;
    for rule in &suite.score.metrics {
        let Some(value) = measurements.get(&rule.metric).copied() else {
            continue;
        };
        let target = rule
            .target
            .map_or_else(|| value.max(1.0), |target| target as f64);
        let score = match rule.direction {
            EvalMetricDirection::LowerIsBetter => inverse_score(value, target),
            EvalMetricDirection::HigherIsBetter => (value / target).clamp(0.0, 1.0),
        };
        weighted += score * f64::from(rule.weight);
        total_weight = total_weight.saturating_add(rule.weight);
    }
    (total_weight > 0).then_some(weighted / f64::from(total_weight))
}

fn inverse_score(value: f64, target: f64) -> f64 {
    if value <= 0.0 {
        1.0
    } else {
        (1.0 - (value / target)).clamp(0.0, 1.0)
    }
}

fn pass_rate_variance(cases: &[EvalCaseRunResult]) -> f64 {
    if cases.is_empty() {
        return 0.0;
    }
    let mean = cases.iter().map(|case| case.pass_rate).sum::<f64>() / cases.len() as f64;
    cases
        .iter()
        .map(|case| (case.pass_rate - mean).powi(2))
        .sum::<f64>()
        / cases.len() as f64
}

fn diagnostic_if_failed(passed: bool, message: &str) -> Vec<EvalDiagnostic> {
    if passed {
        Vec::new()
    } else {
        vec![EvalDiagnostic {
            severity: EvalDiagnosticSeverity::Error,
            message: message.into(),
        }]
    }
}

fn normalize_patch_for_compare(value: &str) -> String {
    normalize_newlines(value)
        .lines()
        .filter(|line| !line.starts_with("index "))
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n")
}

fn default_run_id(suite_id: &str) -> String {
    format!("{}-{suite_id}", unix_ms())
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn relative_artifact(run_dir: &Path, kind: &str, path: &Path) -> EvalArtifactRef {
    EvalArtifactRef {
        kind: kind.into(),
        path: path.strip_prefix(run_dir).unwrap_or(path).to_path_buf(),
    }
}
