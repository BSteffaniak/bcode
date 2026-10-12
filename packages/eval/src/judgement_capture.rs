//! Bounded public-consumer capture for independently scheduled judgement trials.
//!
//! This module neither starts daemons nor resolves credentials. The caller owns isolated
//! treatment configuration and independent ground truth. A public response without telemetry
//! is useful raw evidence but cannot be promoted into a complete benchmark observation.

mod decision;
pub use decision::{AnswerShape, DecisionCapture, DecisionStage, QuestionShape, RequestShape};
mod jobs;
pub use jobs::{
    CaptureComparison, CaptureJob, ComparisonJob, compare_capture_job, decode_capture_job,
    execute_capture_job,
};
mod raw;
pub use raw::{RawCaptureScore, raw_capture_scores};
mod report;
pub use report::{compare_captures, compare_captures_quality_only};
mod normal_eval;
mod turns;
/// Run one actual eval attempt in a separately selected CLI process, never a daemon runtime.
///
/// # Errors
/// Rejects incomplete identities, unsafe paths, incompatible suites, and missing attempt evidence.
pub fn capture_normal_eval_run(job: CaptureJob) -> Result<ConsumerCapture, &'static str> {
    let ConsumerRequest::EvalRun {
        suite_path,
        output_dir,
        subject_binary,
        artifact_root,
    } = job.consumer
    else {
        return Err("normal eval consumer required");
    };
    if job.schema_version != 1
        || !job.allow_live
        || job.request.source != crate::judgement_benchmark::EvidenceSource::LiveConsumer
        || job.request.feature != Feature::EvalCategorization
        || job.request.timeout_ms == 0
        || job.request.timeout_ms > 300_000
    {
        return Err("explicit live eval job required");
    }
    let mut capture = ConsumerCapture {
        schema_version: CAPTURE_SCHEMA_VERSION,
        scope: CaptureScope::NormalEvalAttempt,
        request: job.request,
        operation: "eval.run.actual_attempt".into(),
        status: CaptureStatus::ConsumerUnavailable,
        output: None,
        consumer_executed: false,
        outcome: TerminalOutcome::InfrastructureFailure,
        operation_latency_ms: None,
        trial_latency_ms: None,
        authority_preserved: None,
        request_only: None,
        telemetry: CaptureTelemetry::default(),
    };
    if normal_eval::execute(
        &suite_path,
        &output_dir,
        &subject_binary,
        &artifact_root,
        &mut capture,
    )
    .is_err()
    {
        // Preserve stage and any verified partial facts; unavailable telemetry remains unknown.
        capture.status = CaptureStatus::PublicOperationFailed;
    }
    if serde_json::to_vec(&capture)
        .map_err(|_| "invalid eval capture output")?
        .len()
        > MAX_CAPTURE_BYTES
    {
        return Err("eval capture output exceeds bound");
    }
    Ok(capture)
}
pub use turns::NormalTurnRequest;
mod services;
pub use services::{CategorizationDimension, ServiceConsumer};

use crate::judgement_benchmark::{
    Arm, BenchmarkObservation, ConsumerEvidence, ConsumerOutput, EvidenceSource, TerminalOutcome,
};
use bcode_client::BcodeClient;
use bcode_model::judgement::Feature;
use bcode_session_models::{ContextSelectionRequest, SessionId};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Version two adds content-free decision evidence; version one reads with empty decision stages.
/// Older readers must reject version two rather than assume complete decision coverage.
pub const CAPTURE_SCHEMA_VERSION: u32 = 2;
/// Maximum serialized input/output envelope; source input is never retained in the capture.
pub const MAX_CAPTURE_BYTES: usize = 128 * 1024;

/// Verification caches and model caches are distinct; neither is inferred from repetition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheCondition {
    /// Verified fresh subject/state/cache context.
    Cold,
    /// Verified reuse within the recorded context.
    Warm,
    /// Cache lifecycle was not observed.
    Unknown,
}

