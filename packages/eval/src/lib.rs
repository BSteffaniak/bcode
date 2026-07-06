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
    EvalObservation, EvalRegexTarget, EvalRegressionReport, EvalRepetitionResult, EvalRunManifest,
    EvalRunResult, EvalSuite, EvalVariant, EvalVariantRunResult,
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
    let suite = load_suite(&options.suite_path)?;
    let suite_dir = options
        .suite_path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
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
    if variant.executor != EvalExecutorKind::Command {
        return Err(EvalError::UnsupportedExecutor {
            variant_id: variant.id.clone(),
            reason: "use executor = \"command\" with a command template; agent/direct-tool/replay adapters must be expressed through public commands until native adapters are added".into(),
        });
    }
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
            let passed = normalize_newlines(&expected).trim() == normalize_newlines(diff).trim();
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
    let mut diagnostics = Vec::new();
    for candidate_variant in &candidate.variants {
        if let Some(base_variant) = baseline_run
            .variants
            .iter()
            .find(|variant| variant.variant_id == candidate_variant.variant_id)
        {
            if candidate_variant.pass_rate < base_variant.pass_rate {
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
    let efficiency = inverse_score(
        measurements.get("diff_bytes").copied().unwrap_or_default(),
        50_000.0,
    );
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
        if line.starts_with("diff --git") || line.starts_with("--- ") {
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
