//! Provider-independent scoring for SDK model and agent results.

use bcode_eval_models::EvalMeasurementSet;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Provider-independent material captured from one SDK operation for scoring.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SdkEvalSubject {
    /// Final generated output text.
    pub output: String,
    /// Decoded structured output when the operation produced one.
    #[serde(default)]
    pub structured_value: Option<serde_json::Value>,
    /// Ordered tool trace entries. Applications choose the serializable trace schema.
    #[serde(default)]
    pub tool_trace: Vec<serde_json::Value>,
    /// Ordered agent/model/tool steps. Applications choose the serializable step schema.
    #[serde(default)]
    pub agent_steps: Vec<serde_json::Value>,
    /// End-to-end latency in milliseconds when measured.
    #[serde(default)]
    pub latency_ms: Option<u64>,
    /// Provider/application usage measurements such as token counts or cost.
    #[serde(default)]
    pub usage: EvalMeasurementSet,
    /// Application-defined material available to custom criteria.
    #[serde(default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl SdkEvalSubject {
    /// Create a subject from final output text.
    #[must_use]
    pub fn new(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            ..Self::default()
        }
    }

    /// Attach one decoded structured value.
    #[must_use]
    pub fn structured_value(mut self, value: serde_json::Value) -> Self {
        self.structured_value = Some(value);
        self
    }

    /// Attach an ordered tool trace.
    #[must_use]
    pub fn tool_trace(mut self, trace: impl IntoIterator<Item = serde_json::Value>) -> Self {
        self.tool_trace = trace.into_iter().collect();
        self
    }

    /// Attach ordered agent steps.
    #[must_use]
    pub fn agent_steps(mut self, steps: impl IntoIterator<Item = serde_json::Value>) -> Self {
        self.agent_steps = steps.into_iter().collect();
        self
    }

    /// Attach measured latency.
    #[must_use]
    pub const fn latency_ms(mut self, latency_ms: u64) -> Self {
        self.latency_ms = Some(latency_ms);
        self
    }

    /// Add one usage measurement.
    #[must_use]
    pub fn usage(mut self, key: impl Into<String>, value: f64) -> Self {
        self.usage.insert(key.into(), value);
        self
    }

    /// Add application-defined scoring metadata.
    #[must_use]
    pub fn metadata(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }
}

/// One normalized criterion score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SdkCriterionScore {
    /// Stable criterion name.
    pub criterion: String,
    /// Normalized score in the inclusive range zero to one.
    pub score: f64,
    /// Whether the criterion's acceptance threshold passed.
    pub passed: bool,
    /// Criterion-specific numeric measurements.
    #[serde(default)]
    pub measurements: EvalMeasurementSet,
    /// Safe human-readable diagnostic messages.
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

impl SdkCriterionScore {
    /// Create a score after validating its normalized range.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is empty or score is non-finite or outside zero through one.
    pub fn new(
        criterion: impl Into<String>,
        score: f64,
        passed: bool,
    ) -> Result<Self, SdkEvalError> {
        let criterion = criterion.into();
        if criterion.trim().is_empty() {
            return Err(SdkEvalError::InvalidCriterionName);
        }
        if !score.is_finite() || !(0.0..=1.0).contains(&score) {
            return Err(SdkEvalError::InvalidScore { criterion, score });
        }
        Ok(Self {
            criterion,
            score,
            passed,
            measurements: EvalMeasurementSet::new(),
            diagnostics: Vec::new(),
        })
    }

    /// Add one numeric measurement.
    #[must_use]
    pub fn measurement(mut self, key: impl Into<String>, value: f64) -> Self {
        self.measurements.insert(key.into(), value);
        self
    }

    /// Add one safe diagnostic.
    #[must_use]
    pub fn diagnostic(mut self, diagnostic: impl Into<String>) -> Self {
        self.diagnostics.push(diagnostic.into());
        self
    }
}