/// Exact secret-free subject provenance. Config contents and credential values are forbidden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectIdentity {
    /// Absolute selected subject executable, not a PATH lookup or worker-control replacement.
    pub executable: String,
    /// SHA-256 of that executable.
    pub executable_sha256: String,
    /// Exact daemon artifact identity verified by the hosting harness.
    pub artifact_id: String,
    /// Digest of resolved non-secret configuration semantics; never credential file contents.
    pub configuration_sha256: String,
    /// Whether all layers and request overrides were included in that configuration identity.
    pub configuration_complete: bool,
    /// Effective provider observed at execution, not inferred from a configuration filename.
    pub provider: Option<String>,
    /// Effective concrete model observed at execution, not a profile alias.
    pub model: Option<String>,
    /// Non-secret account reference, when selected.
    pub auth_profile: Option<String>,
    /// Selected model profile reference, when used.
    pub model_profile: Option<String>,
    /// Isolated subject state identity, never a database path to inspect.
    pub state_id: String,
}

/// Normalized per-call facts; covers verification, discovery, inference and ordinary model work.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallMeasurement {
    /// Domain-generic operation identity, never provider wire data.
    pub operation: String,
    /// Retry attempt, zero-based; retries remain separately accounted.
    pub attempt: u32,
    /// Actual provider/model identities, if observed.
    pub provider: Option<String>,
    /// Concrete model identity, if observed.
    pub model: Option<String>,
    /// Measured duration, not trial wall time.
    pub duration_ms: Option<f64>,
    /// Public normalized terminal outcome.
    pub outcome: TerminalOutcome,
    /// Provider-reported input usage, absent when unavailable.
    pub input_tokens: Option<u64>,
    /// Provider-reported output usage, absent when unavailable.
    pub output_tokens: Option<u64>,
    /// Provider-reported cached input usage.
    pub cached_input_tokens: Option<u64>,
    /// Pricing identity/version used to calculate this call's USD cost.
    pub pricing_id: Option<String>,
    /// Full call charge including non-token billing; unknown is not free.
    pub cost_usd: Option<f64>,
}

/// Nullable facts are preserved until the public execution boundary can supply them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureTelemetry {
    /// Content-free request/score/application evidence; older captures contain no stages.
    #[serde(default)]
    pub decisions: DecisionCapture,
    /// All calls, including verification, retries and ordinary model work, observed.
    pub calls_complete: bool,
    /// Complete optional discovery count.
    pub discovery_calls: Option<u64>,
    /// Complete optional verification count.
    pub verification_calls: Option<u64>,
    /// Complete optional inference count.
    pub inference_calls: Option<u64>,
    /// Verified cache hit; not inferred from zero calls.
    pub verification_cache_hit: Option<bool>,
    /// Raw normalized central operation facts; service attempts are not HTTP retries.
    /// Verification billing and whole-task cost remain unknown unless explicitly reported.
    #[serde(default)]
    pub optional_operation: Option<bcode_model::judgement::OptionalOperationObservation>,
    /// At least one executed optional operation lacked central facts; totals remain unknown.
    #[serde(default)]
    pub optional_counts_incomplete: bool,
    /// Producer explicitly closed observation of all optional operations for this turn.
    /// A connected stream or a successful conversational turn alone cannot establish this.
    #[serde(default)]
    pub optional_operations_complete: bool,
    /// All normalized optional operations, including every trajectory prefix.
    #[serde(default)]
    pub optional_operations: Vec<bcode_model::judgement::OptionalOperationObservation>,
    /// V2 exact bounded producer envelope when passive operations were aggregated.
    /// Retains totals, qualifiers and authenticated session/turn correlation, not guessed counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregated_turn_observation: Option<bcode_model::judgement::TurnJudgementObservation>,
    /// Bounded call ledger; absence does not mean zero calls.
    pub calls: Vec<CallMeasurement>,
}

