//! Advisory categorization of bounded, allowlisted newest-request facts.
//!
//! Default policy is Auto: off/unavailable produces no labels, not a heuristic
//! fallback. Ordered fixed-vocabulary signals retain local negation and boundaries,
//! but do not fully resolve intent; `other` is an abstention,
//! not evidence of an actual miscellaneous task. Probabilities are provider-reported
//! metadata, not calibrated evidence, and do not change label application.

use bcode_model::judgement::{
    self, Answer, Feature, OptionalRequest, OptionalResponse, Question, Request, State,
};
use bcode_plugin_sdk::{
    NativeServiceContext, ServiceBridgeRequest, ServiceBridgeResponse, ServiceResponse,
};
use bcode_session_models::{
    CategorizationRequest, MAX_CATEGORIZATION_REQUEST_BYTES, MAX_CATEGORIZATION_SOURCE_BYTES,
    SessionCategorization,
};
use bcode_tool::{ToolInvocationServiceRequest, ToolInvocationServiceResolution};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const TASKS: &[&str] = &[
    "implementation",
    "debugging",
    "review",
    "testing",
    "documentation",
    "planning",
    "other",
];
const DOMAINS: &[&str] = &[
    "frontend",
    "backend",
    "persistence",
    "security",
    "tooling",
    "other",
];

/// Allowlisted semantic signals only; never raw input, paths, or provider metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewestRequestFacts {
    /// Recognized task signals.
    pub tasks: BTreeSet<String>,
    /// Recognized domain signals.
    pub domains: BTreeSet<String>,
    /// Ordered fixed-vocabulary cues; `gap` marks omitted words, not their contents.
    /// Negation is a lexical cue, not a claim about its scope or resolved intent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sequence: Vec<String>,
}

/// Extract signals from at most 8192 characters and 512 words of one request.
/// The returned value contains only fixed vocabulary, regardless of input contents.
#[must_use]
pub fn newest_request_facts(input: &str) -> NewestRequestFacts {
    let bounded: String = input.chars().take(8192).collect();
    let mut facts = NewestRequestFacts::default();
    for source_word in bounded.split_whitespace().take(512) {
        let word = source_word
            .trim_matches(|c: char| !c.is_ascii_alphabetic())
            .to_ascii_lowercase();
        let task = match word.as_str() {
            "implement" | "add" | "build" => Some("implementation"),
            "debug" | "fix" | "bug" => Some("debugging"),
            "review" | "audit" => Some("review"),
            "test" | "tests" | "testing" => Some("testing"),
            "docs" | "documentation" | "document" => Some("documentation"),
            "plan" | "design" => Some("planning"),
            _ => None,
        };
        let domain = match word.as_str() {
            "frontend" | "ui" | "tui" | "rendering" => Some("frontend"),
            "backend" | "server" | "api" => Some("backend"),
            "database" | "storage" | "persistence" => Some("persistence"),
            "security" | "auth" | "permissions" => Some("security"),
            "cli" | "tooling" | "cargo" | "plugin" => Some("tooling"),
            _ => None,
        };
        let cue = match word.as_str() {
            "no" | "not" | "never" | "without" | "don't" | "cannot" | "can't" | "avoid" => {
                Some("negation")
            }
            "but" | "instead" | "rather" => Some("contrast"),
            "only" => Some("only"),
            _ => None,
        };
        let signal = task.or(domain).or(cue).unwrap_or("gap");
        // Collapse omitted spans, but preserve repeated recognized cues and order.
        if signal != "gap"
            || facts
                .sequence
                .last()
                .is_none_or(|previous| previous != "gap")
        {
            facts.sequence.push(signal.to_owned());
        }
        if source_word.ends_with([';', '.', '!', '?']) {
            facts.sequence.push("boundary".to_owned());
        }
        if let Some(task) = task {
            facts.tasks.insert(task.to_owned());
        }
        if let Some(domain) = domain {
            facts.domains.insert(domain.to_owned());
        }
    }
    if facts.tasks.is_empty() && facts.domains.is_empty() {
        facts.sequence.clear();
    }
    facts
}