/// Application-extensible criterion over one complete SDK evaluation subject.
pub trait SdkEvalCriterion: Send + Sync {
    /// Score one subject.
    ///
    /// # Errors
    ///
    /// Returns a typed evaluation error when scoring cannot produce an honest result.
    fn score(&self, subject: &SdkEvalSubject) -> Result<SdkCriterionScore, SdkEvalError>;
}

/// Ordered provider-independent SDK scorer.
#[derive(Default)]
pub struct SdkEvaluator {
    criteria: Vec<Box<dyn SdkEvalCriterion>>,
}

impl std::fmt::Debug for SdkEvaluator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SdkEvaluator")
            .field("criterion_count", &self.criteria.len())
            .finish()
    }
}

impl SdkEvaluator {
    /// Create an empty evaluator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one criterion. Criteria execute in registration order.
    #[must_use]
    pub fn criterion<C>(mut self, criterion: C) -> Self
    where
        C: SdkEvalCriterion + 'static,
    {
        self.criteria.push(Box::new(criterion));
        self
    }

    /// Score one subject with every configured criterion.
    ///
    /// The overall score is the arithmetic mean of criterion scores. An empty evaluator returns
    /// no criterion scores, overall score zero, and `passed = true` rather than inventing evidence.
    ///
    /// # Errors
    ///
    /// Returns the first typed criterion failure or invalid score.
    pub fn evaluate(&self, subject: &SdkEvalSubject) -> Result<SdkEvalReport, SdkEvalError> {
        let mut scores = Vec::with_capacity(self.criteria.len());
        for criterion in &self.criteria {
            scores.push(criterion.score(subject)?);
        }
        let passed = scores.iter().all(|score| score.passed);
        let overall_score = if scores.is_empty() {
            0.0
        } else {
            scores.iter().map(|score| score.score).sum::<f64>() / scores.len() as f64
        };
        Ok(SdkEvalReport {
            passed,
            overall_score,
            scores,
        })
    }
}

/// Aggregate result from scoring one SDK subject.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SdkEvalReport {
    /// Whether every criterion passed.
    pub passed: bool,
    /// Arithmetic mean of normalized criterion scores.
    pub overall_score: f64,
    /// Criterion scores in registration order.
    pub scores: Vec<SdkCriterionScore>,
}

/// SDK scoring failure.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SdkEvalError {
    /// Criterion name was empty.
    #[error("SDK evaluation criterion name must not be empty")]
    InvalidCriterionName,
    /// Criterion emitted an invalid normalized score.
    #[error("SDK evaluation criterion {criterion} emitted invalid score {score}")]
    InvalidScore {
        /// Criterion name.
        criterion: String,
        /// Invalid score.
        score: f64,
    },
    /// A required subject measurement was absent.
    #[error("SDK evaluation subject is missing {field}")]
    MissingSubjectField {
        /// Stable missing field name.
        field: &'static str,
    },
    /// Application-defined criterion failed.
    #[error("SDK evaluation criterion failed: {0}")]
    Criterion(String),
    /// Run ID was empty.
    #[error("SDK evaluation run ID must not be empty")]
    InvalidRunId,
    /// Required evaluator/scoring configuration was empty.
    #[error("SDK evaluation run configuration field {field} must not be empty")]
    InvalidRunConfig {
        /// Stable configuration field name.
        field: &'static str,
    },
    /// Required provenance was empty.
    #[error("SDK evaluation provenance field {field} must not be empty")]
    InvalidProvenance {
        /// Stable provenance field name.
        field: &'static str,
    },
    /// Case ID was duplicated within one run.
    #[error("SDK evaluation case ID {case_id} is duplicated")]
    DuplicateCaseId {
        /// Duplicate case ID.
        case_id: String,
    },
    /// Run artifact uses an unsupported schema.
    #[error("unsupported SDK evaluation run schema {actual}")]
    UnsupportedRunSchema {
        /// Artifact schema version.
        actual: u32,
    },
    /// Run artifact serialization failed.
    #[error("SDK evaluation serialization failed: {0}")]
    Serialization(String),
    /// Run artifact I/O failed.
    #[error("SDK evaluation I/O failed: {0}")]
    Io(String),
}

