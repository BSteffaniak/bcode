//! Advisory categorization of bounded, allowlisted newest-request facts.
//!
//! Default policy is Auto: off/unavailable produces no labels, not a heuristic
//! fallback. Ordered fixed-vocabulary signals retain local negation and boundaries,
//! but do not fully resolve intent; `other` is an abstention,
//! not evidence of an actual miscellaneous task. Probabilities are provider-reported
//! metadata, not calibrated evidence, and do not change label application.

mod intent;

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
    /// Versioned, fixed-vocabulary grammatical evidence; absent in legacy facts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    intent: Option<intent::Intent>,
}

/// Extract signals from at most 8192 characters and 512 words of one request.
/// The returned value contains only fixed vocabulary, regardless of input contents.
#[must_use]
pub fn newest_request_facts(input: &str) -> NewestRequestFacts {
    let bounded = intent::lexical_projection(input);
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
    if !facts.tasks.is_empty() || !facts.domains.is_empty() {
        facts.intent = Some(intent::extract(input));
    }
    facts
}

fn optional_request(facts: &NewestRequestFacts) -> Option<OptionalRequest> {
    if facts.intent.as_ref().is_some_and(|value| !value.valid())
        || facts.tasks.is_empty() && facts.domains.is_empty()
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
    // Lexical cues cannot justify an unobserved label. Omit missing axes;
    // signal-bearing axes retain explicit abstention for ambiguous intent.
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
    let mut questions = BTreeMap::new();
    if !facts.tasks.is_empty() {
        questions.insert(
            "task".to_owned(),
            choice(
                "Choose the requested advisory task using fixed-vocabulary intent version 1 when present, otherwise legacy ordered cues. Purpose/Step/Prohibition/Unresolved are bounded grammatical evidence, not resolved intent: explicit means_of can distinguish a goal from its means, but position alone cannot. Independent unresolved goals, incomplete evidence or uncertain scope may require other; never force a purpose label. negation marks a negative cue, boundary marks punctuation, contrast and only preserve those cues, and gap means omitted unknown words. These cues do not establish negation scope; use other when ambiguous or only prohibited tasks are supported.",
                &facts.tasks,
            ),
        );
    }
    if !facts.domains.is_empty() {
        questions.insert(
            "domain".to_owned(),
            choice(
                "Choose the domain of the requested task using fixed-vocabulary intent evidence when present and legacy ordered cues otherwise. Do not transfer a means or prohibited clause domain onto its purpose. Unresolved associations remain ambiguous. gap means omitted unknown words, not adjacency in the source; use other when intent or domain association is ambiguous.",
                &facts.domains,
            ),
        );
    }
    Some(OptionalRequest {
        feature: Feature::SessionCategorization,
        selection: None,
        request: Request {
            model_id: "auto".to_owned(),
            state: State::Structured(serde_json::to_value(facts).ok()?),
            questions,
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
    if !(1..=2).contains(&query.questions.len())
        || response.answers.len() != query.questions.len()
        || query
            .questions
            .keys()
            .any(|id| !["task", "domain"].contains(&id.as_str()))
    {
        return None;
    }
    for (id, question) in &query.questions {
        let Question::Choice { options, .. } = question else {
            return None;
        };
        let Answer::Choice { selected, .. } = response.answers.get(id)? else {
            return None;
        };
        if options.len() < 2 || !options.contains_key(selected) {
            return None;
        }
    }
    let axis = |id: &str| match response.answers.get(id) {
        Some(Answer::Choice { selected, .. }) => selected.clone(),
        _ => "other".to_owned(),
    };
    Some(SessionCategorization {
        source_sequence: request.source_sequence,
        task: axis("task"),
        domain: axis("domain"),
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

#[cfg(test)]
use bcode_session_models::SessionCategoryRankCandidate as RankCandidate;
use bcode_session_models::{
    SessionCategoryRankRequest as RankRequest, SessionCategoryRankResponse as RankResponse,
};

/// Rank already eligible cards without IO, inference, persistence or title inspection.
///
/// # Errors
/// Rejects future versions, invalid axis vocabulary, oversized pages, duplicate
/// identities and invalid label vocabulary. Stale/missing labels retain baseline.
pub fn rank(request: &RankRequest) -> Result<RankResponse, &'static str> {
    if request.version != 1 || request.candidates.len() > 100 {
        return Err("invalid ranking version or bound");
    }
    if request
        .task
        .as_ref()
        .is_some_and(|v| !TASKS.contains(&v.as_str()))
        || request
            .domain
            .as_ref()
            .is_some_and(|v| !DOMAINS.contains(&v.as_str()))
    {
        return Err("invalid ranking query axis");
    }
    let mut seen = BTreeSet::new();
    let mut matched = Vec::new();
    let mut remaining = Vec::new();
    let has_query = request.task.is_some() || request.domain.is_some();
    for candidate in &request.candidates {
        if !seen.insert(candidate.session_id) {
            return Err("duplicate ranking identity");
        }
        if candidate.categorization.as_ref().is_some_and(|label| {
            !TASKS.contains(&label.task.as_str()) || !DOMAINS.contains(&label.domain.as_str())
        }) {
            return Err("invalid ranking label axis");
        }
        let eligible = candidate.categorization.as_ref().is_some_and(|label| {
            has_query
                && candidate.source_sequence != 0
                && label.source_sequence == candidate.source_sequence
                && request
                    .task
                    .as_ref()
                    .is_none_or(|task| task != "other" && task == &label.task)
                && request
                    .domain
                    .as_ref()
                    .is_none_or(|domain| domain != "other" && domain == &label.domain)
        });
        if eligible {
            matched.push(candidate.session_id);
        } else {
            remaining.push(candidate.session_id);
        }
    }
    let ordered: Vec<_> = matched.iter().chain(&remaining).copied().collect();
    let applied = ordered
        .iter()
        .copied()
        .ne(request.candidates.iter().map(|c| c.session_id));
    Ok(RankResponse {
        version: 1,
        ordered_session_ids: ordered,
        matched_session_ids: matched,
        applied,
    })
}

/// Decode bounded native service data and return the portable advisory ranking.
pub(crate) fn invoke_rank(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.payload.len() > 65_536 {
        return ServiceResponse::error("invalid_request", "ranking input exceeds bound");
    }
    let Ok(request) = context.request.payload_json::<RankRequest>() else {
        return ServiceResponse::error("invalid_request", "invalid ranking input");
    };
    rank(&request).map_or_else(
        |_| ServiceResponse::error("invalid_request", "invalid ranking input"),
        |response| {
            ServiceResponse::json(&response).unwrap_or_else(|_| {
                ServiceResponse::error("serialization_failed", "ranking unavailable")
            })
        },
    )
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
    fn card(task: Option<&str>, domain: &str) -> RankCandidate {
        RankCandidate {
            session_id: SessionId::new(),
            source_sequence: 9,
            categorization: task.map(|task| SessionCategorization {
                source_sequence: 9,
                task: task.into(),
                domain: domain.into(),
            }),
        }
    }

    #[test]
    fn native_ranking_is_stable_axis_specific_and_title_free() {
        let cards = vec![
            card(None, "other"),
            card(Some("documentation"), "backend"),
            card(Some("documentation"), "tooling"),
            card(Some("documentation"), "tooling"),
        ];
        let request = RankRequest {
            version: 1,
            task: Some("documentation".into()),
            domain: Some("tooling".into()),
            candidates: cards.clone(),
        };
        let response = rank(&request).unwrap();
        assert_eq!(
            response.ordered_session_ids,
            vec![
                cards[2].session_id,
                cards[3].session_id,
                cards[0].session_id,
                cards[1].session_id
            ]
        );
        assert_eq!(
            response.matched_session_ids,
            vec![cards[2].session_id, cards[3].session_id]
        );
        assert!(response.applied);
        let mut task_only = request.clone();
        task_only.domain = None;
        assert_eq!(
            rank(&task_only).unwrap().matched_session_ids,
            cards[1..].iter().map(|c| c.session_id).collect::<Vec<_>>()
        );
        let mut wrong_axis = request;
        wrong_axis.task = Some("tooling".into());
        assert!(rank(&wrong_axis).is_err());
        assert!(
            serde_json::from_value::<RankRequest>(serde_json::json!({
                "version":1,"task":null,"domain":null,"candidates":[],"title":"answer leak"
            }))
            .is_err()
        );
    }

    #[test]
    fn native_rank_routes_without_optional_bridge_and_decodes_portable_response() {
        use bcode_plugin_sdk::RustPlugin;
        let request = RankRequest {
            version: 1,
            task: Some("documentation".into()),
            domain: Some("tooling".into()),
            candidates: vec![card(None, "other"), card(Some("documentation"), "tooling")],
        };
        let expected = rank(&request).unwrap();
        let context = NativeServiceContext {
            diagnostics: None,
            plugin_id: "bcode.session-derivation".into(),
            request: bcode_plugin_sdk::ServiceRequest {
                interface_id: bcode_session_models::CATEGORIZATION_INTERFACE_ID.into(),
                operation: bcode_session_models::OP_RANK_CATEGORIZATION.into(),
                payload: serde_json::to_vec(&request).unwrap(),
            },
            config: bcode_plugin_sdk::PluginConfigContext::default(),
            events: bcode_plugin_sdk::ServiceEventEmitter::default(),
            cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            bridge: bcode_plugin_sdk::ServiceBridge::new(
                None,
                std::ptr::null_mut(),
                bcode_plugin_sdk::ServiceCancellation::default(),
            ),
            transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
        };
        let mut plugin = crate::SessionDerivationPlugin;
        let reply = plugin.invoke_service(context);
        assert!(reply.error.is_none());
        let portable: bcode_session_models::SessionCategoryRankResponse =
            serde_json::from_slice(&reply.payload).unwrap();
        assert_eq!(portable, expected);
    }

    #[test]
    fn missing_stale_other_and_absent_query_preserve_baseline() {
        let mut cards = vec![card(None, "other"), card(Some("documentation"), "tooling")];
        cards[1].categorization.as_mut().unwrap().source_sequence = 8;
        let mut request = RankRequest {
            version: 1,
            task: Some("documentation".into()),
            domain: None,
            candidates: cards,
        };
        let baseline: Vec<_> = request.candidates.iter().map(|c| c.session_id).collect();
        for task in [Some("documentation".into()), Some("other".into()), None] {
            request.task = task;
            let response = rank(&request).unwrap();
            assert_eq!(response.ordered_session_ids, baseline);
            assert!(response.matched_session_ids.is_empty());
            assert!(!response.applied);
        }
        request.candidates[1]
            .categorization
            .as_mut()
            .unwrap()
            .source_sequence = 9;
        request.task = Some("other".into());
        assert!(rank(&request).unwrap().matched_session_ids.is_empty());
        request.candidates.push(request.candidates[0].clone());
        assert!(rank(&request).is_err());
        request.candidates.pop();
        request.version = 2;
        assert!(rank(&request).is_err());
        request.version = 1;
        request.candidates = (0..101).map(|_| card(None, "other")).collect();
        assert!(rank(&request).is_err());
    }

    struct OptionalCallbackData {
        response: Vec<u8>,
        captured: std::sync::Mutex<Vec<Vec<u8>>>,
    }

    extern "C" fn optional_callback(
        input_ptr: *const u8,
        input_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
        user_data: *mut std::ffi::c_void,
    ) -> i32 {
        let data = unsafe { &*user_data.cast::<OptionalCallbackData>() };
        if data.response.len() > output_capacity {
            return -1;
        }
        let Ok(mut captured) = data.captured.lock() else {
            return -1;
        };
        let bytes = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
        captured.push(bytes.to_vec());
        drop(captured);
        unsafe {
            std::ptr::copy_nonoverlapping(data.response.as_ptr(), output_ptr, data.response.len());
            *output_len = data.response.len();
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
            let callback = OptionalCallbackData {
                response: serde_json::to_vec(&ServiceBridgeResponse::Service(
                    ToolInvocationServiceResolution::Responded {
                        payload: payload.clone(),
                    },
                ))
                .unwrap(),
                captured: std::sync::Mutex::new(Vec::new()),
            };
            let request = CategorizationRequest {
                session_id: SessionId::new(),
                source_sequence: 7,
                newest_request: "fix backend private-token-marker".into(),
            };
            let context = NativeServiceContext {
                diagnostics: None,
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
                    std::ptr::from_ref(&callback).cast_mut().cast(),
                    bcode_plugin_sdk::ServiceCancellation::default(),
                ),
                transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
            };
            let reply = invoke(&context);
            drop(context);
            let captured = callback.captured.into_inner().unwrap();
            assert_eq!(captured.len(), 1);
            let ServiceBridgeRequest::InvokeService(invocation) =
                serde_json::from_slice(&captured[0]).unwrap()
            else {
                panic!("expected optional application invocation");
            };
            assert_eq!(invocation.operation, judgement::OP_OPTIONAL_JUDGE);
            let optional: OptionalRequest = serde_json::from_value(invocation.payload).unwrap();
            assert_eq!(optional.feature, Feature::SessionCategorization);
            assert_eq!(
                optional.request.state,
                State::Structured(serde_json::json!({
                    "tasks": ["debugging"], "domains": ["backend"],
                    "sequence": ["debugging", "backend", "gap"],
                    "intent": {"version": 1, "complete": true, "clauses": [{
                        "ordinal": 0, "role": "unresolved", "tasks": ["debugging"],
                        "domains": ["backend"], "means_of": null
                    }]}
                }))
            );
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
            ("Fix this", vec!["debugging", "other"], vec![]),
            ("backend", vec![], vec!["backend", "other"]),
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
                if expected.is_empty() {
                    assert!(!query.questions.contains_key(id));
                    continue;
                }
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
        for (task, domain, accepted) in [
            ("debugging", None, true),
            ("implementation", None, false),
            ("debugging", Some("backend"), false),
            ("debugging", Some("other"), false),
        ] {
            let mut answers = BTreeMap::from([(
                "task".into(),
                Answer::Choice {
                    selected: task.into(),
                    probabilities: None,
                    confidence: None,
                },
            )]);
            if let Some(domain) = domain {
                answers.insert(
                    "domain".into(),
                    Answer::Choice {
                        selected: domain.into(),
                        probabilities: None,
                        confidence: None,
                    },
                );
            }
            let result = labels(
                &request,
                &query,
                OptionalResponse {
                    observation: None,
                    response: Some(judgement::Response {
                        answers,
                        usage: None,
                    }),
                },
            );
            if accepted {
                assert_eq!(
                    result,
                    Some(SessionCategorization {
                        source_sequence: 12,
                        task: "debugging".into(),
                        domain: "other".into(),
                    })
                );
            } else {
                assert!(result.is_none());
            }
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
    fn intent_provider_state_is_typed_secret_safe_and_legacy_compatible() {
        let source = "Document tooling by implementing token=SECRET /private/customer.";
        let facts = newest_request_facts(source);
        let query = optional_request(&facts).unwrap();
        assert_eq!(query.request.questions.len(), 2);
        let value = serde_json::to_value(&query).unwrap();
        let encoded = value.to_string();
        for secret in ["SECRET", "token=", "/private", "customer", "implementing"] {
            assert!(!encoded.contains(secret));
        }
        let mut legacy = serde_json::to_value(&facts).unwrap();
        legacy.as_object_mut().unwrap().remove("intent");
        let legacy: NewestRequestFacts = serde_json::from_value(legacy).unwrap();
        assert!(optional_request(&legacy).is_some());
        let mut future = serde_json::to_value(&facts).unwrap();
        future["intent"]["version"] = serde_json::json!(2);
        let future: NewestRequestFacts = serde_json::from_value(future).unwrap();
        assert!(optional_request(&future).is_none());
    }

    #[test]
    fn quoted_and_identifier_only_sources_do_not_request_judgement() {
        for source in [
            "\"fix backend\"",
            "`document tooling`",
            "token=fix /private/backend tooling_document",
        ] {
            assert!(optional_request(&newest_request_facts(source)).is_none());
        }
    }

    #[test]
    fn intent_keeps_abstention_and_unavailable_response_behavior() {
        let request = CategorizationRequest {
            session_id: bcode_session_models::SessionId::new(),
            source_sequence: 1,
            newest_request: "Document tooling by implementing an example. Implement backend."
                .to_owned(),
        };
        let query = optional_request(&newest_request_facts(&request.newest_request)).unwrap();
        assert!(query.request.questions.values().all(|question| matches!(question, Question::Choice { options, .. } if options.contains_key("other"))));
        // Intent adds no local fallback labels or additional inference question.
        assert_eq!(query.request.questions.len(), 2);
        assert!(
            labels(
                &request,
                &query.request,
                OptionalResponse {
                    observation: None,
                    response: None
                }
            )
            .is_none()
        );
        let answers = query
            .request
            .questions
            .keys()
            .map(|id| {
                (
                    id.clone(),
                    Answer::Choice {
                        selected: "other".to_owned(),
                        probabilities: None,
                        confidence: None,
                    },
                )
            })
            .collect();
        let result = labels(
            &request,
            &query.request,
            OptionalResponse {
                observation: None,
                response: Some(judgement::Response {
                    answers,
                    usage: None,
                }),
            },
        )
        .unwrap();
        assert_eq!(result.task, "other");
        assert_eq!(result.domain, "other");
    }

    #[test]
    fn empty_or_untrusted_signals_do_not_request_judgement() {
        assert!(optional_request(&NewestRequestFacts::default()).is_none());
        assert!(
            optional_request(&NewestRequestFacts {
                tasks: BTreeSet::from(["SECRET".into()]),
                domains: BTreeSet::new(),
                sequence: Vec::new(),
                intent: None,
            })
            .is_none()
        );
    }
    #[test]
    fn single_signal_axis_omits_missing_question_and_defaults_its_label() {
        for (source, id, selected, task, domain) in [
            ("backend", "domain", "backend", "other", "backend"),
            ("debug", "task", "debugging", "debugging", "other"),
        ] {
            let request = CategorizationRequest {
                session_id: SessionId::new(),
                source_sequence: 13,
                newest_request: source.to_owned(),
            };
            let query = optional_request(&newest_request_facts(source))
                .unwrap()
                .request;
            assert_eq!(query.questions.len(), 1);
            let Question::Choice { options, .. } = &query.questions[id] else {
                panic!("expected choice");
            };
            assert_eq!(options.len(), 2);
            assert!(options.contains_key(selected));
            assert!(options.contains_key("other"));
            let response = OptionalResponse {
                observation: None,
                response: Some(judgement::Response {
                    answers: BTreeMap::from([(
                        id.to_owned(),
                        Answer::Choice {
                            selected: selected.to_owned(),
                            probabilities: None,
                            confidence: None,
                        },
                    )]),
                    usage: None,
                }),
            };
            assert_eq!(
                labels(&request, &query, response),
                Some(SessionCategorization {
                    source_sequence: 13,
                    task: task.to_owned(),
                    domain: domain.to_owned(),
                })
            );
        }
        assert!(optional_request(&newest_request_facts("hello world")).is_none());
    }

    #[test]
    fn single_axis_rejects_wrong_answer_kind_and_unknown_authored_id() {
        let request = CategorizationRequest {
            session_id: SessionId::new(),
            source_sequence: 14,
            newest_request: "backend".to_owned(),
        };
        let query = optional_request(&newest_request_facts("backend"))
            .unwrap()
            .request;
        let response = |answers| OptionalResponse {
            observation: None,
            response: Some(judgement::Response {
                answers,
                usage: None,
            }),
        };
        assert!(
            labels(
                &request,
                &query,
                response(BTreeMap::from([(
                    "domain".to_owned(),
                    Answer::YesNo { probability: 0.5 }
                ),]))
            )
            .is_none()
        );
        let mut unknown = query.clone();
        let question = unknown.questions.remove("domain").unwrap();
        unknown.questions.insert("unexpected".to_owned(), question);
        assert!(
            labels(
                &request,
                &unknown,
                response(BTreeMap::from([(
                    "unexpected".to_owned(),
                    Answer::Choice {
                        selected: "backend".to_owned(),
                        probabilities: None,
                        confidence: None,
                    }
                ),]))
            )
            .is_none()
        );
        let mut singleton = query;
        if let Question::Choice { options, .. } = singleton.questions.get_mut("domain").unwrap() {
            options.retain(|id, _| id == "other");
        }
        assert!(
            labels(
                &request,
                &singleton,
                response(BTreeMap::from([(
                    "domain".to_owned(),
                    Answer::Choice {
                        selected: "other".to_owned(),
                        probabilities: None,
                        confidence: None,
                    }
                ),]))
            )
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
