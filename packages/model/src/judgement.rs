//! Portable, non-conversational judgement model semantics.
//!
//! The service interface is independent of the turn-based model provider interfaces. A provider
//! may implement any nonempty subset of the question kinds; callers must check both the provider
//! and the selected model before invoking it. None of these types contain provider wire formats.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// First compatible version of the judgement model provider plugin service.
pub const INTERFACE_ID: &str = "bcode.judgement-model-provider/v1";
/// Discover provider and model judgement capabilities.
pub const OP_MODELS: &str = "models";
/// Evaluate a batch of named questions against a single state.
pub const OP_JUDGE: &str = "judge";

/// Optional provider-owned authentication and model-readiness verification capability.
/// Providers may advertise this alongside v1 judgement without changing that interface.
pub const VERIFICATION_INTERFACE_ID: &str = "bcode.judgement-verification/v1";
/// Verify credentials and readiness without evaluating application state.
pub const OP_VERIFY: &str = "verify";
/// Application operation for advisory, failure-optional judgement.
pub const OP_OPTIONAL_JUDGE: &str = "optional_judge";

/// Enhancement whose opt-out applies before any discovery or provider call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    /// Classify a tool result before optional downstream processing.
    ToolResultTriage,
    /// Rank already retrieved context.
    ContextReranking,
    /// Advisory progress assessment, never execution authority.
    ProgressMonitoring,
    /// Recommend skills or tools, never authorize them.
    Recommendations,
    /// Categorize bounded session metadata.
    SessionCategorization,
    /// Categorize bounded evaluation evidence.
    EvalCategorization,
}

/// Transient provider verification envelope. Never persist or echo credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationRequest {
    /// Exact centrally resolved model.
    pub model_id: String,
    /// Host-resolved account context.
    pub provider_context: crate::ProviderRequestContext,
}

/// Authentication evidence independent of quota and model readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthenticationStatus {
    /// Upstream accepted these credentials.
    Accepted,
    /// Upstream explicitly rejected these credentials.
    Rejected,
    /// No conclusive authentication evidence.
    Unknown,
}

/// Readiness of the exact requested model; failures must not imply rejected auth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelReadiness {
    /// Ready for judgement.
    Ready,
    /// Model is not available to the account.
    Unavailable,
    /// Quota or rate limit prevents use.
    QuotaLimited,
    /// Retryable upstream failure.
    TemporaryFailure,
    /// Verification is unsupported by this provider.
    Unsupported,
    /// Authentication checked, but model readiness was not checked.
    NotChecked,
}

/// Provider-owned verification mechanism, without transport details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationMethod {
    /// Authentication-only discovery; no inference requested (billing is provider-owned).
    AuthenticatedDiscovery,
    /// A fixed harmless lightweight judgement was issued, potentially billed.
    LightweightJudgement,
}

/// Secret-free, typed verification result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationResult {
    /// Provider-owned verification mechanism.
    pub method: VerificationMethod,
    /// Authentication evidence.
    pub authentication: AuthenticationStatus,
    /// Model readiness evidence.
    pub model: ModelReadiness,
}

impl VerificationResult {
    /// Whether authentication and readiness permit a first useful judgement.
    /// `NotChecked` permits use only after independent central capability validation.
    #[must_use]
    pub const fn is_ready(self) -> bool {
        matches!(self.authentication, AuthenticationStatus::Accepted)
            && matches!(
                self.model,
                ModelReadiness::Ready | ModelReadiness::NotChecked
            )
    }
}

/// Caller-owned trigger and questions. The application owns optional routing policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionalRequest {
    /// Feature opt-out to consult before provider work.
    pub feature: Feature,
    /// Exact selection, when explicitly pinned; never silently switch accounts.
    #[serde(default)]
    pub selection: Option<SetupSelection>,
    /// Bounded state and typed questions. Auto replaces model ID centrally.
    pub request: Request,
}