/// Require output to contain one substring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputContains {
    expected: String,
}

impl OutputContains {
    /// Create an output substring criterion.
    #[must_use]
    pub fn new(expected: impl Into<String>) -> Self {
        Self {
            expected: expected.into(),
        }
    }
}

impl SdkEvalCriterion for OutputContains {
    fn score(&self, subject: &SdkEvalSubject) -> Result<SdkCriterionScore, SdkEvalError> {
        let passed = subject.output.contains(&self.expected);
        SdkCriterionScore::new("output_contains", f64::from(passed), passed)
    }
}

/// Require exact decoded structured output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredEquals {
    expected: serde_json::Value,
}

impl StructuredEquals {
    /// Create an exact structured-value criterion.
    #[must_use]
    pub const fn new(expected: serde_json::Value) -> Self {
        Self { expected }
    }
}

impl SdkEvalCriterion for StructuredEquals {
    fn score(&self, subject: &SdkEvalSubject) -> Result<SdkCriterionScore, SdkEvalError> {
        let actual =
            subject
                .structured_value
                .as_ref()
                .ok_or(SdkEvalError::MissingSubjectField {
                    field: "structured_value",
                })?;
        let passed = actual == &self.expected;
        SdkCriterionScore::new("structured_equals", f64::from(passed), passed)
    }
}

/// Require an exact number of tool trace entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolTraceCount {
    expected: usize,
}

impl ToolTraceCount {
    /// Create an exact tool-trace count criterion.
    #[must_use]
    pub const fn new(expected: usize) -> Self {
        Self { expected }
    }
}

impl SdkEvalCriterion for ToolTraceCount {
    fn score(&self, subject: &SdkEvalSubject) -> Result<SdkCriterionScore, SdkEvalError> {
        let actual = subject.tool_trace.len();
        let passed = actual == self.expected;
        SdkCriterionScore::new("tool_trace_count", f64::from(passed), passed)
            .map(|score| score.measurement("tool_trace_count", actual as f64))
    }
}

/// Require an exact number of serialized agent steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentStepCount {
    expected: usize,
}

impl AgentStepCount {
    /// Create an exact agent-step count criterion.
    #[must_use]
    pub const fn new(expected: usize) -> Self {
        Self { expected }
    }
}

impl SdkEvalCriterion for AgentStepCount {
    fn score(&self, subject: &SdkEvalSubject) -> Result<SdkCriterionScore, SdkEvalError> {
        let actual = subject.agent_steps.len();
        let passed = actual == self.expected;
        SdkCriterionScore::new("agent_step_count", f64::from(passed), passed)
            .map(|score| score.measurement("agent_step_count", actual as f64))
    }
}

/// Require measured latency at or below one bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencyAtMost {
    maximum_ms: u64,
}

impl LatencyAtMost {
    /// Create a latency-bound criterion.
    #[must_use]
    pub const fn new(maximum_ms: u64) -> Self {
        Self { maximum_ms }
    }
}

impl SdkEvalCriterion for LatencyAtMost {
    fn score(&self, subject: &SdkEvalSubject) -> Result<SdkCriterionScore, SdkEvalError> {
        let actual = subject
            .latency_ms
            .ok_or(SdkEvalError::MissingSubjectField {
                field: "latency_ms",
            })?;
        let passed = actual <= self.maximum_ms;
        SdkCriterionScore::new("latency_at_most", f64::from(passed), passed)
            .map(|score| score.measurement("latency_ms", actual as f64))
    }
}

/// Require one usage measurement at or below one bound.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageAtMost {
    key: String,
    maximum: f64,
}

impl UsageAtMost {
    /// Create a bounded usage criterion for one measurement key.
    #[must_use]
    pub fn new(key: impl Into<String>, maximum: f64) -> Self {
        Self {
            key: key.into(),
            maximum,
        }
    }
}

