use super::*;
use bcode_model::judgement::{Answer, Response};
use std::collections::BTreeMap;

fn fixture() -> SelectionRequest {
    let mut candidates: Vec<_> = [
        (
            "read-distractor",
            Source::File,
            "filesystem.read:call-a:path-a:lines:1-2",
            "release notes unrelated to parser",
        ),
        (
            "explicit",
            Source::ToolResult,
            "tool:call-explicit",
            "explicit user requested evidence",
        ),
        (
            "search-relevant",
            Source::Search,
            "session-search:session-a:event:42:hit-b",
            "parser regression fixed by token lookahead",
        ),
    ]
    .into_iter()
    .map(|(id, source, provenance, snippet)| Candidate {
        id: id.into(),
        source,
        provenance: provenance.into(),
        snippet: snippet.into(),
        protected: id == "explicit",
    })
    .collect();
    // Whole-snippet budget admits either source, but not both.
    let budget_bytes = candidates[2].snippet.len();
    candidates[0].snippet.push_str(" noise");
    SelectionRequest {
        task: "Fix parser lookahead regression".into(),
        candidates,
        budget_bytes: Some(budget_bytes),
    }
}

fn advised(selection: SelectionRequest) -> ApplyRequest {
    ApplyRequest {
        selection,
        judgement: OptionalResponse {
            observation: None,
            response: Some(Response {
                answers: BTreeMap::from([
                    ("candidate_0".into(), Answer::YesNo { probability: 0.1 }),
                    ("candidate_2".into(), Answer::YesNo { probability: 0.9 }),
                ]),
                usage: None,
            }),
        },
    }
}

#[test]
fn constrained_budget_admits_relevant_original_evidence_not_distractor() {
    let input = fixture();
    let original = input.clone();
    let prepared = prepare(&input).judgement.unwrap();
    assert_eq!(
        prepared.feature,
        bcode_model::judgement::Feature::ContextReranking
    );
    assert!(!prepared.request.questions.contains_key("candidate_1"));
    assert!(bcode_model::judgement::validate_request(&prepared.request).is_ok());
    let result = apply(&advised(input.clone()));
    assert_eq!(
        result.ordered_ids,
        ["search-relevant", "explicit", "read-distractor"]
    );
    assert_eq!(result.included_ids, ["search-relevant", "explicit"]);
    assert!(result.applied);
    // Admission names original content, never model-generated replacement text/provenance.
    let admitted: Vec<_> = result
        .included_ids
        .iter()
        .map(|id| {
            input
                .candidates
                .iter()
                .find(|candidate| &candidate.id == id)
                .unwrap()
        })
        .collect();
    assert_eq!(admitted[0], &original.candidates[2]);
    assert_eq!(admitted[1], &original.candidates[1]);
    assert_eq!(input, original);
}

#[test]
fn off_no_key_error_uncertainty_and_malformed_advice_have_identical_baseline() {
    let input = fixture();
    let baseline = apply(&ApplyRequest {
        selection: input.clone(),
        judgement: OptionalResponse {
            observation: None,
            response: None,
        },
    });
    assert_eq!(baseline.included_ids, ["read-distractor", "explicit"]);
    assert!(!baseline.applied);
    for probability in [0.5, f64::NAN, 1.1] {
        let mut request = advised(input.clone());
        request
            .judgement
            .response
            .as_mut()
            .unwrap()
            .answers
            .insert("candidate_0".into(), Answer::YesNo { probability });
        assert_eq!(apply(&request), baseline);
    }
    let mut incomplete = advised(input.clone());
    incomplete
        .judgement
        .response
        .as_mut()
        .unwrap()
        .answers
        .remove("candidate_2");
    assert_eq!(apply(&incomplete), baseline);
    let mut foreign = advised(input);
    foreign
        .judgement
        .response
        .as_mut()
        .unwrap()
        .answers
        .insert("foreign".into(), Answer::YesNo { probability: 1.0 });
    assert_eq!(apply(&foreign), baseline);
}

#[test]
fn no_budget_pressure_no_optional_advice_and_protected_slots_survive_zero_budget() {
    let mut input = fixture();
    input.budget_bytes = Some(0);
    assert!(prepare(&input).judgement.is_none());
    let zero = apply(&advised(input.clone()));
    assert_eq!(zero.included_ids, ["explicit"]);
    assert_eq!(
        zero.ordered_ids,
        ["read-distractor", "explicit", "search-relevant"]
    );
    input.budget_bytes = Some(usize::MAX);
    assert!(prepare(&input).judgement.is_none());
    let all = apply(&advised(input));
    assert_eq!(all.included_ids, all.ordered_ids);
    assert!(!all.applied);
}

#[test]
fn invalid_duplicate_unprovenanced_or_oversized_sources_are_not_ranked() {
    for case in 0..3 {
        let mut input = fixture();
        match case {
            0 => input.candidates[2].id = input.candidates[0].id.clone(),
            1 => input.candidates[2].provenance.clear(),
            _ => input.candidates[2].snippet = "x".repeat(2049),
        }
        assert!(prepare(&input).judgement.is_none());
        assert!(apply(&advised(input)).included_ids.is_empty());
    }
}

#[test]
fn pressure_is_only_among_snippets_that_can_fit_individually() {
    for budget in [1, 2, 3, 4] {
        let mut input = fixture();
        input.candidates[0].snippet = "xxxxx".into();
        input.candidates[2].snippet = "xx".into();
        input.budget_bytes = Some(budget);
        assert!(prepare(&input).judgement.is_none());
        let baseline = apply(&ApplyRequest {
            selection: input.clone(),
            judgement: OptionalResponse {
                observation: None,
                response: None,
            },
        });
        assert_eq!(apply(&advised(input)), baseline);
        assert!(!baseline.applied);
    }
}

#[test]
fn producer_owned_pressure_and_protected_data_keep_the_existing_contract() {
    let mut input = fixture();
    input.budget_bytes = None;
    let prepared = prepare(&input).judgement.unwrap();
    let serialized = serde_json::to_string(&prepared).unwrap();
    assert!(!serialized.contains("explicit user requested evidence"));
    assert!(!serialized.contains("tool:call-explicit"));
    let result = apply(&advised(input));
    assert_eq!(result.included_ids, result.ordered_ids);
    assert!(result.applied);
}

#[test]
fn whole_utf8_snippets_never_split_and_protected_content_is_not_transferred() {
    let mut input = fixture();
    input.candidates[0].snippet = "界🦀".into();
    input.candidates[2].snippet = "界".into();
    input.budget_bytes = Some(3);
    assert!(prepare(&input).judgement.is_none());
    let result = apply(&advised(input));
    assert_eq!(result.included_ids, ["explicit", "search-relevant"]);
    assert!(!result.applied);
}