/// Advisory result; absence always means retain ordinary caller behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionalResponse {
    /// Validated answer set, or unavailable/disabled/failed.
    pub response: Option<Response>,
    /// Transient operation facts; absent on responses from older implementations.
    #[serde(default)]
    pub observation: Option<OptionalOperationObservation>,
}

/// Normalized outcome of an optional operation, without provider errors or request content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionalOperationOutcome {
    /// Configuration disabled the operation before any provider work.
    Disabled,
    /// Another operation held the shared permit.
    Busy,
    /// Failure backoff prevented provider work.
    Backoff,
    /// No unambiguous usable selection or capability was available.
    Unavailable,
    /// Verification failed or did not establish readiness.
    VerificationFailed,
    /// Inference or answer validation failed.
    InferenceFailed,
    /// The total optional operation deadline elapsed.
    TimedOut,
    /// A validated advisory answer set was returned.
    Advice,
}

/// Bounded, secret-free facts attached only to the transient optional request result.
///
/// Invocation counts measure application/service entries attempted, not HTTP attempts or retries.
/// Provider-private transport and verification billing are unknown at this boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionalOperationObservation {
    /// Normalized terminal result of this operation.
    pub outcome: OptionalOperationOutcome,
    /// Attempted application discovery operations.
    pub discovery_invocations: u64,
    /// Attempted provider model-listing service invocations, including discovery.
    pub model_listing_invocations: u64,
    /// Attempted provider verification service invocations.
    pub verification_invocations: u64,
    /// Attempted provider inference service invocations.
    pub inference_invocations: u64,
    /// Total elapsed monotonic time in milliseconds, including unsuccessful work.
    pub elapsed_ms: u64,
    /// A verification result (positive or negative) came from the verification cache.
    pub verification_cache_hit: bool,
    /// Exact selected provider plugin ID, never an auth account or profile.
    pub effective_provider: Option<String>,
    /// Exact selected model ID.
    pub effective_model: Option<String>,
    /// Provider-reported usage from successful validated inference only.
    pub inference_usage: Option<Usage>,
    /// Unknown: the verification contract does not report token usage.
    pub verification_usage: Option<Usage>,
    /// Unknown: the verification contract does not report billing.
    pub verification_cost_usd: Option<f64>,
    /// Unknown: application invocation counts do not expose transport attempts.
    pub transport_attempts: Option<u64>,
}

impl Default for OptionalOperationObservation {
    fn default() -> Self {
        Self {
            outcome: OptionalOperationOutcome::Unavailable,
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
        }
    }
}

/// Host application service for bounded, non-conversational judgement from authorized workflow
/// blocks. This is not a provider plugin interface and must not be routed to providers directly.
pub const WORKFLOW_APPLICATION_INTERFACE_ID: &str = "bcode.judgement-application/v1";
/// Evaluate one request through the application's catalog and credential resolution.
pub const OP_WORKFLOW_JUDGE: &str = "judge";

/// Workflow-owned request. No credentials or provider wire payloads belong in this contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowJudgementRequest {
    /// Enabled judgement provider plugin ID.
    pub provider_plugin_id: String,
    /// Explicit owned profile, or empty to select provider-declared daemon environment keys.
    pub auth_profile: String,
    /// State and questions to evaluate.
    pub request: Request,
}

/// The semantic operation supported by a judgement model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    /// Select an option from a finite set.
    Choice,
    /// Rate on an ordered rubric.
    Score,
    /// Estimate whether a statement holds.
    YesNo,
}

/// One application-checked selection for a non-conversational evaluator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupSelection {
    /// Enabled provider plugin route.
    pub provider_plugin_id: String,
    /// Advertised model ID; callers should prefer concrete versions for stable calibration.
    pub model_id: String,
    /// Named owned account, or empty for explicitly selected daemon environment auth.
    pub auth_profile: String,
}

/// Bounded, secret-free setup snapshot for choosing a non-conversational evaluator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupCatalog {
    /// Enabled providers with a registered judgement service.
    pub providers: Vec<SetupProvider>,
}