impl CaptureTelemetry {
    /// Retain normalized operation facts from a public response. No provider data is persisted.
    /// Service invocations cannot establish HTTP retry, full billing or whole-task coverage.
    pub fn retain_optional_response(
        &mut self,
        response: &bcode_model::judgement::OptionalResponse,
    ) {
        // Retaining a new operation invalidates any previously closed observation window.
        self.optional_operations_complete = false;
        if self.optional_operations.len() >= 1024 {
            self.optional_counts_incomplete = true;
            self.discovery_calls = None;
            self.verification_calls = None;
            self.inference_calls = None;
            return;
        }
        if let Some(observation) = &response.observation {
            if self.optional_operations.is_empty() && !self.optional_counts_incomplete {
                self.discovery_calls = Some(0);
                self.verification_calls = Some(0);
                self.inference_calls = Some(0);
            }
            self.discovery_calls = self
                .discovery_calls
                .and_then(|count| count.checked_add(observation.discovery_invocations));
            self.verification_calls = self
                .verification_calls
                .and_then(|count| count.checked_add(observation.verification_invocations));
            self.inference_calls = self
                .inference_calls
                .and_then(|count| count.checked_add(observation.inference_invocations));
            self.optional_operations.push(observation.clone());
            self.verification_cache_hit = Some(observation.verification_cache_hit);
            self.optional_operation = Some(observation.clone());
            // Deliberately do not mark the call ledger complete: listing, provider transport
            // retries, verification billing and ordinary model work are separate coverage.
        } else {
            self.optional_counts_incomplete = true;
            self.discovery_calls = None;
            self.verification_calls = None;
            self.inference_calls = None;
        }
    }

    /// Retain additive producer telemetry; silence is unknown, never a disabled zero-call baseline.
    pub(super) fn retain_service_response(
        &mut self,
        response: &serde_json::Value,
    ) -> Result<(), ()> {
        if let Some(value) = response.get("observation").filter(|value| !value.is_null()) {
            let observation: bcode_model::judgement::OptionalOperationObservation =
                serde_json::from_value(value.clone()).map_err(|_| ())?;
            if let Some(projection) = &observation.decision {
                self.decisions
                    .observe_projection(projection, observation.outcome);
            } else {
                self.decisions
                    .observe_service(Some(observation.outcome), None);
            }
            if let Some(applied) = response.get("applied").and_then(serde_json::Value::as_bool) {
                self.decisions.applied(Some(applied));
            }
            self.retain_optional_response(&bcode_model::judgement::OptionalResponse {
                response: None,
                observation: Some(observation),
            });
        } else {
            self.decisions.observe_service(
                None,
                response.get("applied").and_then(serde_json::Value::as_bool),
            );
            self.retain_optional_response(&bcode_model::judgement::OptionalResponse {
                response: None,
                observation: None,
            });
        }
        Ok(())
    }

    /// Retain the implemented eval-attempt producer, never a normal-turn coverage claim.
    ///
    /// # Errors
    /// Rejects incompatible or internally inconsistent eval evidence.
    pub fn retain_eval_observation(
        &mut self,
        observation: &crate::diagnosis::EvalAttemptJudgementObservation,
    ) -> Result<(), &'static str> {
        observation.validate()?;
        for inference in &observation.operation.inference {
            self.retain_optional_response(&bcode_model::judgement::OptionalResponse {
                response: None,
                observation: Some(inference.clone()),
            });
        }
        // Explicit eval closure applies only after all retained inference facts.
        self.optional_operations_complete =
            observation.complete && !self.optional_counts_incomplete;
        if !self.optional_operations_complete {
            self.optional_counts_incomplete = true;
            self.discovery_calls = None;
            self.verification_calls = None;
            self.inference_calls = None;
        } else if observation.operation.inference.is_empty() && !self.optional_counts_incomplete {
            self.discovery_calls = Some(0);
            self.verification_calls = Some(0);
            self.inference_calls = Some(0);
        }
        self.decisions.applied(Some(
            observation.operation.application
                == Some(bcode_model::judgement::ApplicationDisposition::Applied),
        ));
        Ok(())
    }

    /// Read normalized product evidence without promoting partial or legacy coverage.
    pub(super) fn retain_turn_observation(
        &mut self,
        observation: &bcode_model::judgement::TurnJudgementObservation,
    ) -> Result<(), ()> {
        if observation.scope != bcode_model::judgement::ObservationScope::NormalTurn {
            return Err(());
        }
        observation.validate().map_err(|_| ())?;
        if !observation.passive_aggregates.is_empty() {
            self.aggregated_turn_observation = Some(observation.clone());
        }
        for operation in &observation.operations {
            for inference in &operation.inference {
                if let Some(projection) = &inference.decision {
                    self.decisions
                        .observe_projection(projection, inference.outcome);
                    self.decisions
                        .applied(operation.application.map(|application| {
                            application == bcode_model::judgement::ApplicationDisposition::Applied
                        }));
                }
                self.retain_optional_response(&bcode_model::judgement::OptionalResponse {
                    response: None,
                    observation: Some(inference.clone()),
                });
            }
        }
        // Counts of observed operations are not complete turn totals until explicit closure.
        self.optional_operations_complete = observation.complete;
        if !observation.complete {
            self.optional_counts_incomplete = true;
            self.discovery_calls = None;
            self.verification_calls = None;
            self.inference_calls = None;
        } else if observation
            .operations
            .iter()
            .all(|operation| operation.inference.is_empty())
        {
            self.discovery_calls = Some(0);
            self.verification_calls = Some(0);
            self.inference_calls = Some(0);
        }
        Ok(())
    }

    /// Total measured trial charge only when every potentially billed call is accounted.
    pub fn total_cost_usd(&self) -> Option<f64> {
        if !self.calls_complete {
            return None;
        }
        let mut sum = 0.0;
        for call in &self.calls {
            let cost = call.cost_usd?;
            if !cost.is_finite()
                || cost < 0.0
                || call.pricing_id.as_ref().is_none_or(String::is_empty)
            {
                return None;
            }
            sum += cost;
        }
        sum.is_finite().then_some(sum)
    }
}