impl SdkEvalCriterion for UsageAtMost {
    fn score(&self, subject: &SdkEvalSubject) -> Result<SdkCriterionScore, SdkEvalError> {
        let actual = subject
            .usage
            .get(&self.key)
            .copied()
            .ok_or(SdkEvalError::MissingSubjectField { field: "usage" })?;
        let passed = actual <= self.maximum;
        SdkCriterionScore::new(
            format!("usage_at_most:{}", self.key),
            f64::from(passed),
            passed,
        )
        .map(|score| score.measurement(self.key.clone(), actual))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentStepCount, LatencyAtMost, OutputContains, SdkCriterionScore, SdkEvalCriterion,
        SdkEvalError, SdkEvalSubject, SdkEvaluator, StructuredEquals, ToolTraceCount, UsageAtMost,
    };

    #[derive(Debug)]
    struct CustomCriterion;

    impl SdkEvalCriterion for CustomCriterion {
        fn score(&self, subject: &SdkEvalSubject) -> Result<SdkCriterionScore, SdkEvalError> {
            let passed = subject.metadata.get("quality") == Some(&serde_json::json!("good"));
            SdkCriterionScore::new("custom_quality", f64::from(passed), passed)
        }
    }

    #[test]
    fn evaluator_scores_every_sdk_subject_dimension_and_custom_criteria() {
        let subject = SdkEvalSubject::new("useful answer")
            .structured_value(serde_json::json!({"ok": true}))
            .tool_trace([serde_json::json!({"tool":"lookup"})])
            .agent_steps([
                serde_json::json!({"type":"model"}),
                serde_json::json!({"type":"tool"}),
            ])
            .latency_ms(25)
            .usage("total_tokens", 12.0)
            .metadata("quality", serde_json::json!("good"));
        let report = SdkEvaluator::new()
            .criterion(OutputContains::new("answer"))
            .criterion(StructuredEquals::new(serde_json::json!({"ok": true})))
            .criterion(ToolTraceCount::new(1))
            .criterion(AgentStepCount::new(2))
            .criterion(LatencyAtMost::new(30))
            .criterion(UsageAtMost::new("total_tokens", 20.0))
            .criterion(CustomCriterion)
            .evaluate(&subject)
            .expect("all dimensions score");

        assert!(report.passed);
        assert!((report.overall_score - 1.0).abs() < f64::EPSILON);
        assert_eq!(report.scores.len(), 7);
    }

    #[test]
    fn invalid_and_missing_scores_are_typed_errors() {
        assert!(matches!(
            SdkCriterionScore::new("bad", f64::NAN, false),
            Err(SdkEvalError::InvalidScore { .. })
        ));
        assert_eq!(
            SdkEvaluator::new()
                .criterion(StructuredEquals::new(serde_json::json!(null)))
                .evaluate(&SdkEvalSubject::new("text")),
            Err(SdkEvalError::MissingSubjectField {
                field: "structured_value"
            })
        );
    }
}

/// Current reproducible SDK evaluation-run schema version.
pub const SDK_EVAL_RUN_SCHEMA_VERSION: u32 = 1;
/// Canonical filename written for one SDK evaluation run.
pub const SDK_EVAL_RUN_FILENAME: &str = "sdk-eval.json";

/// Provenance required to reproduce and compare one SDK evaluation case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdkEvalProvenance {
    /// Stable dataset name.
    pub dataset_id: String,
    /// Dataset version or immutable digest.
    pub dataset_version: String,
    /// Stable case ID within the dataset.
    pub case_id: String,
    /// Provider ID when provider selection is relevant.
    #[serde(default)]
    pub provider_id: Option<String>,
    /// Model ID used by the evaluated operation.
    pub model_id: String,
    /// Stable configuration identity or immutable digest.
    pub config_id: String,
    /// Application-defined provenance such as git revision or prompt version.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl SdkEvalProvenance {
    fn validate(&self) -> Result<(), SdkEvalError> {
        for (field, value) in [
            ("dataset_id", self.dataset_id.as_str()),
            ("dataset_version", self.dataset_version.as_str()),
            ("case_id", self.case_id.as_str()),
            ("model_id", self.model_id.as_str()),
            ("config_id", self.config_id.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(SdkEvalError::InvalidProvenance { field });
            }
        }
        Ok(())
    }
}