/// One enabled provider's advertised options and authentication state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupProvider {
    /// Plugin route identity, not the user-facing auth provider name.
    pub plugin_id: String,
    /// Provider-contributed display name.
    pub display_name: String,
    /// Human-facing auth ID for setup instructions.
    pub auth_provider_id: String,
    /// Named profiles that are owned and have usable credential material in the daemon.
    /// Names are account references, never the credential values themselves.
    pub profiles: Vec<String>,
    /// Whether daemon environment credentials are available; not remote verification.
    pub environment_available: bool,
    /// Model capabilities advertised by this provider, if discovery succeeded.
    pub models: Vec<Model>,
    /// Normalized discovery diagnostic; never a provider error body.
    pub unavailable_reason: Option<String>,
}

/// Provider-advertised models and their supported question kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelList {
    /// Provider identifier (not an API URL or credential).
    pub provider_id: String,
    /// Question kinds implemented by this provider surface. Model support is checked separately.
    pub question_kinds: BTreeSet<QuestionKind>,
    /// Models the provider can serve.
    pub models: Vec<Model>,
}

/// Judgement capability for one model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    /// Provider-native model ID, resolved by the application catalog.
    pub model_id: String,
    /// Question kinds known to be supported. Absence means unsupported, not inferred.
    pub question_kinds: BTreeSet<QuestionKind>,
}

/// State to evaluate, kept as JSON-compatible text or structured data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum State {
    /// Plain text state.
    Text(String),
    /// Structured state.
    Structured(serde_json::Value),
}

/// One named question. Option IDs and rubric levels are supplied by the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Question {
    /// Pick exactly one option; map keys are stable caller-owned IDs.
    Choice {
        /// The question to ask.
        instructions: String,
        /// Option ID to human-readable description.
        options: BTreeMap<String, String>,
    },
    /// Rate against an ordered rubric, from lowest to highest.
    Score {
        /// The question to ask.
        instructions: String,
        /// Nonempty descriptive ordered levels.
        levels: Vec<String>,
    },
    /// Probability that a statement is true.
    YesNo {
        /// Statement or question to evaluate.
        instructions: String,
    },
}

impl Question {
    /// The capability needed to answer this question.
    #[must_use]
    pub const fn kind(&self) -> QuestionKind {
        match self {
            Self::Choice { .. } => QuestionKind::Choice,
            Self::Score { .. } => QuestionKind::Score,
            Self::YesNo { .. } => QuestionKind::YesNo,
        }
    }
}

/// One non-conversational invocation. Authentication stays in application/provider routing,
/// never in the judgement state or persisted session messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Selected catalog model ID.
    pub model_id: String,
    /// Data to judge.
    pub state: State,
    /// Caller-owned question IDs to questions.
    pub questions: BTreeMap<String, Question>,
}

/// Provider-side invocation envelope. Applications resolve credentials independently of the
/// caller's judgement state; provider plugins may not persist or echo this context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRequest {
    /// Judgement request after central model identity resolution.
    pub judgement: Request,
    /// Transient, host-resolved credential and provider settings.
    pub provider_context: crate::ProviderRequestContext,
}

/// A typed answer. Probability values are in [0, 1], not assertions of certainty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Answer {
    /// Selected option, with optional distribution over the requested options.
    Choice {
        /// Selected caller-owned option ID.
        selected: String,
        /// Probability by option ID, if the provider supplies it.
        probabilities: Option<BTreeMap<String, f64>>,
        /// Provider-reported confidence if available.
        confidence: Option<f64>,
    },
    /// Numeric position on the ordered rubric (zero based).
    Score {
        /// Rating in the inclusive range from zero to the last rubric level.
        value: f64,
        /// Probability for each rubric level in order, if supplied.
        probabilities: Option<Vec<f64>>,
        /// Provider-reported confidence if available.
        confidence: Option<f64>,
    },
    /// Estimated probability that the statement is true.
    YesNo {
        /// Probability in [0, 1].
        probability: f64,
    },
}