/// Caller-owned trial identity and verified isolation, separate from semantic ground truth.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequest {
    /// Frozen preregistration identity.
    pub preregistration_id: String,
    /// Independent task identity.
    pub task_id: String,
    /// Zero-based repetition.
    pub repetition: u32,
    /// Single tested enhancement.
    pub feature: Feature,
    /// Requested treatment; capture does not mutate configuration to enact it.
    pub arm: Arm,
    /// Raw source; live selection requires explicit opt-in from the outer harness.
    pub source: EvidenceSource,
    /// Stable unique public capture locator.
    pub capture_id: String,
    /// Same immutable inputs/settings except treatment.
    pub pairing_id: String,
    /// Observed feature isolation, None when not verified.
    pub isolated_feature: Option<bool>,
    /// Exact subject provenance, independently verified by the outer harness.
    pub subject: SubjectIdentity,
    /// Observed cache lifecycle.
    pub cache_condition: CacheCondition,
    /// Positive bounded wall-time budget in milliseconds, at most five minutes.
    pub timeout_ms: u64,
}

/// Supported public consumer inputs; no provider answer or oracle is supplied here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConsumerRequest {
    /// Observes actual automatic consumers downstream of one admitted normal turn.
    NormalTurn { request: Box<NormalTurnRequest> },
    /// Executes a real producer algorithm through bounded public plugin routing.
    Service { consumer: ServiceConsumer },
    /// Runs actual application context ranking/admission over bounded original evidence.
    Context { request: ContextSelectionRequest },
    /// Runs the same eval diagnosis request/attachment adapter used by workflow capture.
    EvalDiagnosis {
        /// Independently observed oracle facts; no prompt or raw tool output.
        oracle: serde_json::Value,
        /// Existing deterministic diagnosis, independent of advisory model labels.
        diagnosis: serde_json::Value,
    },
    /// Actual single-attempt workflow evaluation; dispatched only by an isolated CLI process.
    EvalRun {
        /// Authored suite confined to the caller's authorized external artifact root.
        suite_path: std::path::PathBuf,
        /// Fresh output directory under that root.
        output_dir: std::path::PathBuf,
        /// Explicit pinned subject executable.
        subject_binary: std::path::PathBuf,
        /// Explicit authorized artifact root; confinement does not grant access.
        artifact_root: std::path::PathBuf,
    },
    /// Reads existing disposable labels; does not claim categorization trigger execution.
    SessionCategorization {
        /// Existing harness-owned session only.
        session_id: SessionId,
        /// Activation sequence expected by the harness; stale results are unavailable.
        source_sequence: u64,
    },
    /// Explicit unsupported consumer, never substituted with generic provider inference.
    Unavailable,
}