fn optional_request(facts: &NewestRequestFacts) -> Option<OptionalRequest> {
    if facts.tasks.is_empty() && facts.domains.is_empty()
        || facts.sequence.len() > 1024
        || facts.sequence.iter().any(|signal| {
            !facts.tasks.contains(signal)
                && !facts.domains.contains(signal)
                && !["negation", "contrast", "only", "gap", "boundary"].contains(&signal.as_str())
        })
        || facts
            .tasks
            .iter()
            .any(|task| !TASKS[..TASKS.len() - 1].contains(&task.as_str()))
        || facts
            .domains
            .iter()
            .any(|domain| !DOMAINS[..DOMAINS.len() - 1].contains(&domain.as_str()))
    {
        return None;
    }
    // Lexical cues cannot justify an unobserved label. Keep an explicit
    // abstention for missing axes and conflicts; these are not resolved intent.
    let choice = |instructions: &str, signals: &BTreeSet<String>| Question::Choice {
        instructions: instructions.to_owned(),
        options: signals
            .iter()
            .map(|label| (label.clone(), label.clone()))
            .chain([(
                "other".to_owned(),
                "Insufficient or ambiguous signals".to_owned(),
            )])
            .collect(),
    };
    Some(OptionalRequest {
        feature: Feature::SessionCategorization,
        selection: None,
        request: Request {
            model_id: "auto".to_owned(),
            state: State::Structured(serde_json::to_value(facts).ok()?),
            questions: BTreeMap::from([
                (
                    "task".to_owned(),
                    choice(
                        "Choose the requested advisory task from ordered lexical cues, not every mentioned task. negation marks a negative cue, boundary marks punctuation, contrast and only preserve those cues, and gap means omitted unknown words. These cues do not establish negation scope; use other when ambiguous or only prohibited tasks are supported.",
                        &facts.tasks,
                    ),
                ),
                (
                    "domain".to_owned(),
                    choice(
                        "Choose the domain of the requested task using ordered lexical cues, not a domain mentioned only in a prohibition. gap means omitted unknown words, not adjacency in the source; use other when intent or domain association is ambiguous.",
                        &facts.domains,
                    ),
                ),
            ]),
        },
    })
}

fn labels(
    request: &CategorizationRequest,
    query: &Request,
    result: OptionalResponse,
) -> Option<SessionCategorization> {
    let response = result.response?;
    // Central access validates model capabilities and numerical values. Correlate the
    // returned envelope with exactly our caller-owned questions, kinds, and options.
    if query.questions.len() != 2 || response.answers.len() != query.questions.len() {
        return None;
    }
    for (id, question) in &query.questions {
        let Question::Choice { options, .. } = question else {
            return None;
        };
        let Answer::Choice { selected, .. } = response.answers.get(id)? else {
            return None;
        };
        if !options.contains_key(selected) {
            return None;
        }
    }
    let Answer::Choice { selected: task, .. } = response.answers.get("task")? else {
        return None;
    };
    let Answer::Choice {
        selected: domain, ..
    } = response.answers.get("domain")?
    else {
        return None;
    };
    Some(SessionCategorization {
        source_sequence: request.source_sequence,
        task: task.clone(),
        domain: domain.clone(),
    })
}

/// Transient service envelope; operation facts are not portable session labels or history.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct CategorizationServiceResponse {
    categorization: Option<SessionCategorization>,
    #[serde(default)]
    observation: Option<judgement::OptionalOperationObservation>,
}

fn apply_optional(
    request: &CategorizationRequest,
    query: &Request,
    mut response: OptionalResponse,
) -> CategorizationServiceResponse {
    let observation = response.observation.take();
    CategorizationServiceResponse {
        categorization: labels(request, query, response),
        observation,
    }
}

