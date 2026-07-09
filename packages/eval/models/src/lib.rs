#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Serializable schema types for Bcode eval suites, runs, and reports.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Current eval improvement campaign schema version.
pub const CURRENT_IMPROVEMENT_SCHEMA_VERSION: u32 = 1;

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
    /// Regression threshold configuration.
    #[serde(default)]
    pub regression: EvalRegressionConfig,
    /// Arbitrary suite metadata.
    #[serde(default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
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
    /// Metric-specific scoring rules.
    #[serde(default)]
    pub metrics: Vec<EvalMetricScoreConfig>,
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
            metrics: Vec::new(),
        }
    }
}

/// Suite-level metric scoring rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalMetricScoreConfig {
    /// Metric key.
    pub metric: String,
    /// Direction considered better.
    pub direction: EvalMetricDirection,
    /// Weight for this metric in efficiency scoring.
    pub weight: u32,
    /// Optional target used to normalize the metric.
    #[serde(default)]
    pub target: Option<u64>,
}

/// Metric optimization direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalMetricDirection {
    /// Lower values are better.
    LowerIsBetter,
    /// Higher values are better.
    HigherIsBetter,
}

/// Regression thresholds for baseline comparisons.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRegressionConfig {
    /// Minimum acceptable candidate pass rate.
    #[serde(default)]
    pub min_pass_rate: Option<f64>,
    /// Maximum allowed token increase in percent.
    #[serde(default)]
    pub max_token_increase_percent: Option<f64>,
    /// Maximum allowed latency increase in percent.
    #[serde(default)]
    pub max_latency_increase_percent: Option<f64>,
    /// Treat newly failed variants as regressions.
    #[serde(default = "default_true")]
    pub fail_on_new_failure: bool,
}

impl Default for EvalRegressionConfig {
    fn default() -> Self {
        Self {
            min_pass_rate: None,
            max_token_increase_percent: Some(15.0),
            max_latency_increase_percent: Some(25.0),
            fail_on_new_failure: true,
        }
    }
}

/// Direct model-callable tool invocation configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalDirectToolConfig {
    /// Optional explicit producer plugin id.
    #[serde(default)]
    pub plugin_id: Option<String>,
    /// Tool name.
    pub tool_name: String,
    /// Tool arguments.
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// Replay configuration for analyzing existing session/event artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalReplayConfig {
    /// JSONL transcript path relative to the suite directory.
    pub transcript: PathBuf,
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
    /// Direct tool invocation settings for `direct_tool` variants.
    #[serde(default)]
    pub direct_tool: Option<EvalDirectToolConfig>,
    /// Replay settings for `replay` variants.
    #[serde(default)]
    pub replay: Option<EvalReplayConfig>,
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
    /// Direct tool invocation settings for this case.
    #[serde(default)]
    pub direct_tool: Option<EvalDirectToolConfig>,
    /// Replay settings for this case.
    #[serde(default)]
    pub replay: Option<EvalReplayConfig>,
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

/// Improvement campaign metadata for multi-generation eval self-improvement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalImprovementCampaign {
    /// Campaign schema version.
    pub schema_version: u32,
    /// Stable campaign id.
    pub id: String,
    /// Human-readable campaign name.
    pub name: String,
    /// Suite path used to create the campaign.
    pub suite_path: PathBuf,
    /// Suite id captured at campaign creation.
    pub suite_id: String,
    /// Unix timestamp in milliseconds when the campaign was created.
    pub created_unix_ms: u128,
    /// Campaign artifact root.
    pub output_dir: PathBuf,
    /// Baseline generation id.
    pub baseline_generation_id: String,
    /// Latest generation id on the main line.
    #[serde(default)]
    pub latest_generation_id: Option<String>,
    /// Best known generation id.
    #[serde(default)]
    pub best_generation_id: Option<String>,
    /// Campaign objective used as the default viewer lens.
    #[serde(default)]
    pub objective: EvalImprovementObjective,
    /// Arbitrary campaign metadata.
    #[serde(default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// Campaign objective/display lens for eval improvement history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalImprovementObjective {
    /// Track improvement over generations.
    #[default]
    Progression,
    /// Emphasize each generation's delta from its parent.
    ParentComparison,
    /// Emphasize each generation's delta from the baseline.
    BaselineComparison,
    /// Compare selected variants/generations.
    VariantComparison,
}

