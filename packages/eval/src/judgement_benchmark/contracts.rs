//! Version-one portable capture and report contracts. Unknown fields fail closed.
use bcode_model::judgement::Feature;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// All features are reported separately, including features with no scheduled work.
pub const FEATURES: [Feature; 6] = [
    Feature::ToolResultTriage,
    Feature::ContextReranking,
    Feature::Recommendations,
    Feature::ProgressMonitoring,
    Feature::SessionCategorization,
    Feature::EvalCategorization,
];

/// Independently authored schedule, oracle and gates, frozen before captures.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkPlan {
    /// Only version 1 is supported.
    pub schema_version: u32,
    /// Nonempty immutable corpus/review identity, not a provider answer script.
    pub oracle_id: String,
    /// Nonempty preregistration identity shared by captures.
    pub preregistration_id: String,
    /// Independently scored tasks. IDs are globally unique.
    pub tasks: Vec<BenchmarkTask>,
    /// Gates selected before observing outcomes.
    pub gates: Gates,
}

/// One independent task cluster with a known scheduled repetition count.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkTask {
    /// Stable task identity, shared by both arms.
    pub task_id: String,
    /// Exactly one enhancement under test; other enhancements are disabled.
    pub feature: Feature,
    /// Scheduled repetitions indexed from zero; missing attempts remain in denominator.
    pub repetitions: u32,
    /// Independent oracle authored without seeing this run's outputs.
    pub oracle: Oracle,
}

/// Ground truth is semantic and independent of model scores.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Oracle {
    /// A label vocabulary with graded credit; absence earns the explicit abstention credit.
    Classification {
        /// Valid labels and their independently assigned credit in [0, 1].
        labels: BTreeMap<String, f64>,
        /// Appropriate abstention credit in [0, 1].
        abstention_credit: f64,
    },
    /// NDCG at k plus admission validity; tied relevance is unconstrained.
    Ranking {
        /// Every supplied candidate, with relevance in [0, 4].
        relevance: BTreeMap<String, f64>,
        /// Ranking cutoff, positive and no larger than candidate count.
        k: usize,
        /// Candidate costs in the same domain-neutral units as budget.
        costs: BTreeMap<String, u64>,
        /// Maximum admitted candidate cost.
        budget: u64,
        /// Independently solved maximum feasible admitted relevance gain under this budget.
        /// Used to score actual admission separately from ranking; zero means no useful admission.
        ideal_admission_gain: f64,
        /// Required candidate IDs which must be admitted.
        required: Vec<String>,
        /// Protected fixed ranking positions (zero based).
        protected_positions: BTreeMap<String, usize>,
    },
    /// Event detection relative to a bounded independently labeled observation window.
    Progress {
        /// Expected advice label; None requires abstention (including productive controls).
        label: Option<String>,
        /// Earliest legitimate event timestamp relative to trial start.
        onset_ms: u64,
        /// Latest acceptable detection timestamp; no detection scores zero on positives.
        deadline_ms: u64,
    },
}

/// Captured final consumer result, not the provider response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConsumerOutput {
    /// Final label or abstention.
    Classification { label: Option<String> },
    /// Complete final order and actual constrained admission.
    Ranking {
        order: Vec<String>,
        admitted: Vec<String>,
    },
    /// First matching consumer advice timestamp, not provider response time.
    Progress {
        label: Option<String>,
        detected_ms: Option<u64>,
    },
}

/// Independently isolated on/off treatment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arm {
    Off,
    On,
}

/// Origin of evidence. Only live actual-consumer execution qualifies for live comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    LiveConsumer,
    OfflineConsumer,
    ScorerCalibration,
}

/// Reliability is measured independently of quality and advisory outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOutcome {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    InfrastructureFailure,
}

/// Verification status of one independently observed consumer fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    /// Public capture establishes the fact.
    Confirmed,
    /// Public capture contradicts the fact.
    Rejected,
    /// Capture cannot establish the fact; never treated as confirmed.
    Unknown,
}
impl From<bool> for EvidenceStatus {
    fn from(value: bool) -> Self {
        if value {
            Self::Confirmed
        } else {
            Self::Rejected
        }
    }
}

