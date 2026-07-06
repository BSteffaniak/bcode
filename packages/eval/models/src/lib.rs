#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Serializable schema types for Bcode eval suites, runs, and reports.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Current eval suite schema version.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Named eval suite containing cases, variants, judges, and run defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalSuite {
    /// Suite schema version.
    pub schema_version: u32,
    /// Stable suite id used in artifact paths and baselines.
    pub id: String,
    /// Human-readable suite name.
    pub name: String,
    /// Optional suite description.
    #[serde(default)]
    pub description: Option<String>,
    /// Run defaults.
    #[serde(default)]
    pub run: EvalRunConfig,
    /// Environment capture settings.
    #[serde(default)]
    pub environment: EvalEnvironmentConfig,
    /// Score aggregation weights.
    #[serde(default)]
    pub score: EvalScoreConfig,
    /// Variant definitions.
    #[serde(default)]
    pub variants: Vec<EvalVariant>,
    /// Case definitions.
    #[serde(default)]
    pub cases: Vec<EvalCase>,
}

/// Run defaults for a suite or invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalRunConfig {
    /// Repetitions per case/variant pair.
    pub repetitions: u32,
    /// Per-repetition timeout in milliseconds.
    pub timeout_ms: u64,
    /// Workspace isolation strategy.
    pub isolation: EvalIsolation,
    /// Randomize case order before execution.
    pub randomize_case_order: bool,
    /// Stop at the first failed repetition.
    pub fail_fast: bool,
}

impl Default for EvalRunConfig {
    fn default() -> Self {
        Self {
            repetitions: 1,
            timeout_ms: 60_000,
            isolation: EvalIsolation::TempCopy,
            randomize_case_order: false,
            fail_fast: false,
        }
    }
}

/// Workspace isolation strategy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalIsolation {
    /// Copy fixture files into the repetition workspace.
    #[default]
    TempCopy,
    /// Run in an empty temporary workspace.
    EmptyTemp,
    /// Run directly in the fixture directory.
    InPlace,
}

/// Environment capture toggles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalEnvironmentConfig {
    /// Capture git metadata for the current repository.
    pub capture_git: bool,
    /// Capture Bcode executable/version metadata when available.
    pub capture_bcode_version: bool,
    /// Capture tool version metadata when available.
    pub capture_tool_versions: bool,
}

impl Default for EvalEnvironmentConfig {
    fn default() -> Self {
        Self {
            capture_git: true,
            capture_bcode_version: true,
            capture_tool_versions: true,
        }
    }
}

/// Weighted score configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalScoreConfig {
    /// Correctness weight.
    pub correctness: f64,
    /// Efficiency weight.
    pub efficiency: f64,
    /// Speed weight.
    pub speed: f64,
    /// Cost weight.
    pub cost: f64,
    /// Stability weight.
    pub stability: f64,
    /// Whether failed correctness gates the overall score to zero.
    pub correctness_required: bool,
}

impl Default for EvalScoreConfig {
    fn default() -> Self {
        Self {
            correctness: 0.65,
            efficiency: 0.15,
            speed: 0.10,
            cost: 0.0,
            stability: 0.10,
            correctness_required: true,
        }
    }
}

/// A suite variant describing what changes between comparable runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalVariant {
    /// Stable variant id.
    pub id: String,
    /// Optional human-readable name.
    #[serde(default)]
    pub name: Option<String>,
    /// Executor kind.
    #[serde(default)]
    pub executor: EvalExecutorKind,
    /// Optional model/profile metadata for reports.
    #[serde(default)]
    pub profile: Option<String>,
    /// Optional model id metadata for reports.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional declared allowed tools for reports/templates.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Command template for command execution.
    #[serde(default)]
    pub command: Option<String>,
    /// Additional environment variables for execution.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Prompt overlays used by agent-like command templates.
    #[serde(default)]
    pub prompt_overlay: EvalPromptOverlay,
    /// Arbitrary variant metadata.
    #[serde(default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// Executor kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalExecutorKind {
    /// Execute a shell command template.
    #[default]
    Command,
    /// Agent session execution through a public command/API adapter.
    Agent,
    /// Direct tool invocation adapter.
    DirectTool,
    /// Replay/analyze existing artifacts.
    Replay,
}