/// Versioned evaluator identity required to reproduce scoring behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdkEvalRunConfig {
    /// Stable evaluator/scoring configuration ID.
    pub evaluator_id: String,
    /// Immutable evaluator version or digest.
    pub evaluator_version: String,
    /// Additional scoring configuration provenance.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl SdkEvalRunConfig {
    fn validate(&self) -> Result<(), SdkEvalError> {
        if self.evaluator_id.trim().is_empty() {
            return Err(SdkEvalError::InvalidRunConfig {
                field: "evaluator_id",
            });
        }
        if self.evaluator_version.trim().is_empty() {
            return Err(SdkEvalError::InvalidRunConfig {
                field: "evaluator_version",
            });
        }
        Ok(())
    }
}

/// One reproducible SDK evaluation case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SdkEvalCase {
    /// Complete case provenance.
    pub provenance: SdkEvalProvenance,
    /// Provider-independent material to score.
    pub subject: SdkEvalSubject,
}

/// One scored SDK evaluation case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SdkEvalCaseResult {
    /// Complete case provenance.
    pub provenance: SdkEvalProvenance,
    /// Aggregate criterion report.
    pub report: SdkEvalReport,
}

/// Persisted reproducible SDK evaluation run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SdkEvalRun {
    /// Run schema version.
    pub schema_version: u32,
    /// Application-assigned run ID.
    pub run_id: String,
    /// Stable SHA-256 digest of evaluator configuration, ordered provenance, and subjects.
    pub fingerprint: String,
    /// Versioned evaluator/scoring configuration.
    pub configuration: SdkEvalRunConfig,
    /// Case results in input order.
    pub cases: Vec<SdkEvalCaseResult>,
    /// Whether every case passed every criterion.
    pub passed: bool,
    /// Arithmetic mean of case overall scores.
    pub overall_score: f64,
}

/// Observable lifecycle event for one SDK evaluation run.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SdkEvalRunEvent {
    /// Run started.
    RunStarted {
        /// Run ID.
        run_id: String,
        /// Number of cases to evaluate.
        case_count: usize,
    },
    /// One case started.
    CaseStarted {
        /// Zero-based case index.
        index: usize,
        /// Complete case provenance.
        provenance: SdkEvalProvenance,
    },
    /// One case completed.
    CaseFinished {
        /// Zero-based case index.
        index: usize,
        /// Whether the case passed.
        passed: bool,
        /// Normalized case score.
        score: f64,
    },
    /// Run completed.
    RunFinished {
        /// Whether every case passed.
        passed: bool,
        /// Normalized run score.
        score: f64,
        /// Reproducibility fingerprint.
        fingerprint: String,
    },
}

/// Application observer for SDK evaluation lifecycle events.
pub trait SdkEvalObserver: Send + Sync {
    /// Observe one ordered run event.
    fn observe(&self, event: &SdkEvalRunEvent);
}

/// No-op observer for callers that do not need lifecycle events.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSdkEvalObserver;

impl SdkEvalObserver for NoopSdkEvalObserver {
    fn observe(&self, _event: &SdkEvalRunEvent) {}
}