/// Normalized token usage, when supplied by the provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input token count.
    pub input_tokens: u64,
    /// Output token count (may be zero for non-generative models).
    pub output_tokens: u64,
}

/// Complete answer set for one request; never a stream of assistant text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Exact caller-owned IDs and corresponding typed answers.
    pub answers: BTreeMap<String, Answer>,
    /// Provider-reported usage when available; do not estimate it here.
    pub usage: Option<Usage>,
}

/// Defensive contract limit on encoded state and questions (including all strings).
pub const MAX_REQUEST_BYTES: usize = 256 * 1024;
/// Defensive contract limit on encoded result.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// Maximum questions in one request. This is a bounded invocation limit, not a model lifetime cap.
pub const MAX_QUESTIONS: usize = 64;
/// Maximum choices or score levels in a single question.
pub const MAX_OPTIONS: usize = 128;

/// Validate a request before provider work. All providers and callers should apply this limit.
///
/// # Errors
/// Returns an error for empty identifiers/questions, unsupported state, invalid rubrics or
/// options, or requests exceeding the portable size and cardinality limits.
pub fn validate_request(request: &Request) -> Result<(), &'static str> {
    if request.model_id.is_empty()
        || request.questions.is_empty()
        || request.questions.len() > MAX_QUESTIONS
    {
        return Err("invalid judgement model or question count");
    }
    let state = serde_json::to_value(&request.state).map_err(|_| "invalid judgement state")?;
    if !matches!(
        state,
        serde_json::Value::String(_) | serde_json::Value::Array(_) | serde_json::Value::Object(_)
    ) {
        return Err("judgement state must be text, an array or an object");
    }
    if request.questions.iter().any(|(id, question)| {
        id.is_empty()
            || match question {
                Question::Choice {
                    instructions,
                    options,
                } => {
                    instructions.is_empty()
                        || options.len() < 2
                        || options.len() > MAX_OPTIONS
                        || options
                            .iter()
                            .any(|(key, value)| key.is_empty() || value.is_empty())
                }
                Question::Score {
                    instructions,
                    levels,
                } => {
                    instructions.is_empty()
                        || levels.len() < 2
                        || levels.len() > MAX_OPTIONS
                        || levels.iter().any(String::is_empty)
                }
                Question::YesNo { instructions } => instructions.is_empty(),
            }
    }) {
        return Err("invalid judgement question");
    }
    if serde_json::to_vec(request)
        .map_err(|_| "invalid judgement request")?
        .len()
        > MAX_REQUEST_BYTES
    {
        return Err("judgement request exceeds size limit");
    }
    Ok(())
}

/// Check that the selected model and its provider both advertise every requested operation.
///
/// # Errors
/// Returns an error if the selected model is absent or either surface lacks any question kind.
pub fn select_supported_model<'a>(
    request: &Request,
    listing: &'a ModelList,
) -> Result<&'a Model, &'static str> {
    validate_request(request)?;
    let model = listing
        .models
        .iter()
        .find(|model| model.model_id == request.model_id)
        .ok_or("judgement model is not available")?;
    if request.questions.values().any(|question| {
        !listing.question_kinds.contains(&question.kind())
            || !model.question_kinds.contains(&question.kind())
    }) {
        return Err("judgement model does not support requested questions");
    }
    Ok(model)
}