/// Credential-free attestation to captured public execution facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerEvidence {
    /// Capture origin; synthetic/provider-only answers cannot qualify.
    pub source: EvidenceSource,
    /// Bounded public operation identity.
    pub operation: String,
    /// Opaque bounded capture locator; no secrets or raw provider metadata.
    pub capture_id: String,
    /// Capture observed actual consumer application, not just inference.
    pub consumer_executed: EvidenceStatus,
    /// Exactly the tested feature differs between arms.
    pub isolated_feature: EvidenceStatus,
    /// Same immutable inputs and execution settings, excluding this feature switch.
    pub pairing_id: String,
    /// Actual optional discovery calls.
    pub discovery_calls: u64,
    /// Actual optional verification calls (may incur cost).
    pub verification_calls: u64,
    /// Actual optional inference calls.
    pub inference_calls: u64,
    /// Canonical outcomes/permissions were not changed by advisory labels.
    pub authority_preserved: EvidenceStatus,
    /// No additional implicit persistence of request-only judgement input.
    pub request_only: EvidenceStatus,
}

/// One arm of a scheduled trial. Missing measurements are not zero.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkObservation {
    /// Version 1 only.
    pub schema_version: u32,
    /// Frozen preregistration identity.
    pub preregistration_id: String,
    /// Task identity.
    pub task_id: String,
    /// Zero-based scheduled repetition.
    pub repetition: u32,
    /// Feature switch under test.
    pub arm: Arm,
    /// Actual execution evidence.
    pub evidence: ConsumerEvidence,
    /// Terminal execution outcome, independent of semantic score.
    pub outcome: TerminalOutcome,
    /// Final bounded output; None means unavailable, not abstention.
    pub output: Option<ConsumerOutput>,
    /// Full trial wall latency; None means not measured.
    pub latency_ms: Option<f64>,
    /// Total trial USD cost including verification, retries and ordinary model work.
    /// None if pricing or any billed component is unknown; never assume missing usage is free.
    pub cost_usd: Option<f64>,
}

/// Preregistered bounded deterministic uncertainty settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapSettings {
    /// Two-sided confidence level in [0.8, 0.99].
    pub confidence_level: f64,
    /// Replicate count in [200, 10000].
    pub replicates: usize,
    /// At least five independent task clusters; repeats cannot satisfy this minimum.
    pub minimum_tasks: usize,
}
impl Default for BootstrapSettings {
    fn default() -> Self {
        Self {
            confidence_level: 0.95,
            replicates: 2000,
            minimum_tasks: 5,
        }
    }
}

/// Independently preregistered critical limits for one feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureLimits {
    /// Feature receiving these limits; duplicates are rejected.
    pub feature: Feature,
    /// Required positive lower confidence bound for quality utility gain.
    pub quality_gain: f64,
    /// Maximum tolerated mean quality loss (regression diagnostic).
    pub quality_loss: f64,
    /// Maximum tolerated upper confidence bound for latency increase.
    pub latency_increase_ms: f64,
    /// Maximum tolerated upper confidence bound for cost increase.
    pub cost_increase_usd: f64,
    /// Maximum tolerated reliability loss; lower CI must meet this for improvement.
    pub reliability_loss: f64,
}

/// Preregistered descriptive gates; never selected from observed outcomes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gates {
    /// Preregistered task-cluster bootstrap settings.
    #[serde(default)]
    pub bootstrap: BootstrapSettings,
    /// Feature-specific critical limits. Missing limits prohibit improvement claims.
    #[serde(default)]
    pub feature_limits: Vec<FeatureLimits>,
    /// Minimum independent task clusters per feature.
    pub minimum_tasks: usize,
    /// Required complete paired quality fraction of scheduled trials, in (0, 1].
    pub minimum_completeness: f64,
    /// Required task-balanced mean quality gain, positive.
    pub quality_gain: f64,
    /// Allowed mean quality loss, nonnegative.
    pub quality_loss: f64,
    /// Maximum acceptable positive latency delta in milliseconds.
    pub latency_increase_ms: f64,
    /// Maximum acceptable positive USD cost delta.
    pub cost_increase_usd: f64,
    /// Maximum acceptable task-balanced reliability decrease in [0, 1].
    pub reliability_loss: f64,
}

