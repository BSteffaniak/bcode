//! Fake judgement provider for exercising the shared service contract independently of Jev.

use bcode_model::judgement::{
    self, Answer, Model, ModelList, ProviderRequest, Question, QuestionKind, Response, Usage,
};
use bcode_plugin_sdk::{ServiceRequest, ServiceResponse};
use std::collections::BTreeSet;

pub fn invoke(request: &ServiceRequest) -> ServiceResponse {
    if request.interface_id != judgement::INTERFACE_ID {
        return ServiceResponse::error("unsupported_interface", "unsupported judgement interface");
    }
    match request.operation.as_str() {
        judgement::OP_MODELS => {
            if request
                .payload_json::<bcode_model::ProviderRequestContext>()
                .is_err()
            {
                return ServiceResponse::error("invalid_request", "invalid discovery request");
            }
            encode(&listing())
        }
        judgement::OP_JUDGE => {
            let Ok(envelope) = request.payload_json::<ProviderRequest>() else {
                return ServiceResponse::error("invalid_request", "invalid judgement request");
            };
            let model = listing();
            let Ok(selected) = judgement::select_supported_model(&envelope.judgement, &model)
            else {
                return ServiceResponse::error("unsupported_question", "question not supported");
            };
            if envelope.provider_context.auth.is_none() {
                return ServiceResponse::error("auth_required", "provider credentials required");
            }
            let answers = envelope
                .judgement
                .questions
                .iter()
                .map(|(id, question)| {
                    let answer = match question {
                        Question::Choice { options, .. } => Answer::Choice {
                            selected: options.keys().next().cloned().unwrap_or_default(),
                            probabilities: None,
                            confidence: None,
                        },
                        Question::Score { .. } => Answer::Score {
                            value: 0.0,
                            probabilities: None,
                            confidence: None,
                        },
                        Question::YesNo { .. } => Answer::YesNo { probability: 0.75 },
                    };
                    (id.clone(), answer)
                })
                .collect();
            let result = Response {
                answers,
                usage: Some(Usage {
                    input_tokens: 1,
                    output_tokens: 0,
                }),
            };
            if judgement::validate_response(&envelope.judgement, selected, &result).is_err() {
                return ServiceResponse::error("invalid_answer", "invalid judgement answer");
            }
            encode(&result)
        }
        _ => ServiceResponse::error("unsupported_operation", "unsupported judgement operation"),
    }
}

fn listing() -> ModelList {
    ModelList {
        provider_id: "bcode.fake-provider".into(),
        question_kinds: BTreeSet::from([
            QuestionKind::Choice,
            QuestionKind::Score,
            QuestionKind::YesNo,
        ]),
        models: vec![Model {
            model_id: "fake-judgement".into(),
            question_kinds: BTreeSet::from([
                QuestionKind::Choice,
                QuestionKind::Score,
                QuestionKind::YesNo,
            ]),
        }],
    }
}

fn encode<T: serde::Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value).unwrap_or_else(|_| {
        ServiceResponse::error("encode_failed", "failed to encode judgement response")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_plugin_sdk::ServiceRequest;

    use std::collections::BTreeMap;

    #[test]
    fn fake_provider_rejects_unrelated_and_future_interfaces() {
        for interface_id in [
            "bcode.model-provider/v3",
            "bcode.judgement-model-provider/v2",
        ] {
            let response = invoke(&ServiceRequest {
                interface_id: interface_id.into(),
                operation: judgement::OP_MODELS.into(),
                payload: serde_json::to_vec(&bcode_model::ProviderRequestContext::default())
                    .unwrap(),
            });
            assert!(response.error.is_some());
        }
    }

    #[test]
    fn fake_provider_returns_typed_answers_for_all_supported_kinds() {
        let context = bcode_model::ProviderRequestContext {
            auth: Some(bcode_model::ProviderAuthContext::default()),
            ..Default::default()
        };
        let listing_request = ServiceRequest {
            interface_id: judgement::INTERFACE_ID.into(),
            operation: judgement::OP_MODELS.into(),
            payload: serde_json::to_vec(&context).unwrap(),
        };
        let listing: ModelList = invoke(&listing_request).payload_json().unwrap();
        let request = judgement::Request {
            model_id: "fake-judgement".into(),
            state: judgement::State::Text("sample".into()),
            questions: BTreeMap::from([
                (
                    "choice".into(),
                    Question::Choice {
                        instructions: "Pick".into(),
                        options: BTreeMap::from([
                            ("a".into(), "A".into()),
                            ("b".into(), "B".into()),
                        ]),
                    },
                ),
                (
                    "score".into(),
                    Question::Score {
                        instructions: "Rate".into(),
                        levels: vec!["bad".into(), "good".into()],
                    },
                ),
                (
                    "yes".into(),
                    Question::YesNo {
                        instructions: "True?".into(),
                    },
                ),
            ]),
        };
        let service_request = ServiceRequest {
            interface_id: judgement::INTERFACE_ID.into(),
            operation: judgement::OP_JUDGE.into(),
            payload: serde_json::to_vec(&ProviderRequest {
                judgement: request.clone(),
                provider_context: context,
            })
            .unwrap(),
        };
        let response: Response = invoke(&service_request).payload_json().unwrap();
        judgement::validate_response(
            &request,
            judgement::select_supported_model(&request, &listing).unwrap(),
            &response,
        )
        .unwrap();
        assert!(matches!(
            response.answers["yes"],
            Answer::YesNo { probability: 0.75 }
        ));
    }
}