/// Prompt overlay applied to a case prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalPromptOverlay {
    /// Text prepended to the case prompt.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Text appended to the case prompt.
    #[serde(default)]
    pub suffix: Option<String>,
    /// System prompt addition metadata.
    #[serde(default)]
    pub system_append: Option<String>,
}

/// A single eval task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalCase {
    /// Stable case id.
    pub id: String,
    /// Optional human-readable name.
    #[serde(default)]
    pub name: Option<String>,
    /// Optional fixture directory.
    #[serde(default)]
    pub fixture: Option<PathBuf>,
    /// Prompt/task text.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Optional per-case timeout override.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Optional case command override.
    #[serde(default)]
    pub command: Option<String>,
    /// Additional environment variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Judges for this case.
    #[serde(default)]
    pub judges: Vec<EvalJudgeConfig>,
    /// Arbitrary case metadata.
    #[serde(default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// Judge configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvalJudgeConfig {
    /// Compare git diff against an expected patch.
    ExactDiff {
        /// Expected patch path relative to suite directory.
        expected_patch: PathBuf,
        /// Whether this judge is required for correctness.
        #[serde(default = "default_true")]
        required: bool,
    },
    /// Run a command in the workspace.
    Command {
        /// Command to execute with the platform shell.
        command: String,
        /// Expected exit code.
        #[serde(default)]
        expected_exit_code: i32,
        /// Whether this judge is required for correctness.
        #[serde(default = "default_true")]
        required: bool,
    },
    /// Compare a workspace file against an expected file.
    FileSnapshot {
        /// Workspace-relative file path.
        path: PathBuf,
        /// Expected file path relative to suite directory.
        expected: PathBuf,
        /// Whether this judge is required for correctness.
        #[serde(default = "default_true")]
        required: bool,
    },
    /// Assert regex presence/absence.
    Regex {
        /// Target to inspect.
        target: EvalRegexTarget,
        /// Regex pattern.
        pattern: String,
        /// Whether a match is expected.
        #[serde(default = "default_true")]
        should_match: bool,
        /// Whether this judge is required for correctness.
        #[serde(default = "default_true")]
        required: bool,
    },
    /// Enforce a numeric metric threshold.
    MetricThreshold {
        /// Metric key.
        metric: String,
        /// Inclusive minimum value.
        #[serde(default)]
        min: Option<f64>,
        /// Inclusive maximum value.
        #[serde(default)]
        max: Option<f64>,
        /// Whether this judge is required for correctness.
        #[serde(default = "default_true")]
        required: bool,
    },
}

/// Regex judge target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalRegexTarget {
    /// Captured stdout.
    Stdout,
    /// Captured stderr.
    Stderr,
    /// Captured git diff.
    Diff,
    /// Final combined output.
    Output,
}

/// Run manifest persisted with each run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalRunManifest {
    /// Unique run id.
    pub run_id: String,
    /// Suite id.
    pub suite_id: String,
    /// Suite name.
    pub suite_name: String,
    /// Unix timestamp in milliseconds.
    pub started_unix_ms: u128,
    /// Run artifact root.
    pub output_dir: PathBuf,
    /// Suite path when loaded from disk.
    #[serde(default)]
    pub suite_path: Option<PathBuf>,
    /// Captured environment metadata.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

/// Raw observations emitted during execution and judging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalObservation {
    /// Unix timestamp in milliseconds.
    pub unix_ms: u128,
    /// Observation source.
    pub source: String,
    /// Observation payload.
    pub payload: serde_json::Value,
}

/// Numeric measurements by metric key.
pub type EvalMeasurementSet = BTreeMap<String, f64>;

/// Artifact reference in a run directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalArtifactRef {
    /// Artifact kind.
    pub kind: String,
    /// Path relative to the run directory.
    pub path: PathBuf,
}

/// Diagnostic emitted by execution or judging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalDiagnostic {
    /// Severity.
    pub severity: EvalDiagnosticSeverity,
    /// Message.
    pub message: String,
}

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalDiagnosticSeverity {
    /// Informational diagnostic.
    Info,
    /// Warning diagnostic.
    Warning,
    /// Error diagnostic.
    Error,
}