/// Per-arm oracle scoring. Invalid outputs score zero, unavailable outputs remain unknown.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputScore {
    /// Normalized semantic utility in [0, 1].
    pub quality: f64,
    /// Structural/admission constraints met.
    pub constraints_passed: bool,
    /// Observed positive detection delay; None for absence, false positives or invalid events.
    pub detection_delay_ms: Option<u64>,
}

/// Descriptive task-cluster comparison with explicit measurement coverage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairedMetric {
    /// Number of scheduled pairs with both measurements known.
    pub pairs: usize,
    /// Independent tasks contributing at least one known pair.
    pub tasks: usize,
    /// Average within task first, then equal-weight average across tasks (on minus off).
    pub mean_delta: Option<f64>,
    /// Two-sided task-cluster percentile bootstrap interval; None below minimum tasks.
    /// Describes this task corpus only, not six-feature or population significance.
    pub confidence_interval: Option<[f64; 2]>,
    /// Range of task mean deltas, not a confidence interval.
    pub task_delta_range: Option<[f64; 2]>,
}

/// Gates are descriptive evidence decisions, not statistical significance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Improved,
    Tradeoff,
    Regressed,
    Inconclusive,
}

/// Independent feature report; cross-feature averaging is intentionally absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureReport {
    /// Enhancement measured.
    pub feature: Feature,
    /// Number of independent scheduled tasks.
    pub scheduled_tasks: usize,
    /// All scheduled paired trials, including missing/infrastructure failures.
    pub scheduled_pairs: usize,
    /// Expected arm observations, twice `scheduled_pairs`.
    pub scheduled_arms: usize,
    /// Uniquely captured arm observations, regardless of eligibility.
    pub observed_arms: usize,
    /// Observations satisfying live execution constraints.
    pub live_arms: usize,
    /// Successful live outcomes over scheduled arms; unavailable denominator is not erased.
    pub successful_arms: usize,
    /// Evaluated quality pairs / scheduled pairs, unknown for empty schedule.
    pub completeness: Option<f64>,
    /// Paired task-balanced quality.
    pub quality: PairedMetric,
    /// Paired full-trial latency, measured across all terminal outcomes.
    pub latency_ms: PairedMetric,
    /// Paired total cost, unknown prices excluded with explicit coverage.
    pub cost_usd: PairedMetric,
    /// Paired success indicators; failed outcomes remain in this denominator.
    pub reliability: PairedMetric,
    /// Paired positive progress detection delays, conditional on both detecting.
    pub detection_delay_ms: PairedMetric,
    /// Per-arm confusion, coverage, ranking components and progress event counts.
    pub diagnostics: Vec<super::ArmDiagnostics>,
    /// Descriptive preregistered gate outcome.
    pub verdict: Verdict,
    /// Bounded identity/reason strings for missing, invalid or incomplete evidence.
    pub gaps: Vec<String>,
}

/// Versioned independent report. Source-ready does not assert live model improvement.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkReport {
    /// Report version 3 (per-arm semantic diagnostic counts and component metrics).
    pub schema_version: u32,
    /// Frozen oracle provenance.
    pub oracle_id: String,
    /// Frozen gate provenance.
    pub preregistration_id: String,
    /// Gates retained for audit.
    pub gates: Gates,
    /// Independent schedule and oracles retained so scoring is auditable.
    pub tasks: Vec<BenchmarkTask>,
    /// Raw bounded observations retained, including duplicates and failed attempts.
    pub observations: Vec<BenchmarkObservation>,
    /// Six independent reports.
    pub features: Vec<FeatureReport>,
}