impl SdkEvaluator {
    /// Evaluate ordered reproducible cases and emit ordered lifecycle events.
    ///
    /// The fingerprint excludes `run_id` and criterion results. It hashes schema version plus the
    /// complete ordered provenance and subjects, so rerunning identical inputs under another run
    /// ID produces the same fingerprint.
    ///
    /// # Errors
    ///
    /// Returns an error for empty/duplicate IDs, incomplete provenance, serialization failures, or
    /// criterion failures.
    pub fn run(
        &self,
        run_id: impl Into<String>,
        configuration: &SdkEvalRunConfig,
        cases: &[SdkEvalCase],
        observer: &dyn SdkEvalObserver,
    ) -> Result<SdkEvalRun, SdkEvalError> {
        let run_id = run_id.into();
        if run_id.trim().is_empty() {
            return Err(SdkEvalError::InvalidRunId);
        }
        configuration.validate()?;
        let mut case_ids = std::collections::BTreeSet::new();
        for case in cases {
            case.provenance.validate()?;
            if !case_ids.insert(case.provenance.case_id.clone()) {
                return Err(SdkEvalError::DuplicateCaseId {
                    case_id: case.provenance.case_id.clone(),
                });
            }
        }
        observer.observe(&SdkEvalRunEvent::RunStarted {
            run_id: run_id.clone(),
            case_count: cases.len(),
        });
        let mut results = Vec::with_capacity(cases.len());
        for (index, case) in cases.iter().enumerate() {
            observer.observe(&SdkEvalRunEvent::CaseStarted {
                index,
                provenance: case.provenance.clone(),
            });
            let report = self.evaluate(&case.subject)?;
            observer.observe(&SdkEvalRunEvent::CaseFinished {
                index,
                passed: report.passed,
                score: report.overall_score,
            });
            results.push(SdkEvalCaseResult {
                provenance: case.provenance.clone(),
                report,
            });
        }
        let passed = results.iter().all(|case| case.report.passed);
        let overall_score = if results.is_empty() {
            0.0
        } else {
            results
                .iter()
                .map(|case| case.report.overall_score)
                .sum::<f64>()
                / results.len() as f64
        };
        let fingerprint = sdk_eval_fingerprint(configuration, cases)?;
        observer.observe(&SdkEvalRunEvent::RunFinished {
            passed,
            score: overall_score,
            fingerprint: fingerprint.clone(),
        });
        Ok(SdkEvalRun {
            schema_version: SDK_EVAL_RUN_SCHEMA_VERSION,
            run_id,
            fingerprint,
            configuration: configuration.clone(),
            cases: results,
            passed,
            overall_score,
        })
    }
}

fn sdk_eval_fingerprint(
    configuration: &SdkEvalRunConfig,
    cases: &[SdkEvalCase],
) -> Result<String, SdkEvalError> {
    use sha2::{Digest, Sha256};

    #[derive(Serialize)]
    struct FingerprintInput<'a> {
        schema_version: u32,
        configuration: &'a SdkEvalRunConfig,
        cases: &'a [SdkEvalCase],
    }
    let encoded = serde_json::to_vec(&FingerprintInput {
        schema_version: SDK_EVAL_RUN_SCHEMA_VERSION,
        configuration,
        cases,
    })
    .map_err(|error| SdkEvalError::Serialization(error.to_string()))?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

/// Atomically persist one SDK evaluation run in a directory.
///
/// # Errors
///
/// Returns an error when the run is invalid or cannot be encoded, written, or atomically renamed.
pub fn write_sdk_eval_run(
    directory: impl AsRef<std::path::Path>,
    run: &SdkEvalRun,
) -> Result<std::path::PathBuf, SdkEvalError> {
    if run.schema_version != SDK_EVAL_RUN_SCHEMA_VERSION {
        return Err(SdkEvalError::UnsupportedRunSchema {
            actual: run.schema_version,
        });
    }
    let directory = directory.as_ref();
    std::fs::create_dir_all(directory).map_err(|error| SdkEvalError::Io(error.to_string()))?;
    let path = directory.join(SDK_EVAL_RUN_FILENAME);
    let temporary = directory.join(format!(".{SDK_EVAL_RUN_FILENAME}.tmp"));
    let bytes = serde_json::to_vec_pretty(run)
        .map_err(|error| SdkEvalError::Serialization(error.to_string()))?;
    std::fs::write(&temporary, bytes).map_err(|error| SdkEvalError::Io(error.to_string()))?;
    std::fs::rename(&temporary, &path).map_err(|error| SdkEvalError::Io(error.to_string()))?;
    Ok(path)
}

