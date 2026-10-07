//! Content-free projections of portable judgement requests and responses.
//!
//! Caller IDs, instructions, state, option descriptions and model-selection settings may contain
//! secrets. Only ordinal references, shapes and numeric scores cross this capture boundary.
use bcode_model::judgement::{
    Answer, OptionalOperationOutcome, OptionalRequest, OptionalResponse, Question, QuestionKind,
    State,
};
use serde::{Deserialize, Serialize};

const MAX_STAGES: usize = 8;
const MAX_QUESTIONS: usize = 16;
const MAX_OPTIONS: usize = 32;

/// Bounded decision evidence. Missing stages are not proof that judgement was disabled.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionCapture {
    /// Stages in invocation order; each request uses its own ordinal namespace.
    pub stages: Vec<DecisionStage>,
    /// Number of stages omitted by the capture bound, not by the producer.
    pub omitted_stages: u64,
}

/// One request/score/application sequence, without source content or caller-controlled IDs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionStage {
    /// Request shape, absent when the producer does not expose the prepared request.
    pub request: Option<RequestShape>,
    /// Typed numeric answers using request-local question and option ordinals.
    pub answers: Vec<AnswerShape>,
    /// Central fallback/advice reason; absent until a public response is observed.
    pub outcome: Option<OptionalOperationOutcome>,
    /// Producer application gate, absent unless explicitly returned by the producer.
    pub applied: Option<bool>,
    /// False when answers are unavailable, truncated or cannot be mapped to a prepared request.
    pub answers_complete: bool,
    /// Portable central evidence, including explicit omission counts and prepared shapes.
    #[serde(default)]
    pub central_projection: Option<bcode_model::judgement::DecisionProjection>,
}

/// Shape-only request evidence. Omission of content is deliberate and never lossless replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestShape {
    /// Text versus structured state; no keys, values or hashes of secrets are retained.
    pub structured_state: bool,
    /// Encoded state size before sanitization, not a token estimate.
    pub state_bytes: usize,
    /// Number of questions before the capture limit.
    pub question_count: usize,
    /// Question shapes in the portable contract's sorted ID order.
    pub questions: Vec<QuestionShape>,
    /// Actual omitted producer evidence cannot be inferred from the received request.
    pub producer_omitted_evidence: Option<u64>,
}

/// Instructions and descriptions are omitted; option references are sorted-map ordinals.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionShape {
    /// Portable question kind, not a provider wire type.
    pub kind: QuestionKind,
    /// Choice option count or score rubric length, zero for yes/no.
    pub alternatives: usize,
}

/// Numeric score evidence. No raw answer keys or choice IDs are persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerShape {
    /// Ordinal in the prepared request's sorted question map.
    pub question: usize,
    /// Existing normalized answer kind.
    pub kind: QuestionKind,
    /// Score value or yes/no probability; choice uses `selected_option`.
    pub value: Option<f64>,
    /// Ordinal in the prepared choice's sorted option map.
    pub selected_option: Option<usize>,
    /// Probability vector in option/rubric order, when reported completely within the bound.
    pub probabilities: Option<Vec<f64>>,
    /// Provider-reported confidence, not a calibrated probability of task success.
    pub confidence: Option<f64>,
}

impl DecisionCapture {
    /// Adapt central portable evidence without guessing producer application or omissions.
    pub(super) fn observe_projection(
        &mut self,
        projection: &bcode_model::judgement::DecisionProjection,
        outcome: OptionalOperationOutcome,
    ) {
        if self.stages.len() >= MAX_STAGES {
            self.omitted_stages = self.omitted_stages.saturating_add(1);
            return;
        }
        self.stages.push(DecisionStage {
            request: None, // State shape/size is deliberately not exported centrally.
            answers: projection
                .answers
                .as_ref()
                .map_or_else(Vec::new, |answers| {
                    answers
                        .iter()
                        .map(|answer| AnswerShape {
                            question: answer.question,
                            kind: answer.kind,
                            value: answer.value,
                            selected_option: answer.selected_option,
                            probabilities: answer.probabilities.clone(),
                            confidence: answer.confidence,
                        })
                        .collect()
                }),
            outcome: Some(outcome),
            applied: projection.applied,
            answers_complete: projection.answers.is_some()
                && projection.omitted_answers == Some(0)
                && projection.answers.as_ref().is_some_and(|answers| {
                    answers
                        .iter()
                        .all(|answer| answer.omitted_probabilities == 0)
                }),
            central_projection: Some(projection.clone()),
        });
    }

    /// Capture a producer skip without asserting whether configuration or evidence caused it.
    pub(super) fn skipped(&mut self) {
        self.observe_service(None, None);
    }

    /// Retain only normalized producer facts, never arbitrary response JSON.
    pub(super) fn observe_service(
        &mut self,
        outcome: Option<OptionalOperationOutcome>,
        applied: Option<bool>,
    ) {
        if self.stages.len() >= MAX_STAGES {
            self.omitted_stages = self.omitted_stages.saturating_add(1);
            return;
        }
        self.stages.push(DecisionStage {
            request: None,
            answers: vec![],
            outcome,
            applied,
            answers_complete: false,
            central_projection: None,
        });
    }