pub(crate) fn invoke(context: &NativeServiceContext) -> ServiceResponse {
    // Reject oversized or unknown fields without echoing untrusted payloads.
    let result = (|| {
        if context.request.payload.len() > MAX_CATEGORIZATION_REQUEST_BYTES {
            return None;
        }
        let request = context
            .request
            .payload_json::<CategorizationRequest>()
            .ok()?;
        if request.newest_request.len() > MAX_CATEGORIZATION_SOURCE_BYTES {
            return None;
        }
        let optional = optional_request(&newest_request_facts(&request.newest_request))?;
        let query = optional.request.clone();
        let response = context
            .bridge
            .request(&ServiceBridgeRequest::InvokeService(
                ToolInvocationServiceRequest {
                    invocation_id: format!(
                        "categorize-{}-{}",
                        request.session_id, request.source_sequence
                    ),
                    request_id: format!(
                        "categorize-{}-{}",
                        request.session_id, request.source_sequence
                    ),
                    route_id: Some(judgement::WORKFLOW_APPLICATION_INTERFACE_ID.to_owned()),
                    interface_id: judgement::WORKFLOW_APPLICATION_INTERFACE_ID.to_owned(),
                    operation: judgement::OP_OPTIONAL_JUDGE.to_owned(),
                    payload: serde_json::to_value(optional).ok()?,
                },
            ))
            .ok()?;
        let ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded { payload }) =
            response
        else {
            return None;
        };
        Some(apply_optional(
            &request,
            &query,
            serde_json::from_value(payload).ok()?,
        ))
    })();
    serde_json::to_value(result.unwrap_or_default()).map_or_else(
        |_| ServiceResponse::error("serialization_failed", "categorization unavailable"),
        |payload| {
            ServiceResponse::json(&payload).unwrap_or_else(|_| {
                ServiceResponse::error("serialization_failed", "categorization unavailable")
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::SessionId;
    extern "C" fn optional_callback(
        input_ptr: *const u8,
        input_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
        user_data: *mut std::ffi::c_void,
    ) -> i32 {
        let bytes = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
        let ServiceBridgeRequest::InvokeService(request) = serde_json::from_slice(bytes).unwrap()
        else {
            panic!("expected optional application invocation");
        };
        assert_eq!(request.operation, judgement::OP_OPTIONAL_JUDGE);
        let optional: OptionalRequest = serde_json::from_value(request.payload).unwrap();
        assert_eq!(optional.feature, Feature::SessionCategorization);
        assert_eq!(
            optional.request.state,
            State::Structured(serde_json::json!({
                "tasks": ["debugging"], "domains": ["backend"],
                "sequence": ["debugging", "backend", "gap"]
            }))
        );
        let payload = unsafe { &*user_data.cast::<serde_json::Value>() };
        let encoded = serde_json::to_vec(&ServiceBridgeResponse::Service(
            ToolInvocationServiceResolution::Responded {
                payload: payload.clone(),
            },
        ))
        .unwrap();
        assert!(encoded.len() <= output_capacity);
        unsafe {
            std::ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
            *output_len = encoded.len();
        }
        0
    }

    #[test]
    fn service_decode_preserves_transient_observation_and_legacy_unknown() {
        let observation = judgement::OptionalOperationObservation {
            outcome: judgement::OptionalOperationOutcome::TimedOut,
            verification_invocations: 1,
            elapsed_ms: 37,
            ..judgement::OptionalOperationObservation::default()
        };
        for payload in [
            serde_json::json!({"response": null}),
            serde_json::to_value(OptionalResponse {
                response: None,
                observation: Some(observation.clone()),
            })
            .unwrap(),
            serde_json::to_value(OptionalResponse {
                response: Some(judgement::Response {
                    answers: BTreeMap::new(),
                    usage: None,
                }),
                observation: Some(observation),
            })
            .unwrap(),
        ] {
            let expected = serde_json::from_value::<OptionalResponse>(payload.clone())
                .unwrap()
                .observation;
            let request = CategorizationRequest {
                session_id: SessionId::new(),
                source_sequence: 7,
                newest_request: "fix backend private-token-marker".into(),
            };
            let context = NativeServiceContext {
                plugin_id: "bcode.session-derivation".into(),
                request: bcode_plugin_sdk::ServiceRequest {
                    interface_id: "bcode.session-categorization/v1".into(),
                    operation: "categorize".into(),
                    payload: serde_json::to_vec(&request).unwrap(),
                },
                config: bcode_plugin_sdk::PluginConfigContext::default(),
                events: bcode_plugin_sdk::ServiceEventEmitter::default(),
                cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
                bridge: bcode_plugin_sdk::ServiceBridge::new(
                    Some(optional_callback),
                    std::ptr::from_ref(&payload).cast_mut().cast(),
                    bcode_plugin_sdk::ServiceCancellation::default(),
                ),
                transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
            };
            let reply = invoke(&context);
            assert!(reply.error.is_none());
            let decoded: CategorizationServiceResponse =
                serde_json::from_slice(&reply.payload).unwrap();
            assert!(decoded.categorization.is_none());
            assert_eq!(decoded.observation, expected);
            let value: serde_json::Value = serde_json::from_slice(&reply.payload).unwrap();
            let portable: bcode_session_models::CategorizationResponse = serde_json::from_value(
                serde_json::json!({"categorization": value["categorization"]}),
            )
            .unwrap();
            assert!(portable.categorization.is_none());
        }
        let old: CategorizationServiceResponse =
            serde_json::from_value(serde_json::json!({"categorization": null})).unwrap();
        assert!(old.observation.is_none());
    }

    #[test]
    fn representative_label_inputs_and_ambiguity_follow_actual_signal_path() {
        for (source, task, domain) in [
            (
                "Debug the database persistence failure",
                "debugging",
                "persistence",
            ),
            ("Document the frontend UI", "documentation", "frontend"),
        ] {
            let facts = newest_request_facts(source);
            assert_eq!(facts.tasks, BTreeSet::from([task.to_owned()]));
            assert_eq!(facts.domains, BTreeSet::from([domain.to_owned()]));
            let optional = optional_request(&facts).unwrap();
            assert_eq!(optional.feature, Feature::SessionCategorization);
            assert_eq!(
                optional.request.state,
                State::Structured(serde_json::to_value(facts).unwrap())
            );
        }
        // Ambiguous ordinary input has no trigger, not an invented other/other label.
        assert!(optional_request(&newest_request_facts("Hello, can you help?")).is_none());
    }

    #[test]
    fn choices_cannot_invent_missing_or_unobserved_signals() {
        for (source, tasks, domains) in [
            ("Fix this", vec!["debugging", "other"], vec!["other"]),
            ("backend", vec!["other"], vec!["backend", "other"]),
            (
                "Review and test backend security",
                vec!["other", "review", "testing"],
                vec!["backend", "other", "security"],
            ),
        ] {
            let query = optional_request(&newest_request_facts(source))
                .unwrap()
                .request;
            for (id, expected) in [("task", tasks), ("domain", domains)] {
                let Question::Choice { options, .. } = &query.questions[id] else {
                    panic!("expected choice");
                };
                assert_eq!(
                    options.keys().map(String::as_str).collect::<Vec<_>>(),
                    expected
                );
            }
        }
        let request = CategorizationRequest {
            session_id: SessionId::new(),
            source_sequence: 12,
            newest_request: "Fix this".into(),
        };
        let query = optional_request(&newest_request_facts(&request.newest_request))
            .unwrap()
            .request;
        for (domain, accepted) in [("backend", false), ("other", true)] {
            let result = labels(
                &request,
                &query,
                OptionalResponse {
                    observation: None,
                    response: Some(judgement::Response {
                        answers: BTreeMap::from([
                            (
                                "task".into(),
                                Answer::Choice {
                                    selected: "debugging".into(),
                                    probabilities: None,
                                    confidence: None,
                                },
                            ),
                            (
                                "domain".into(),
                                Answer::Choice {
                                    selected: domain.into(),
                                    probabilities: None,
                                    confidence: None,
                                },
                            ),
                        ]),
                        usage: None,
                    }),
                },
            );
            assert_eq!(result.is_some(), accepted);
        }
    }

    #[test]
    fn ordered_cues_distinguish_opposite_intent_without_resolving_scope() {
        let review = newest_request_facts("Review backend; do not implement frontend");
        let implement = newest_request_facts("Implement frontend; do not review backend");
        assert_eq!(review.tasks, implement.tasks);
        assert_eq!(review.domains, implement.domains);
        assert_eq!(
            review.sequence,
            [
                "review",
                "backend",
                "boundary",
                "gap",
                "negation",
                "implementation",
                "frontend"
            ]
        );
        assert_eq!(
            implement.sequence,
            [
                "implementation",
                "frontend",
                "boundary",
                "gap",
                "negation",
                "review",
                "backend"
            ]
        );
        assert_ne!(
            optional_request(&review).unwrap().request.state,
            optional_request(&implement).unwrap().request.state
        );
        // Unknown words remain explicitly omitted: this representation cannot fully
        // interpret quotations, scope, hypotheticals, or arbitrary language.
        assert_eq!(
            newest_request_facts("review backend if SECRET_CONDITION"),
            newest_request_facts("review backend because PRIVATE_REASON")
        );
    }

    #[test]
    fn ordered_cues_are_bounded_secret_safe_and_reject_injected_text() {
        let facts = newest_request_facts(
            "Do not fix backend token=DO_NOT_SEND /private/customer; instead review frontend only.",
        );
        assert_eq!(
            facts.sequence,
            [
                "gap",
                "negation",
                "debugging",
                "backend",
                "gap",
                "boundary",
                "contrast",
                "review",
                "frontend",
                "only",
                "boundary"
            ]
        );
        let encoded = serde_json::to_string(&optional_request(&facts).unwrap()).unwrap();
        assert!(!encoded.contains("DO_NOT_SEND"));
        assert!(!encoded.contains("/private/customer"));
        let bounded = newest_request_facts(&"fix; ".repeat(600));
        assert_eq!(bounded.sequence.len(), 1024);
        assert_eq!(bounded.tasks, BTreeSet::from(["debugging".into()]));
        let mut injected = facts;
        injected.sequence.push("DO_NOT_SEND".into());
        assert!(optional_request(&injected).is_none());
    }

    #[test]
    fn extraction_is_bounded_and_contains_only_fixed_signals() {
        let facts = newest_request_facts("Fix backend auth token=SECRET /private/path");
        assert_eq!(facts.tasks, BTreeSet::from(["debugging".into()]));
        assert_eq!(
            facts.domains,
            BTreeSet::from(["backend".into(), "security".into()])
        );
        assert_eq!(
            newest_request_facts(&format!("{} fix backend", "x".repeat(8192))),
            NewestRequestFacts::default()
        );
    }
    #[test]
    fn raw_source_never_enters_provider_state() {
        let source = "fix backend token=DO_NOT_SEND /private/customer";
        let optional = optional_request(&newest_request_facts(source)).unwrap();
        let encoded = serde_json::to_string(&optional).unwrap();
        assert!(!encoded.contains("DO_NOT_SEND"));
        assert!(!encoded.contains("/private/customer"));
        assert!(!encoded.contains(source));
    }

    #[test]
    fn empty_or_untrusted_signals_do_not_request_judgement() {
        assert!(optional_request(&NewestRequestFacts::default()).is_none());
        assert!(
            optional_request(&NewestRequestFacts {
                tasks: BTreeSet::from(["SECRET".into()]),
                domains: BTreeSet::new(),
                sequence: Vec::new(),
            })
            .is_none()
        );
    }
    #[test]
    fn validated_labels_retain_source_identity_without_request_facts() {
        let request = CategorizationRequest {
            session_id: SessionId::new(),
            source_sequence: 11,
            newest_request: "fix backend".to_owned(),
        };
        let query = optional_request(&newest_request_facts(&request.newest_request))
            .unwrap()
            .request;
        let response = OptionalResponse {
            observation: None,
            response: Some(judgement::Response {
                answers: BTreeMap::from([
                    (
                        "task".into(),
                        Answer::Choice {
                            selected: "debugging".into(),
                            probabilities: None,
                            confidence: None,
                        },
                    ),
                    (
                        "domain".into(),
                        Answer::Choice {
                            selected: "backend".into(),
                            probabilities: None,
                            confidence: None,
                        },
                    ),
                ]),
                usage: None,
            }),
        };
        assert_eq!(
            labels(&request, &query, response),
            Some(SessionCategorization {
                source_sequence: 11,
                task: "debugging".into(),
                domain: "backend".into()
            })
        );
    }

    #[test]
    fn mismatched_question_kind_option_or_count_is_rejected() {
        let request = CategorizationRequest {
            session_id: SessionId::new(),
            source_sequence: 3,
            newest_request: "fix backend".into(),
        };
        let query = optional_request(&newest_request_facts(&request.newest_request))
            .unwrap()
            .request;
        let choice = |selected: &str| Answer::Choice {
            selected: selected.into(),
            probabilities: None,
            confidence: None,
        };
        for answers in [
            BTreeMap::from([
                ("task".into(), choice("unknown")),
                ("domain".into(), choice("backend")),
            ]),
            BTreeMap::from([
                ("task".into(), Answer::YesNo { probability: 0.5 }),
                ("domain".into(), choice("backend")),
            ]),
            BTreeMap::from([
                ("task".into(), choice("debugging")),
                ("wrong_id".into(), choice("backend")),
            ]),
            BTreeMap::from([
                ("task".into(), choice("debugging")),
                ("domain".into(), choice("backend")),
                ("extra".into(), choice("backend")),
            ]),
        ] {
            assert!(
                labels(
                    &request,
                    &query,
                    OptionalResponse {
                        observation: None,
                        response: Some(judgement::Response {
                            answers,
                            usage: None
                        })
                    }
                )
                .is_none()
            );
        }
    }

    #[test]
    fn unavailable_and_invalid_answers_retain_baseline() {
        let request = CategorizationRequest {
            session_id: SessionId::new(),
            source_sequence: 7,
            newest_request: "fix backend".to_owned(),
        };
        let query = optional_request(&newest_request_facts(&request.newest_request))
            .unwrap()
            .request;
        assert!(
            labels(
                &request,
                &query,
                OptionalResponse {
                    observation: None,
                    response: None
                }
            )
            .is_none()
        );
        assert!(
            labels(
                &request,
                &query,
                OptionalResponse {
                    observation: None,
                    response: Some(judgement::Response {
                        answers: BTreeMap::new(),
                        usage: None
                    })
                }
            )
            .is_none()
        );
    }
}
