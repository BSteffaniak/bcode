use super::*;

fn selection(purpose: Purpose) -> SelectionRequest {
    SelectionRequest {
        purpose,
        task: "Fix the failing parser and validate the change".into(),
        candidates: vec![
            Candidate {
                id: "low".into(),
                description: "Release announcement context".into(),
                protected: false,
            },
            Candidate {
                id: "explicit".into(),
                description: "Explicit user selection".into(),
                protected: true,
            },
            Candidate {
                id: "high".into(),
                description: "Parser diagnostics and focused tests".into(),
                protected: false,
            },
        ],
    }
}

fn advice(selection: SelectionRequest, low: f64, high: f64) -> ApplyRequest {
    ApplyRequest {
        selection,
        judgement: OptionalResponse {
            observation: None,
            response: Some(judgement::Response {
                answers: BTreeMap::from([
                    ("candidate_0".into(), Answer::YesNo { probability: low }),
                    ("candidate_2".into(), Answer::YesNo { probability: high }),
                ]),
                usage: None,
            }),
        },
    }
}

#[test]
fn separate_features_preserve_explicit_candidates_and_complete_permutation() {
    for (purpose, feature) in [
        (Purpose::Context, Feature::ContextReranking),
        (Purpose::Recommendations, Feature::Recommendations),
    ] {
        let input = selection(purpose);
        let prepared = prepare(&input).judgement.unwrap();
        assert_eq!(prepared.feature, feature);
        assert!(prepared.selection.is_none());
        assert_eq!(prepared.request.questions.len(), 2);
        assert!(!prepared.request.questions.contains_key("candidate_1"));
        assert!(judgement::validate_request(&prepared.request).is_ok());
        let result = apply(&advice(input, 0.1, 0.9));
        assert_eq!(result.ordered_ids, ["high", "explicit", "low"]);
        assert!(result.applied);
    }
}

#[test]
fn absent_uncertain_invalid_or_incomplete_results_keep_baseline() {
    let input = selection(Purpose::Context);
    let expected = vec!["low", "explicit", "high"];
    for (low, high) in [
        (0.5, 0.9),
        (0.1, 0.65),
        (f64::NAN, 0.9),
        (-0.1, 0.9),
        (0.1, 1.1),
    ] {
        let result = apply(&advice(input.clone(), low, high));
        assert_eq!(result.ordered_ids, expected);
        assert!(!result.applied);
    }
    let mut absent = advice(input.clone(), 0.1, 0.9);
    absent.judgement.response = None;
    assert_eq!(apply(&absent).ordered_ids, expected);
    let mut incomplete = advice(input.clone(), 0.1, 0.9);
    incomplete
        .judgement
        .response
        .as_mut()
        .unwrap()
        .answers
        .remove("candidate_2");
    assert_eq!(apply(&incomplete).ordered_ids, expected);
    let mut injected = advice(input, 0.1, 0.9);
    injected
        .judgement
        .response
        .as_mut()
        .unwrap()
        .answers
        .insert("foreign".into(), Answer::YesNo { probability: 1.0 });
    assert_eq!(apply(&injected).ordered_ids, expected);
}

#[test]
fn stable_ties_and_protected_only_shortlists_do_not_change_baseline() {
    let mut input = selection(Purpose::Recommendations);
    let result = apply(&advice(input.clone(), 0.9, 0.9));
    assert_eq!(result.ordered_ids, ["low", "explicit", "high"]);
    assert!(!result.applied);
    for candidate in &mut input.candidates {
        candidate.protected = true;
    }
    assert!(prepare(&input).judgement.is_none());
}

#[test]
fn meaningful_bounded_triggers_only() {
    let mut input = selection(Purpose::Context);
    input.task.clear();
    assert!(prepare(&input).judgement.is_none());
    input.task = "x".repeat(8193);
    assert!(prepare(&input).judgement.is_none());
    input.task = "task".into();
    input.candidates[2].id = input.candidates[0].id.clone();
    assert!(prepare(&input).judgement.is_none());
    input.candidates[2].id = "high".into();
    input.candidates[2].description = "x".repeat(2049);
    assert!(prepare(&input).judgement.is_none());
    input.candidates[2].description = "parser".into();
    input.candidates = (0..=MAX_CANDIDATES)
        .map(|index| Candidate {
            id: index.to_string(),
            description: "candidate".into(),
            protected: false,
        })
        .collect();
    assert!(prepare(&input).judgement.is_none());
}