/// Generation record for one eval improvement attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalImprovementGeneration {
    /// Generation id, for example `0003`.
    pub id: String,
    /// Parent generation id.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Branch name.
    pub branch: String,
    /// Unix timestamp in milliseconds when created.
    pub created_unix_ms: u128,
    /// Delta that produced this generation.
    pub delta: EvalImprovementDelta,
    /// Eval run directory for this generation, when available.
    #[serde(default)]
    pub run_dir: Option<PathBuf>,
    /// Comparison against parent, when available.
    #[serde(default)]
    pub vs_parent: Option<EvalImprovementMetricDeltaSet>,
    /// Comparison against baseline, when available.
    #[serde(default)]
    pub vs_baseline: Option<EvalImprovementMetricDeltaSet>,
    /// Verdict for this generation.
    pub verdict: EvalImprovementVerdict,
    /// Arbitrary generation metadata.
    #[serde(default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// Improvement delta metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalImprovementDelta {
    /// Delta kind.
    pub kind: EvalImprovementDeltaKind,
    /// Short human-readable summary.
    pub summary: String,
    /// Affected files, relative to repository root when possible.
    #[serde(default)]
    pub affected_files: Vec<PathBuf>,
    /// Runtime surfaces affected by this delta.
    #[serde(default)]
    pub affected_surfaces: Vec<String>,
    /// Patch path relative to the generation directory.
    #[serde(default)]
    pub patch_path: Option<PathBuf>,
    /// Overlay paths relative to the generation directory.
    #[serde(default)]
    pub overlay_paths: Vec<PathBuf>,
    /// LLM or human rationale for this change.
    #[serde(default)]
    pub rationale: Option<String>,
    /// Expected impact before running the generation.
    #[serde(default)]
    pub expected_impact: Option<String>,
    /// Risk level.
    pub risk: EvalImprovementRisk,
}

/// Kinds of eval improvement deltas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalImprovementDeltaKind {
    /// Baseline generation with no change.
    Baseline,
    /// Temporary system prompt overlay.
    SystemPromptOverlay,
    /// Patch to the real system prompt source/configuration.
    SystemPromptPatch,
    /// Temporary tool description overlay.
    ToolDescriptionOverlay,
    /// Patch to tool schema.
    ToolSchemaPatch,
    /// Patch to tool implementation behavior.
    ToolBehaviorPatch,
    /// Temporary agent profile overlay.
    AgentProfileOverlay,
    /// Temporary permission policy overlay.
    PermissionPolicyOverlay,
    /// Model or model-parameter change.
    ModelChange,
    /// Eval case change.
    EvalCaseChange,
    /// Judge change.
    JudgeChange,
    /// Score configuration change.
    ScoringChange,
    /// Multiple change kinds.
    Mixed,
}

/// Risk level for an improvement proposal or delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalImprovementRisk {
    /// Low-risk isolated overlay or documentation-only change.
    Low,
    /// Medium-risk behavior/configuration change.
    Medium,
    /// High-risk source, policy, or broad prompt change.
    High,
}

/// Improvement verdict for a generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalImprovementVerdict {
    /// Verdict status.
    pub status: EvalImprovementVerdictStatus,
    /// Human-readable rationale.
    #[serde(default)]
    pub rationale: Option<String>,
    /// Whether this generation is eligible for promotion.
    pub promotable: bool,
    /// Actor responsible for a terminal human decision.
    #[serde(default)]
    pub decided_by: Option<String>,
    /// Unix timestamp in milliseconds for the terminal human decision.
    #[serde(default)]
    pub decided_unix_ms: Option<u128>,
}

/// Verdict status for an improvement generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalImprovementVerdictStatus {
    /// Baseline generation.
    Baseline,
    /// Improved against parent and/or baseline.
    Improved,
    /// Regressed against parent and/or baseline.
    Regressed,
    /// Mixed result with tradeoffs.
    Mixed,
    /// Needs human review or additional evals.
    NeedsReview,
    /// Rejected.
    Rejected,
    /// Promoted.
    Promoted,
}

/// Metric deltas for comparing generations.
pub type EvalImprovementMetricDeltaSet = BTreeMap<String, EvalImprovementMetricDelta>;

/// One metric delta value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalImprovementMetricDelta {
    /// Previous value.
    #[serde(default)]
    pub before: Option<f64>,
    /// Current value.
    #[serde(default)]
    pub after: Option<f64>,
    /// Absolute delta, `after - before`.
    #[serde(default)]
    pub absolute: Option<f64>,
    /// Percent delta.
    #[serde(default)]
    pub percent: Option<f64>,
}

/// Improvement proposal captured before a generation is created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalImprovementProposal {
    /// Proposal id.
    pub id: String,
    /// Proposed delta kind.
    pub kind: EvalImprovementDeltaKind,
    /// Proposal summary.
    pub summary: String,
    /// Evidence supporting the proposal.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Candidate change description.
    pub candidate_change: String,
    /// Expected impact.
    #[serde(default)]
    pub expected_impact: Option<String>,
    /// Risk level.
    pub risk: EvalImprovementRisk,
    /// Whether source changes are required.
    pub requires_code_change: bool,
    /// Whether human review is required before promotion.
    pub requires_human_review: bool,
}

const fn default_true() -> bool {
    true
}
