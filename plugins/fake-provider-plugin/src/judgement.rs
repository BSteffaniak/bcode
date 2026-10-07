//! Fake judgement provider for exercising the shared service contract independently of Jev.

use bcode_model::judgement::{
    self, Answer, Model, ModelList, ProviderRequest, Question, QuestionKind, Response, Usage,
};
use bcode_plugin_sdk::{ServiceRequest, ServiceResponse};
use std::collections::{BTreeMap, BTreeSet};

fn yes_no_probabilities(request: &ProviderRequest) -> Result<BTreeMap<String, f64>, &'static str> {
    let setting = request
        .provider_context
        .settings
        .get("fake_judgement_yes_no_probabilities");
    let environment = request
        .provider_context
        .env
        .get("FAKE_JUDGEMENT_YES_NO_PROBABILITIES");
    if let (Some(setting), Some(environment)) = (setting, environment)
        && setting != environment
    {
        return Err("conflicting fake probability configuration");
    }
    let Some(encoded) = setting.or(environment) else {
        return Ok(BTreeMap::new());
    };
    if encoded.len() > judgement::MAX_REQUEST_BYTES {
        return Err("fake probabilities exceed size limit");
    }
    let values: BTreeMap<String, f64> = serde_json::from_str(encoded)
        .map_err(|_| "fake probabilities must be a JSON object of numbers")?;
    if values.iter().any(|(id, probability)| {
        !probability.is_finite()
            || !(0.0..=1.0).contains(probability)
            || !matches!(
                request.judgement.questions.get(id),
                Some(Question::YesNo { .. })
            )
    }) {
        return Err(
            "fake probabilities require known YesNo question IDs and finite values in [0,1]",
        );
    }
    Ok(values)
}

pub fn invoke(request: &ServiceRequest) -> ServiceResponse {
    if request.interface_id == judgement::VERIFICATION_INTERFACE_ID {
        if request.operation != judgement::OP_VERIFY {
            return ServiceResponse::error(
                "unsupported_operation",
                "unsupported verification operation",
            );
        }
        let Ok(envelope) = request.payload_json::<judgement::VerificationRequest>() else {
            return ServiceResponse::error("invalid_request", "invalid verification request");
        };
        let authenticated = envelope.provider_context.auth.is_some();
        return encode(&judgement::VerificationResult {
            method: judgement::VerificationMethod::AuthenticatedDiscovery,
            authentication: if authenticated {
                judgement::AuthenticationStatus::Accepted
            } else {
                judgement::AuthenticationStatus::Unknown
            },
            model: if authenticated && envelope.model_id == "fake-judgement" {
                judgement::ModelReadiness::Ready
            } else {
                judgement::ModelReadiness::Unavailable
            },
        });
    }
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
            let probabilities = match yes_no_probabilities(&envelope) {
                Ok(values) => values,
                Err(message) => {
                    return ServiceResponse::error("invalid_fake_probabilities", message);
                }
            };
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
                        Question::YesNo { .. } => Answer::YesNo {
                            probability: probabilities.get(id).copied().unwrap_or(0.75),
                        },
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

    fn probability_request() -> ProviderRequest {
        ProviderRequest {
            judgement: judgement::Request {
                model_id: "fake-judgement".into(),
                state: judgement::State::Text("synthetic".into()),
                questions: BTreeMap::from([(
                    "yes".into(),
                    Question::YesNo {
                        instructions: "True?".into(),
                    },
                )]),
            },
            provider_context: bcode_model::ProviderRequestContext {
                auth: Some(bcode_model::ProviderAuthContext::default()),
                ..Default::default()
            },
        }
    }

    #[test]
    fn opted_in_yes_no_probabilities_preserve_default_and_boundaries() {
        let mut request = probability_request();
        for value in [None, Some(0.0), Some(1.0), Some(0.1)] {
            if let Some(value) = value {
                request.provider_context.settings.insert(
                    "fake_judgement_yes_no_probabilities".into(),
                    serde_json::json!({"yes":value}).to_string(),
                );
            }
            let response: Response = invoke(&ServiceRequest {
                interface_id: judgement::INTERFACE_ID.into(),
                operation: judgement::OP_JUDGE.into(),
                payload: serde_json::to_vec(&request).unwrap(),
            })
            .payload_json()
            .unwrap();
            let Answer::YesNo { probability } = response.answers["yes"] else {
                panic!("expected YesNo")
            };
            assert!((probability - value.unwrap_or(0.75)).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn opted_in_yes_no_probabilities_accept_owned_profile_env_and_reject_conflicts() {
        let mut request = probability_request();
        request.provider_context.env.insert(
            "FAKE_JUDGEMENT_YES_NO_PROBABILITIES".into(),
            r#"{"yes":0.1}"#.into(),
        );
        assert!((yes_no_probabilities(&request).unwrap()["yes"] - 0.1).abs() < f64::EPSILON);
        request.provider_context.settings.insert(
            "fake_judgement_yes_no_probabilities".into(),
            r#"{"yes":0.9}"#.into(),
        );
        assert!(yes_no_probabilities(&request).is_err());
    }

    #[test]
    fn opted_in_yes_no_probabilities_reject_invalid_values_and_targets() {
        let mut request = probability_request();
        request.judgement.questions.insert(
            "choice".into(),
            Question::Choice {
                instructions: "Pick".into(),
                options: BTreeMap::from([("a".into(), "A".into())]),
            },
        );
        for invalid in [
            r#"{"yes":-0.1}"#,
            r#"{"yes":1.1}"#,
            r#"{"yes":null}"#,
            r#"{"yes":1e999}"#,
            r#"{"unknown":0.5}"#,
            r#"{"choice":0.5}"#,
            "[]",
        ] {
            request
                .provider_context
                .settings
                .insert("fake_judgement_yes_no_probabilities".into(), invalid.into());
            assert!(yes_no_probabilities(&request).is_err());
        }
    }

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
    fn verification_is_typed_and_missing_auth_is_not_ready() {
        for authenticated in [false, true] {
            let request = ServiceRequest {
                interface_id: judgement::VERIFICATION_INTERFACE_ID.into(),
                operation: judgement::OP_VERIFY.into(),
                payload: serde_json::to_vec(&judgement::VerificationRequest {
                    model_id: "fake-judgement".into(),
                    provider_context: bcode_model::ProviderRequestContext {
                        auth: authenticated.then(bcode_model::ProviderAuthContext::default),
                        ..Default::default()
                    },
                })
                .unwrap(),
            };
            let result: judgement::VerificationResult = invoke(&request).payload_json().unwrap();
            assert_eq!(result.is_ready(), authenticated);
            assert_eq!(
                result.method,
                judgement::VerificationMethod::AuthenticatedDiscovery
            );
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