#[test]
fn context_advice_changes_actual_optional_catalog_prompt_without_loading_instructions() {
    use bcode_skill_models::{
        SkillActivation, SkillId, SkillList, SkillSource, SkillSourceKind, SkillSummary,
    };
    let input = selection(Purpose::Context);
    let ranked = apply(&advice(input.clone(), 0.1, 0.9));
    let make_skill = |candidate: &Candidate| SkillSummary {
        id: SkillId::new(&candidate.id),
        name: candidate.id.clone(),
        description: Some(candidate.description.clone()),
        version: None,
        source: SkillSource {
            kind: SkillSourceKind::Repository,
            label: "repo".into(),
            path: None,
            precedence: 0,
        },
        activation: SkillActivation::default(),
        diagnostics: Vec::new(),
        disable_model_invocation: false,
    };
    let baseline = SkillList {
        skills: input.candidates.iter().map(make_skill).collect(),
        diagnostics: Vec::new(),
    };
    let mut reordered = baseline.clone();
    reordered.skills = ranked
        .ordered_ids
        .iter()
        .map(|id| {
            baseline
                .skills
                .iter()
                .find(|skill| skill.id.as_str() == id)
                .unwrap()
                .clone()
        })
        .collect();
    let options = bcode_skill::SkillPromptCatalogOptions::default();
    let before = bcode_skill::format_skill_catalog_for_prompt(&baseline, &options);
    let after = bcode_skill::format_skill_catalog_for_prompt(&reordered, &options);
    assert!(before.find("<id>low</id>").unwrap() < before.find("<id>high</id>").unwrap());
    assert!(after.find("<id>high</id>").unwrap() < after.find("<id>low</id>").unwrap());
    assert_eq!(reordered.skills[1], baseline.skills[1]);
    for candidate in &input.candidates {
        assert!(after.contains(&format!("<id>{}</id>", candidate.id)));
        assert!(after.contains(&candidate.description));
    }
}

#[test]
fn usefulness_probabilities_do_not_rank_within_the_same_band() {
    for purpose in [Purpose::Context, Purpose::Recommendations] {
        for (low, high) in [(0.7, 0.99), (0.01, 0.3), (0.0, 0.34), (0.66, 1.0)] {
            let result = apply(&advice(selection(purpose), low, high));
            assert_eq!(result.ordered_ids, ["low", "explicit", "high"]);
            assert!(!result.applied);
        }
        let result = apply(&advice(selection(purpose), 0.34, 0.66));
        assert_eq!(result.ordered_ids, ["high", "explicit", "low"]);
        assert!(result.applied);
    }
}

#[test]
fn stable_usefulness_partition_retains_each_band_and_protected_slots() {
    let mut input = selection(Purpose::Recommendations);
    input.candidates.extend([
        Candidate {
            id: "other-low".into(),
            description: "Other irrelevant candidate".into(),
            protected: false,
        },
        Candidate {
            id: "other-high".into(),
            description: "Other useful candidate".into(),
            protected: false,
        },
    ]);
    let mut request = advice(input, 0.01, 0.7);
    request
        .judgement
        .response
        .as_mut()
        .unwrap()
        .answers
        .extend([
            ("candidate_3".into(), Answer::YesNo { probability: 0.3 }),
            ("candidate_4".into(), Answer::YesNo { probability: 0.99 }),
        ]);
    let result = apply(&request);
    assert_eq!(
        result.ordered_ids,
        ["high", "explicit", "other-high", "low", "other-low"]
    );
    assert!(result.applied);
}

#[test]
fn actual_service_dispatch_roundtrip_supports_both_features() {
    use bcode_plugin_sdk::prelude::*;
    for purpose in [Purpose::Context, Purpose::Recommendations] {
        let input = selection(purpose);
        let prepare_request = ServiceRequest {
            interface_id: INTERFACE_ID.into(),
            operation: OP_PREPARE.into(),
            payload: serde_json::to_vec(&input).unwrap(),
        };
        let prepared = crate::selection_service(&prepare_request);
        let prepared: PreparedSelection = serde_json::from_slice(&prepared.payload).unwrap();
        assert!(prepared.judgement.is_some());
        let request = ServiceRequest {
            interface_id: INTERFACE_ID.into(),
            operation: OP_APPLY.into(),
            payload: serde_json::to_vec(&advice(input, 0.1, 0.9)).unwrap(),
        };
        let result = crate::selection_service(&request);
        let result: SelectionResult = serde_json::from_slice(&result.payload).unwrap();
        assert_eq!(result.ordered_ids, ["high", "explicit", "low"]);
    }
}