/// Validate the entire answer set against the request and the provider's advertised model.
///
/// # Errors
/// Returns an error if the model does not support a question, any answer is missing, extra,
/// inconsistent, non-finite or out of range, or the response exceeds the size limit.
pub fn validate_response(
    request: &Request,
    model: &Model,
    response: &Response,
) -> Result<(), &'static str> {
    validate_request(request)?;
    if model.model_id != request.model_id
        || request
            .questions
            .values()
            .any(|q| !model.question_kinds.contains(&q.kind()))
    {
        return Err("judgement model does not support requested questions");
    }
    if response.answers.len() != request.questions.len()
        || serde_json::to_vec(response)
            .map_err(|_| "invalid judgement response")?
            .len()
            > MAX_RESPONSE_BYTES
    {
        return Err("invalid judgement answer count or size");
    }
    for (id, question) in &request.questions {
        let answer = response.answers.get(id).ok_or("missing judgement answer")?;
        let valid = match (question, answer) {
            (
                Question::Choice { options, .. },
                Answer::Choice {
                    selected,
                    probabilities,
                    confidence,
                },
            ) => {
                options.contains_key(selected)
                    && valid_confidence(*confidence)
                    && probabilities.as_ref().is_none_or(|values| {
                        values.len() == options.len()
                            && values.iter().all(|(key, value)| {
                                options.contains_key(key) && valid_probability(*value)
                            })
                    })
            }
            (
                Question::Score { levels, .. },
                Answer::Score {
                    value,
                    probabilities,
                    confidence,
                },
            ) => {
                value.is_finite()
                    && *value >= 0.0
                    && *value
                        <= f64::from(
                            u32::try_from(levels.len() - 1).map_err(|_| "invalid rubric size")?,
                        )
                    && valid_confidence(*confidence)
                    && probabilities.as_ref().is_none_or(|values| {
                        values.len() == levels.len()
                            && values.iter().all(|value| valid_probability(*value))
                    })
            }
            (Question::YesNo { .. }, Answer::YesNo { probability }) => {
                valid_probability(*probability)
            }
            _ => false,
        };
        if !valid {
            return Err("invalid judgement answer");
        }
    }
    Ok(())
}