/// Distinguishes absence/fallback from consumer unavailability and transport failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureStatus {
    /// Consumer returned a final result (possibly a semantically ordinary absence).
    Captured,
    /// Consumer retained baseline; public API did not expose the fallback reason.
    FallbackReasonUnknown,
    /// No consumer trigger/adapter is available.
    ConsumerUnavailable,
    /// Public transport or response validation failed; error bodies are not retained.
    PublicOperationFailed,
    /// Bounded request timed out; server-side effect/cancellation status remains unknown.
    TimedOut,
}

/// Which execution boundary was actually measured; service trials are not automatic triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureScope {
    /// Public application consumer, without claiming whole-task efficiency.
    PublicOperation,
    /// Plugin consumer algorithm directly invoked with bounded supplied observations.
    ServiceMicrobenchmark,
    /// Automatic consumers observed downstream of a real admitted normal turn.
    NormalTurn,
    /// Optional diagnosis attached after an actual independently graded workflow eval attempt.
    NormalEvalAttempt,
}

/// Raw bounded artifact. Input and error bodies are deliberately not persisted here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerCapture {
    /// Capture compatibility boundary.
    pub schema_version: u32,
    /// Actual execution boundary, independent of source live/offline status.
    pub scope: CaptureScope,
    /// Trial and exact subject identity.
    pub request: CaptureRequest,
    /// Actual public operation, never a configured operation masquerading as execution.
    pub operation: String,
    /// Explicit availability/fallback status.
    pub status: CaptureStatus,
    /// Public consumer output; unavailable is None, not abstention.
    pub output: Option<ConsumerOutput>,
    /// True only for a trigger execution, not a disposable-label read.
    pub consumer_executed: bool,
    /// Terminal outcome independent of optional advice quality.
    pub outcome: TerminalOutcome,
    /// Measured public operation wall time, not a full ordinary-session trial.
    pub operation_latency_ms: Option<f64>,
    /// Full trial duration measured by outer harness, absent until supplied.
    pub trial_latency_ms: Option<f64>,
    /// Canonical safety verification by outer harness, never inferred from successful advice.
    pub authority_preserved: Option<bool>,
    /// No additional implicit persistence, independently measured.
    pub request_only: Option<bool>,
    /// Missing public telemetry stays unknown.
    pub telemetry: CaptureTelemetry,
}

/// Capture a real bounded operation on a caller-selected client. Does not start a service itself.
///
/// The caller must supply a require-running, exact-artifact/state client and enact treatment
/// through ordinary configuration boundaries before this call. Live work requires `allow_live`.
/// Dropping a timed-out future does not imply daemon-side cancellation or absence of effects.
///
/// # Errors
/// Returns a normalized static error for invalid/oversized input or unauthorized live capture.
pub async fn capture_bounded(
    client: &BcodeClient,
    request: CaptureRequest,
    consumer: ConsumerRequest,
    allow_live: bool,
) -> Result<ConsumerCapture, &'static str> {
    if request.source == EvidenceSource::LiveConsumer && !allow_live {
        return Err("live consumer capture requires explicit opt-in");
    }
    if serde_json::to_vec(&request)
        .map_err(|_| "invalid capture request")?
        .len()
        > MAX_CAPTURE_BYTES
    {
        return Err("capture request exceeds bound");
    }
    if request.timeout_ms == 0 || request.timeout_ms > 300_000 {
        return Err("capture timeout must be in 1..=300000 milliseconds");
    }
    if serde_json::to_vec(&consumer)
        .map_err(|_| "invalid consumer input")?
        .len()
        > MAX_CAPTURE_BYTES
    {
        return Err("consumer input exceeds capture bound");
    }
    let expected = match &consumer {
        ConsumerRequest::NormalTurn { .. } | ConsumerRequest::Unavailable => request.feature,
        ConsumerRequest::Service { consumer } => consumer.feature(),
        ConsumerRequest::Context { .. } => Feature::ContextReranking,
        ConsumerRequest::EvalDiagnosis { .. } | ConsumerRequest::EvalRun { .. } => {
            Feature::EvalCategorization
        }
        ConsumerRequest::SessionCategorization { .. } => Feature::SessionCategorization,
    };
    if expected != request.feature {
        return Err("consumer does not match scheduled feature");
    }
    let mut capture = ConsumerCapture {
        schema_version: CAPTURE_SCHEMA_VERSION,
        scope: CaptureScope::PublicOperation,
        request,
        operation: String::new(),
        status: CaptureStatus::ConsumerUnavailable,
        output: None,
        consumer_executed: false,
        outcome: TerminalOutcome::InfrastructureFailure,
        operation_latency_ms: None,
        trial_latency_ms: None,
        authority_preserved: None,
        request_only: None,
        telemetry: CaptureTelemetry::default(),
    };
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_millis(capture.request.timeout_ms),
        execute_public(client, consumer, &mut capture),
    )
    .await;
    capture.operation_latency_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
    match result {
        Err(_) => {
            capture.status = CaptureStatus::TimedOut;
            capture.outcome = TerminalOutcome::TimedOut;
        }
        Ok(Err(())) => {
            capture.status = CaptureStatus::PublicOperationFailed;
            capture.outcome = TerminalOutcome::InfrastructureFailure;
        }
        Ok(Ok(())) => {}
    }
    if serde_json::to_vec(&capture)
        .map_err(|_| "invalid capture output")?
        .len()
        > MAX_CAPTURE_BYTES
    {
        return Err("consumer output exceeds capture bound");
    }
    Ok(capture)
}