    pub(super) fn applied(&mut self, applied: Option<bool>) {
        if self.omitted_stages > 0 {
            return;
        }
        if let Some(stage) = self.stages.last_mut() {
            stage.applied = applied;
        }
    }

    /// Start capture before dispatch, so a failed/timeout response leaves a visible request gap.
    pub(super) fn begin(&mut self, query: &OptionalRequest) -> Option<usize> {
        if self.stages.len() >= MAX_STAGES {
            self.omitted_stages = self.omitted_stages.saturating_add(1);
            return None;
        }
        let request = &query.request;
        let stage = DecisionStage {
            request: Some(RequestShape {
                structured_state: matches!(&request.state, State::Structured(_)),
                state_bytes: serde_json::to_vec(&request.state).map_or(0, |bytes| bytes.len()),
                question_count: request.questions.len(),
                questions: request
                    .questions
                    .values()
                    .take(MAX_QUESTIONS)
                    .map(|question| QuestionShape {
                        kind: question.kind(),
                        alternatives: match question {
                            Question::Choice { options, .. } => options.len(),
                            Question::Score { levels, .. } => levels.len(),
                            Question::YesNo { .. } => 0,
                        },
                    })
                    .collect(),
                producer_omitted_evidence: None,
            }),
            answers: vec![],
            outcome: None,
            applied: None,
            answers_complete: false,
            central_projection: None,
        };
        self.stages.push(stage);
        Some(self.stages.len() - 1)
    }