fn valid_probability(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn valid_confidence(value: Option<f64>) -> bool {
    value.is_none_or(valid_probability)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_response_legacy_compatibility_and_unknown_measurements() {
        let legacy: OptionalResponse = serde_json::from_value(serde_json::json!({
            "response": null
        }))
        .unwrap();
        assert_eq!(legacy.response, None);
        assert_eq!(legacy.observation, None);

        let observation = OptionalOperationObservation::default();
        assert_eq!(observation.discovery_invocations, 0);
        assert_eq!(observation.model_listing_invocations, 0);
        assert_eq!(observation.verification_invocations, 0);
        assert_eq!(observation.inference_invocations, 0);
        assert!(!observation.verification_cache_hit);
        assert_eq!(observation.verification_usage, None);
        assert_eq!(observation.verification_cost_usd, None);
        assert_eq!(observation.transport_attempts, None);
        let response = OptionalResponse {
            response: None,
            observation: Some(observation),
        };
        let encoded = serde_json::to_value(&response).unwrap();
        assert!(encoded["observation"]["verification_usage"].is_null());
        assert!(encoded["observation"]["verification_cost_usd"].is_null());
        assert!(encoded["observation"]["transport_attempts"].is_null());
        assert_eq!(
            serde_json::from_value::<OptionalResponse>(encoded).unwrap(),
            response
        );
    }

    #[test]
    fn optional_observation_round_trips_successful_usage_and_outcomes() {
        for outcome in [
            OptionalOperationOutcome::Disabled,
            OptionalOperationOutcome::Busy,
            OptionalOperationOutcome::Backoff,
            OptionalOperationOutcome::Unavailable,
            OptionalOperationOutcome::VerificationFailed,
            OptionalOperationOutcome::InferenceFailed,
            OptionalOperationOutcome::TimedOut,
            OptionalOperationOutcome::Advice,
        ] {
            let observation = OptionalOperationObservation {
                outcome,
                inference_invocations: 1,
                inference_usage: Some(Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                }),
                effective_provider: Some("fake".into()),
                effective_model: Some("model".into()),
                ..OptionalOperationObservation::default()
            };
            let encoded = serde_json::to_value(&observation).unwrap();
            assert_eq!(
                serde_json::from_value::<OptionalOperationObservation>(encoded).unwrap(),
                observation
            );
        }
    }

    fn fixture() -> (Request, Model, Response) {
        let request = Request {
            model_id: "model".into(),
            state: State::Text("some state".into()),
            questions: BTreeMap::from([
                (
                    "a".into(),
                    Question::Choice {
                        instructions: "Which?".into(),
                        options: BTreeMap::from([
                            ("first".into(), "First".into()),
                            ("second".into(), "Second".into()),
                        ]),
                    },
                ),
                (
                    "b".into(),
                    Question::Score {
                        instructions: "How much?".into(),
                        levels: vec!["low".into(), "high".into()],
                    },
                ),
                (
                    "c".into(),
                    Question::YesNo {
                        instructions: "Is it?".into(),
                    },
                ),
            ]),
        };
        let model = Model {
            model_id: "model".into(),
            question_kinds: BTreeSet::from([
                QuestionKind::Choice,
                QuestionKind::Score,
                QuestionKind::YesNo,
            ]),
        };
        let response = Response {
            answers: BTreeMap::from([
                (
                    "a".into(),
                    Answer::Choice {
                        selected: "first".into(),
                        probabilities: Some(BTreeMap::from([
                            ("first".into(), 0.8),
                            ("second".into(), 0.2),
                        ])),
                        confidence: Some(0.8),
                    },
                ),
                (
                    "b".into(),
                    Answer::Score {
                        value: 0.6,
                        probabilities: Some(vec![0.4, 0.6]),
                        confidence: Some(0.6),
                    },
                ),
                ("c".into(), Answer::YesNo { probability: 0.7 }),
            ]),
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 0,
            }),
        };
        (request, model, response)
    }

    #[test]
    fn typed_mixed_answers_round_trip() {
        let (request, model, response) = fixture();
        let decoded: Response =
            serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
        assert_eq!(decoded, response);
        validate_response(&request, &model, &decoded).unwrap();
    }

    #[test]
    fn rejects_unsupported_and_inconsistent_answers() {
        let (request, mut model, mut response) = fixture();
        model.question_kinds.remove(&QuestionKind::Score);
        assert!(validate_response(&request, &model, &response).is_err());
        model.question_kinds.insert(QuestionKind::Score);
        response.answers.insert(
            "c".into(),
            Answer::YesNo {
                probability: f64::NAN,
            },
        );
        assert!(validate_response(&request, &model, &response).is_err());
        response.answers.remove("c");
        assert!(validate_response(&request, &model, &response).is_err());
        response
            .answers
            .insert("extra".into(), Answer::YesNo { probability: 0.1 });
        assert!(validate_response(&request, &model, &response).is_err());
    }

    #[test]
    fn selection_requires_both_provider_and_model_support() {
        let (request, model, response) = fixture();
        let mut listing = ModelList {
            provider_id: "fake".into(),
            question_kinds: BTreeSet::from([QuestionKind::Choice, QuestionKind::YesNo]),
            models: vec![model],
        };
        assert!(select_supported_model(&request, &listing).is_err());
        listing.question_kinds.insert(QuestionKind::Score);
        let selected = select_supported_model(&request, &listing).unwrap();
        validate_response(&request, selected, &response).unwrap();
        listing.models.clear();
        assert!(select_supported_model(&request, &listing).is_err());
    }

    #[test]
    fn unknown_question_kinds_fail_closed() {
        let unknown = serde_json::json!({"kind":"future", "instructions":"hi"});
        assert!(serde_json::from_value::<Question>(unknown).is_err());
    }

    #[test]
    fn bounds_are_enforced() {
        let (mut request, _, _) = fixture();
        request.state = State::Text("x".repeat(MAX_REQUEST_BYTES));
        assert!(validate_request(&request).is_err());
        request.state = State::Text("ok".into());
        request.questions.clear();
        assert!(validate_request(&request).is_err());
    }
}