async fn execute_public(
    client: &BcodeClient,
    consumer: ConsumerRequest,
    capture: &mut ConsumerCapture,
) -> Result<(), ()> {
    match consumer {
        ConsumerRequest::NormalTurn { request } => {
            turns::execute(client, *request, capture).await?;
        }
        ConsumerRequest::Service { consumer } => {
            services::execute(client, consumer, capture).await?;
        }
        ConsumerRequest::Context { request } => {
            capture.operation = "select_optional_context".into();
            capture.scope = CaptureScope::ServiceMicrobenchmark;
            let response = client
                .select_optional_context(&request)
                .await
                .map_err(|_| ())?;
            capture
                .telemetry
                .decisions
                .observe_service(None, Some(response.applied));
            capture.consumer_executed = true;
            capture.status = if response.applied {
                CaptureStatus::Captured
            } else {
                CaptureStatus::FallbackReasonUnknown
            };
            capture.output = Some(ConsumerOutput::Ranking {
                order: response.ordered_ids,
                admitted: response.included_ids,
            });
            capture.outcome = TerminalOutcome::Succeeded;
        }
        ConsumerRequest::EvalDiagnosis {
            oracle,
            mut diagnosis,
        } => {
            capture.operation = "eval.diagnosis.optional_judge".into();
            capture.scope = CaptureScope::ServiceMicrobenchmark;
            // No stale advisory labels may be mistaken for this invocation's output.
            if let Some(object) = diagnosis.as_object_mut() {
                object.remove("advisory_model_labels");
            }
            let Some(query) = crate::diagnosis::optional_request(&oracle, &diagnosis) else {
                return Ok(());
            };
            let stage = capture.telemetry.decisions.begin(&query);
            let response = client.optional_judge(&query).await.map_err(|_| ())?;
            capture.telemetry.decisions.finish(stage, &query, &response);
            capture.telemetry.retain_optional_response(&response);
            let operation =
                crate::diagnosis::settle_attempt(&mut diagnosis, Some(&query), Some(response));
            capture.telemetry.decisions.applied(Some(
                operation.application
                    == Some(bcode_model::judgement::ApplicationDisposition::Applied),
            ));
            let label = diagnosis["advisory_model_labels"]["failure_category"]
                .as_str()
                .map(str::to_owned);
            capture.status = if label.is_some() {
                CaptureStatus::Captured
            } else {
                CaptureStatus::FallbackReasonUnknown
            };
            capture.output = Some(ConsumerOutput::Classification { label });
            capture.consumer_executed = true;
            capture.outcome = TerminalOutcome::Succeeded;
        }
        ConsumerRequest::SessionCategorization {
            session_id,
            source_sequence,
        } => {
            capture.operation = "session_categorization.read".into();
            let labels = client
                .session_categorization(session_id)
                .await
                .map_err(|_| ())?;
            if let Some(labels) = labels.filter(|labels| labels.source_sequence == source_sequence)
            {
                // Preserve both independent dimensions without inventing a single label vocabulary.
                capture.output = Some(ConsumerOutput::Classification {
                    label: Some(format!("{}/{}", labels.task, labels.domain)),
                });
                capture.status = CaptureStatus::Captured;
                capture.outcome = TerminalOutcome::Succeeded;
            }
        }
        ConsumerRequest::EvalRun { .. } => return Err(()),
        ConsumerRequest::Unavailable => {}
    }
    Ok(())
}