/// Load one SDK evaluation run from a directory or JSON path.
///
/// # Errors
///
/// Returns an error when the artifact cannot be read/decoded or uses an unsupported schema.
pub fn load_sdk_eval_run(path: impl AsRef<std::path::Path>) -> Result<SdkEvalRun, SdkEvalError> {
    let path = path.as_ref();
    let path = if path.is_dir() {
        path.join(SDK_EVAL_RUN_FILENAME)
    } else {
        path.to_path_buf()
    };
    let bytes = std::fs::read(path).map_err(|error| SdkEvalError::Io(error.to_string()))?;
    let run = serde_json::from_slice::<SdkEvalRun>(&bytes)
        .map_err(|error| SdkEvalError::Serialization(error.to_string()))?;
    if run.schema_version != SDK_EVAL_RUN_SCHEMA_VERSION {
        return Err(SdkEvalError::UnsupportedRunSchema {
            actual: run.schema_version,
        });
    }
    Ok(run)
}

#[cfg(test)]
mod run_tests {
    use super::{
        NoopSdkEvalObserver, OutputContains, SDK_EVAL_RUN_SCHEMA_VERSION, SdkEvalCase,
        SdkEvalObserver, SdkEvalProvenance, SdkEvalRunConfig, SdkEvalRunEvent, SdkEvalSubject,
        SdkEvaluator, load_sdk_eval_run, write_sdk_eval_run,
    };
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct RecordingObserver(Mutex<Vec<SdkEvalRunEvent>>);

    impl SdkEvalObserver for RecordingObserver {
        fn observe(&self, event: &SdkEvalRunEvent) {
            self.0.lock().expect("observer lock").push(event.clone());
        }
    }

    fn config() -> SdkEvalRunConfig {
        SdkEvalRunConfig {
            evaluator_id: "quality".to_string(),
            evaluator_version: "sha256:evaluator".to_string(),
            metadata: std::collections::BTreeMap::new(),
        }
    }

    fn case(id: &str) -> SdkEvalCase {
        SdkEvalCase {
            provenance: SdkEvalProvenance {
                dataset_id: "dataset".to_string(),
                dataset_version: "sha256:fixture".to_string(),
                case_id: id.to_string(),
                provider_id: Some("provider".to_string()),
                model_id: "model".to_string(),
                config_id: "config:v1".to_string(),
                metadata: std::collections::BTreeMap::from([(
                    "git_revision".to_string(),
                    "abc123".to_string(),
                )]),
            },
            subject: SdkEvalSubject::new("accepted answer"),
        }
    }

    #[test]
    fn reproducible_runs_preserve_provenance_events_and_fingerprint() {
        let evaluator = SdkEvaluator::new().criterion(OutputContains::new("answer"));
        let cases = [case("case-a"), case("case-b")];
        let observer = RecordingObserver::default();
        let first = evaluator
            .run("run-a", &config(), &cases, &observer)
            .expect("first run");
        let second = evaluator
            .run("run-b", &config(), &cases, &NoopSdkEvalObserver)
            .expect("second run");
        assert_eq!(first.schema_version, SDK_EVAL_RUN_SCHEMA_VERSION);
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.cases[0].provenance, cases[0].provenance);
        assert!(first.passed);
        let events = observer.0.lock().expect("observer lock");
        assert!(matches!(
            events.first(),
            Some(SdkEvalRunEvent::RunStarted { .. })
        ));
        assert!(matches!(
            events.last(),
            Some(SdkEvalRunEvent::RunFinished { .. })
        ));
        assert_eq!(events.len(), 6);
        drop(events);
    }

    #[test]
    fn run_artifact_round_trips_atomically() {
        let evaluator = SdkEvaluator::new().criterion(OutputContains::new("answer"));
        let run = evaluator
            .run(
                "persisted",
                &config(),
                &[case("case-a")],
                &NoopSdkEvalObserver,
            )
            .expect("run");
        let directory =
            std::env::temp_dir().join(format!("bcode-sdk-eval-run-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let path = write_sdk_eval_run(&directory, &run).expect("write run");
        assert_eq!(load_sdk_eval_run(&directory).expect("directory load"), run);
        assert_eq!(load_sdk_eval_run(path).expect("file load"), run);
        std::fs::remove_dir_all(directory).expect("cleanup run");
    }
}