/// Judge result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalJudgeResult {
    /// Judge kind label.
    pub kind: String,
    /// Whether the judge passed.
    pub passed: bool,
    /// Whether the judge is required for correctness.
    pub required: bool,
    /// Optional normalized score.
    #[serde(default)]
    pub score: Option<f64>,
    /// Measurements emitted by the judge.
    #[serde(default)]
    pub measurements: EvalMeasurementSet,
    /// Diagnostics emitted by the judge.
    #[serde(default)]
    pub diagnostics: Vec<EvalDiagnostic>,
    /// Artifact refs emitted by the judge.
    #[serde(default)]
    pub artifacts: Vec<EvalArtifactRef>,
}

/// Repetition result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRepetitionResult {
    /// Case id.
    pub case_id: String,
    /// Variant id.
    pub variant_id: String,
    /// One-based repetition index.
    pub repetition: u32,
    /// Whether execution and required judges passed.
    pub passed: bool,
    /// Command exit code, when command execution happened.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Wall time in milliseconds.
    pub wall_time_ms: u128,
    /// Measurements.
    #[serde(default)]
    pub measurements: EvalMeasurementSet,
    /// Judge results.
    #[serde(default)]
    pub judges: Vec<EvalJudgeResult>,
    /// Diagnostics.
    #[serde(default)]
    pub diagnostics: Vec<EvalDiagnostic>,
    /// Artifacts.
    #[serde(default)]
    pub artifacts: Vec<EvalArtifactRef>,
}

/// Case aggregate result for one variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalCaseRunResult {
    /// Case id.
    pub case_id: String,
    /// Variant id.
    pub variant_id: String,
    /// Repetition results.
    pub repetitions: Vec<EvalRepetitionResult>,
    /// Pass rate from zero to one.
    pub pass_rate: f64,
}

/// Variant aggregate result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalVariantRunResult {
    /// Variant id.
    pub variant_id: String,
    /// Case results.
    pub cases: Vec<EvalCaseRunResult>,
    /// Pass rate from zero to one.
    pub pass_rate: f64,
    /// Aggregate measurements.
    pub measurements: EvalMeasurementSet,
    /// Aggregate score.
    pub score: EvalScore,
}

/// Score dimensions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EvalScore {
    /// Correctness score.
    pub correctness: f64,
    /// Efficiency score.
    pub efficiency: f64,
    /// Speed score.
    pub speed: f64,
    /// Cost score.
    pub cost: f64,
    /// Stability score.
    pub stability: f64,
    /// Weighted overall score.
    pub overall: f64,
}

/// Full run result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRunResult {
    /// Manifest.
    pub manifest: EvalRunManifest,
    /// Variant results.
    pub variants: Vec<EvalVariantRunResult>,
    /// Whether all variants passed all required checks.
    pub passed: bool,
}

/// Comparison between two or more runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalComparisonReport {
    /// Compared run ids.
    pub run_ids: Vec<String>,
    /// Winning variant id, when available.
    #[serde(default)]
    pub winner: Option<String>,
    /// Variant summaries.
    pub variants: Vec<EvalComparisonVariant>,
    /// Diagnostics/regression notes.
    #[serde(default)]
    pub diagnostics: Vec<EvalDiagnostic>,
}

/// Variant comparison summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalComparisonVariant {
    /// Variant id.
    pub variant_id: String,
    /// Pass rate.
    pub pass_rate: f64,
    /// Overall score.
    pub overall_score: f64,
    /// Aggregate measurements.
    pub measurements: EvalMeasurementSet,
}

/// Baseline record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalBaseline {
    /// Suite id.
    pub suite_id: String,
    /// Baseline run id.
    pub run_id: String,
    /// Path to baseline run.
    pub run_path: PathBuf,
}

/// Regression report against a baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalRegressionReport {
    /// Baseline run id.
    pub baseline_run_id: String,
    /// Candidate run id.
    pub candidate_run_id: String,
    /// Whether regressions were detected.
    pub regressed: bool,
    /// Diagnostics.
    pub diagnostics: Vec<EvalDiagnostic>,
}

const fn default_true() -> bool {
    true
}