    pub(super) fn finish(
        &mut self,
        index: Option<usize>,
        query: &OptionalRequest,
        response: &OptionalResponse,
    ) {
        let Some(stage) = index.and_then(|index| self.stages.get_mut(index)) else {
            return;
        };
        stage.outcome = response.observation.as_ref().map(|value| value.outcome);
        stage.central_projection = response
            .observation
            .as_ref()
            .and_then(|value| value.decision.clone());
        let Some(response) = &response.response else {
            return;
        };
        stage.answers_complete = query.request.questions.len() <= MAX_QUESTIONS
            && response.answers.len() == query.request.questions.len();
        for (ordinal, (id, question)) in query
            .request
            .questions
            .iter()
            .take(MAX_QUESTIONS)
            .enumerate()
        {
            let Some(answer) = response.answers.get(id) else {
                stage.answers_complete = false;
                continue;
            };
            let mut captured = AnswerShape {
                question: ordinal,
                kind: question.kind(),
                value: None,
                selected_option: None,
                probabilities: None,
                confidence: None,
            };
            match (question, answer) {
                (
                    Question::Choice { options, .. },
                    Answer::Choice {
                        selected,
                        probabilities,
                        confidence,
                    },
                ) => {
                    captured.selected_option = options.keys().position(|id| id == selected);
                    captured.confidence = *confidence;
                    if captured.selected_option.is_none() {
                        stage.answers_complete = false;
                    }
                    if let Some(probabilities) = probabilities {
                        if options.len() <= MAX_OPTIONS
                            && probabilities.len() == options.len()
                            && options.keys().all(|id| probabilities.contains_key(id))
                        {
                            captured.probabilities =
                                Some(options.keys().map(|id| probabilities[id]).collect());
                        } else {
                            stage.answers_complete = false;
                        }
                    }
                }
                (
                    Question::Score { levels, .. },
                    Answer::Score {
                        value,
                        probabilities,
                        confidence,
                    },
                ) => {
                    captured.value = Some(*value);
                    captured.confidence = *confidence;
                    if let Some(probabilities) = probabilities {
                        if probabilities.len() == levels.len() && probabilities.len() <= MAX_OPTIONS
                        {
                            captured.probabilities = Some(probabilities.clone());
                        } else {
                            stage.answers_complete = false;
                        }
                    }
                }
                (Question::YesNo { .. }, Answer::YesNo { probability }) => {
                    captured.value = Some(*probability);
                }
                _ => {
                    stage.answers_complete = false;
                    continue;
                }
            }
            // Reject malformed numeric evidence rather than emitting NaN/null or guessed scores.
            let value_valid = match question {
                Question::Score { levels, .. } => captured.value.is_none_or(|value| {
                    levels
                        .len()
                        .checked_sub(1)
                        .and_then(|last| u32::try_from(last).ok())
                        .is_some_and(|last| value >= 0.0 && value <= f64::from(last))
                }),
                Question::YesNo { .. } => captured
                    .value
                    .is_none_or(|value| (0.0..=1.0).contains(&value)),
                Question::Choice { .. } => true,
            };
            if !value_valid
                || captured.value.is_some_and(|value| !value.is_finite())
                || captured
                    .confidence
                    .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
                || captured.probabilities.as_ref().is_some_and(|values| {
                    values
                        .iter()
                        .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
                })
            {
                stage.answers_complete = false;
                continue;
            }
            stage.answers.push(captured);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_model::judgement::{Feature, Request, Response};
    use std::collections::BTreeMap;

    fn query() -> OptionalRequest {
        OptionalRequest {
            feature: Feature::EvalCategorization,
            selection: None,
            request: Request {
                model_id: "secret-model".into(),
                state: State::Text("secret-state".into()),
                questions: BTreeMap::from([(
                    "secret-question".into(),
                    Question::Choice {
                        instructions: "secret-instructions".into(),
                        options: BTreeMap::from([
                            ("secret-a".into(), "secret-description".into()),
                            ("secret-b".into(), "secret-description".into()),
                        ]),
                    },
                )]),
            },
        }
    }

    #[test]
    fn sanitization_preserves_numeric_evidence_not_content() {
        let query = query();
        let response = OptionalResponse {
            observation: None,
            response: Some(Response {
                usage: None,
                answers: BTreeMap::from([(
                    "secret-question".into(),
                    Answer::Choice {
                        selected: "secret-b".into(),
                        probabilities: Some(BTreeMap::from([
                            ("secret-a".into(), 0.25),
                            ("secret-b".into(), 0.75),
                        ])),
                        confidence: Some(0.75),
                    },
                )]),
            }),
        };
        let mut capture = DecisionCapture::default();
        let stage = capture.begin(&query);
        capture.finish(stage, &query, &response);
        let encoded = serde_json::to_string(&capture).unwrap();
        assert!(!encoded.contains("secret"));
        assert_eq!(capture.stages[0].answers[0].selected_option, Some(1));
        assert_eq!(
            capture.stages[0].answers[0].probabilities,
            Some(vec![0.25, 0.75])
        );
        assert!(capture.stages[0].answers_complete);
        assert_eq!(capture.stages[0].applied, None);
        assert_eq!(capture.stages[0].outcome, None);
    }

    #[test]
    fn missing_and_overflow_evidence_remain_explicit() {
        let query = query();
        let mut capture = DecisionCapture::default();
        for _ in 0..=MAX_STAGES {
            capture.begin(&query);
        }
        assert_eq!(capture.stages.len(), MAX_STAGES);
        assert_eq!(capture.omitted_stages, 1);
        capture.applied(Some(true));
        assert!(
            capture
                .stages
                .iter()
                .all(|stage| !stage.answers_complete && stage.applied.is_none())
        );
        assert!(serde_json::to_vec(&capture).unwrap().len() < super::super::MAX_CAPTURE_BYTES);
    }

    #[test]
    fn malformed_and_truncated_answers_do_not_become_complete() {
        let mut query = query();
        query.request.questions = BTreeMap::from([(
            "secret-question".into(),
            Question::YesNo {
                instructions: "secret-instructions".into(),
            },
        )]);
        let response = OptionalResponse {
            observation: None,
            response: Some(Response {
                usage: None,
                answers: BTreeMap::from([(
                    "secret-question".into(),
                    Answer::YesNo { probability: 2.0 },
                )]),
            }),
        };
        let mut capture = DecisionCapture::default();
        let stage = capture.begin(&query);
        capture.finish(stage, &query, &response);
        assert!(!capture.stages[0].answers_complete);
        assert!(capture.stages[0].answers.is_empty());
        for index in 0..=MAX_QUESTIONS {
            query.request.questions.insert(
                index.to_string(),
                Question::YesNo {
                    instructions: String::new(),
                },
            );
        }
        let stage = capture.begin(&query);
        capture.finish(stage, &query, &response);
        assert_eq!(
            capture.stages[1].request.as_ref().unwrap().questions.len(),
            MAX_QUESTIONS
        );
        assert!(!capture.stages[1].answers_complete);
    }

    #[test]
    fn central_projection_adapts_shapes_without_application_inference() {
        let projection = bcode_model::judgement::DecisionProjection {
            schema_version: bcode_model::judgement::DECISION_PROJECTION_VERSION,
            question_count: 1,
            questions: vec![bcode_model::judgement::DecisionQuestion {
                ordinal: 0,
                kind: QuestionKind::YesNo,
                alternatives: 0,
            }],
            omitted_questions: 0,
            answers: Some(vec![bcode_model::judgement::DecisionAnswer {
                question: 0,
                kind: QuestionKind::YesNo,
                value: Some(0.75),
                selected_option: None,
                probabilities: None,
                omitted_probabilities: 0,
                confidence: None,
            }]),
            omitted_answers: Some(0),
            producer_omitted_evidence: None,
            applied: None,
        };
        let mut capture = DecisionCapture::default();
        capture.observe_projection(&projection, OptionalOperationOutcome::Advice);
        assert_eq!(
            capture.stages[0].central_projection.as_ref(),
            Some(&projection)
        );
        assert!(capture.stages[0].answers_complete);
        assert!(capture.stages[0].applied.is_none());
        assert!(capture.stages[0].request.is_none());
    }

    #[test]
    fn disabled_is_not_unknown_fallback() {
        let mut capture = DecisionCapture::default();
        capture.observe_service(Some(OptionalOperationOutcome::Disabled), None);
        capture.observe_service(None, None);
        assert_eq!(
            capture.stages[0].outcome,
            Some(OptionalOperationOutcome::Disabled)
        );
        assert_eq!(capture.stages[1].outcome, None);
        assert!(capture.stages.iter().all(|stage| stage.request.is_none()));
    }
}