impl ConsumerCapture {
    /// Promote only complete verified facts into the shared benchmark scorer contract.
    ///
    /// # Errors
    /// Missing or invalid measurements/provenance return a normalized static gap, never zero.
    pub fn benchmark_observation(&self) -> Result<BenchmarkObservation, &'static str> {
        if self.scope == CaptureScope::ServiceMicrobenchmark && self.trial_latency_ms.is_some() {
            return Err("service latency is not whole-task efficiency");
        }
        if !matches!(self.schema_version, 1 | CAPTURE_SCHEMA_VERSION) {
            return Err("unsupported capture version");
        }
        if !self.consumer_executed {
            return Err("consumer trigger was not executed");
        }
        let identity = &self.request.subject;
        if !identity.configuration_complete
            || !valid_digest(&identity.configuration_sha256)
            || !valid_digest(&identity.executable_sha256)
            || !std::path::Path::new(&identity.executable).is_absolute()
            || identity.artifact_id.is_empty()
            || identity.state_id.is_empty()
            || identity.provider.as_ref().is_none_or(String::is_empty)
            || identity.model.as_ref().is_none_or(String::is_empty)
        {
            return Err("exact subject/configuration/model identity incomplete");
        }
        if self.telemetry.calls.len() > 1024
            || serde_json::to_vec(self)
                .map_err(|_| "invalid capture")?
                .len()
                > MAX_CAPTURE_BYTES
        {
            return Err("capture exceeds bound");
        }
        let isolated_feature = self
            .request
            .isolated_feature
            .ok_or("feature isolation unknown")?;
        let authority_preserved = self
            .authority_preserved
            .ok_or("authority preservation unknown")?;
        let request_only = self
            .request_only
            .ok_or("request-only persistence unknown")?;
        if self.telemetry.optional_counts_incomplete {
            return Err("optional operation counts incomplete");
        }
        if self.scope == CaptureScope::NormalTurn && !self.telemetry.optional_operations_complete {
            return Err("normal-turn optional observation coverage unknown");
        }
        let discovery_calls = self
            .telemetry
            .discovery_calls
            .ok_or("discovery count unknown")?;
        let verification_calls = self
            .telemetry
            .verification_calls
            .ok_or("verification count unknown")?;
        let inference_calls = self
            .telemetry
            .inference_calls
            .ok_or("inference count unknown")?;
        if self.telemetry.calls_complete
            && self.telemetry.calls.is_empty()
            && (discovery_calls != 0 || verification_calls != 0 || inference_calls != 0)
        {
            return Err("complete empty ledger contradicts nonzero optional calls");
        }
        if self.request.arm == Arm::Off
            && (discovery_calls != 0 || verification_calls != 0 || inference_calls != 0)
        {
            return Err("off treatment performed optional provider work");
        }
        if self
            .trial_latency_ms
            .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            return Err("invalid trial latency");
        }
        Ok(BenchmarkObservation {
            schema_version: 1,
            preregistration_id: self.request.preregistration_id.clone(),
            task_id: self.request.task_id.clone(),
            repetition: self.request.repetition,
            arm: self.request.arm,
            evidence: ConsumerEvidence {
                source: self.request.source,
                operation: self.operation.clone(),
                capture_id: self.request.capture_id.clone(),
                consumer_executed: self.consumer_executed.into(),
                isolated_feature: isolated_feature.into(),
                pairing_id: self.request.pairing_id.clone(),
                discovery_calls,
                verification_calls,
                inference_calls,
                authority_preserved: authority_preserved.into(),
                request_only: request_only.into(),
            },
            outcome: self.outcome,
            output: self.output.clone(),
            latency_ms: self.trial_latency_ms,
            cost_usd: if self.scope == CaptureScope::ServiceMicrobenchmark {
                None
            } else {
                self.telemetry.total_cost_usd()
            },
        })
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
