#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Offline quality-fixture and current portable-contract tests, not live model evaluation.

#[path = "fixtures/detailed/mod.rs"]
mod detailed;

use bcode_recap::contracts::{BlockContent, Depth, Error, RecapDocument};
use detailed::{Case, Corpus, corpus, coverage};

fn case<'a>(corpus: &'a Corpus, id: &str) -> &'a Case {
    corpus.cases.iter().find(|case| case.id == id).unwrap()
}

#[test]
fn canonical_detailed_covers_all_items_while_standard_is_selective() {
    for case in corpus().cases {
        let detailed = case.expected_document(Depth::Detailed);
        let standard = case.expected_document(Depth::Standard);
        assert!(coverage(&case, &detailed, Depth::Detailed).is_complete());
        assert!(coverage(&case, &standard, Depth::Standard).is_complete());
        assert!(standard.sections.len() < detailed.sections.len());
        assert!(
            !coverage(&case, &standard, Depth::Detailed)
                .missing
                .is_empty()
        );
        let encoded = serde_json::to_vec(&detailed).unwrap();
        assert_eq!(RecapDocument::from_json_slice(&encoded).unwrap(), detailed);
    }
}

#[test]
fn quantities_examples_and_caveats_are_not_covered_by_a_topic_heading() {
    let corpus = corpus();
    let case = case(&corpus, "dense_numbers_examples_caveats");
    let mut document = case.expected_document(Depth::Detailed);
    document.sections[2].title = case.items[2].summary.clone();
    document.sections[2].blocks[0].content = BlockContent::Text {
        text: "A queue sizing example is discussed.".into(),
    };
    document.sections[3].blocks[0].content = BlockContent::Text {
        text: "Some limitations apply.".into(),
    };
    document.validate().unwrap();
    let report = coverage(case, &document, Depth::Detailed);
    assert_eq!(report.missing, ["worked_arithmetic", "scope_limit"]);
}

#[test]
fn wrong_units_or_a_dropped_negation_do_not_pass_the_canonical_oracle() {
    let corpus = corpus();
    let case = case(&corpus, "dense_numbers_examples_caveats");
    let mut document = case.expected_document(Depth::Detailed);
    let BlockContent::Text { text } = &mut document.sections[0].blocks[0].content else {
        panic!("expected canonical text block");
    };
    *text = text.replace("18 ms", "18 seconds");
    let BlockContent::Text { text } = &mut document.sections[3].blocks[0].content else {
        panic!("expected canonical text block");
    };
    *text = text.replace("were not measured", "were measured");
    document.validate().unwrap();
    assert_eq!(
        coverage(case, &document, Depth::Detailed).missing,
        ["baseline_quantities", "scope_limit"]
    );
}

#[test]
fn portable_contract_retains_more_than_twelve_substantive_sections() {
    let corpus = corpus();
    let case = case(&corpus, "late_video_details");
    let document = case.expected_document(Depth::Detailed);
    assert_eq!(document.sections.len(), 16);
    assert_eq!(document.source.duration_ms, 1_800_000);
    assert_eq!(document.evidence.last().unwrap().range.start_ms, 1_780_000);
    let bytes = serde_json::to_vec(&document).unwrap();
    let restored = RecapDocument::from_json_slice(&bytes).unwrap();
    assert!(coverage(case, &restored, Depth::Detailed).is_complete());
}

#[test]
fn an_early_cutoff_loses_late_exceptions_and_the_final_correction() {
    let corpus = corpus();
    let case = case(&corpus, "late_video_details");
    let mut document = case.expected_document(Depth::Detailed);
    document.sections.truncate(12);
    document.validate().unwrap();
    assert_eq!(
        coverage(case, &document, Depth::Detailed).missing,
        [
            "termination",
            "probe_bias",
            "firmware_exception",
            "final_correction"
        ]
    );
}

#[test]
fn correct_words_with_wrong_evidence_are_not_accepted_as_grounded() {
    let corpus = corpus();
    let case = case(&corpus, "dense_numbers_examples_caveats");
    let mut document = case.expected_document(Depth::Detailed);
    document.sections[5].blocks[0].evidence_ids = vec!["baseline".into()];
    // Reference validity alone cannot establish factual support.
    document.validate().unwrap();
    assert_eq!(
        coverage(case, &document, Depth::Detailed).unsupported,
        ["ambiguous_write"]
    );
}

#[test]
fn tutorial_cannot_swap_before_verification_even_if_all_facts_survive() {
    let corpus = corpus();
    let case = case(&corpus, "tutorial_order");
    let mut document = case.expected_document(Depth::Detailed);
    document.sections.swap(3, 4);
    document.validate().unwrap();
    let report = coverage(case, &document, Depth::Detailed);
    assert!(report.missing.is_empty());
    assert_eq!(report.out_of_order, ["atomic_swap"]);
}

#[test]
fn grouping_tutorial_steps_does_not_require_a_chapter_per_step() {
    let corpus = corpus();
    let case = case(&corpus, "tutorial_order");
    let mut document = case.expected_document(Depth::Detailed);
    let blocks = document
        .sections
        .iter()
        .flat_map(|section| section.blocks.clone())
        .collect();
    document.sections.truncate(1);
    document.sections[0].blocks = blocks;
    document.validate().unwrap();
    assert!(coverage(case, &document, Depth::Detailed).is_complete());
}

#[test]
fn repetition_merges_equivalent_support_without_losing_the_late_exception() {
    let corpus = corpus();
    let case = case(&corpus, "repetition");
    let mut document = case.expected_document(Depth::Detailed);
    document.sections[0].blocks[0].evidence_ids = vec!["repeat".into()];
    assert!(coverage(case, &document, Depth::Detailed).is_complete());
    let mut duplicate = document.sections[0].blocks[0].clone();
    duplicate.id = "duplicate_restatement".into();
    document.sections[0].blocks.push(duplicate);
    document.sections.pop();
    document.validate().unwrap();
    let report = coverage(case, &document, Depth::Detailed);
    assert_eq!(report.repeated, ["ttl_once"]);
    assert_eq!(report.missing, ["security_exception"]);
}

#[test]
fn visual_only_observations_never_become_fabricated_speech() {
    let corpus = corpus();
    let case = case(&corpus, "visual_only");
    let mut document = case.expected_document(Depth::Detailed);
    assert!(document.transcript.is_empty());
    assert!(
        document
            .evidence
            .iter()
            .all(|evidence| evidence.transcript_segment_ids.is_empty())
    );
    assert!(coverage(case, &document, Depth::Detailed).is_complete());
    // Correct visible-state text paired with the wrong real frame is insufficient.
    let BlockContent::Image { asset_id, .. } = &mut document.sections[1].blocks[0].content else {
        panic!("expected canonical image block");
    };
    *asset_id = "panel_before".into();
    document.validate().unwrap();
    assert_eq!(
        coverage(case, &document, Depth::Detailed).unsupported,
        ["final_state"]
    );
}

#[test]
fn unused_transcript_and_injection_text_cannot_satisfy_output_coverage() {
    let corpus = corpus();
    let case = case(&corpus, "dense_numbers_examples_caveats");
    let mut document = case.expected_document(Depth::Detailed);
    assert!(
        document
            .transcript
            .iter()
            .any(|segment| segment.text.contains("UNTRUSTED_FIXTURE_DIRECTIVE"))
    );
    document.sections.clear();
    assert_eq!(
        coverage(case, &document, Depth::Detailed).missing.len(),
        case.items.len()
    );
    // This verifies oracle separation only, not model resistance to source injection.
}

#[test]
fn unknown_document_schema_rejects_without_modifying_the_prior_revision() {
    let corpus = corpus();
    let original = corpus.cases[0].expected_document(Depth::Detailed);
    let prior_bytes = serde_json::to_vec(&original).unwrap();
    let mut future = serde_json::to_value(&original).unwrap();
    future["schema_version"] = serde_json::json!(u32::MAX);
    assert!(matches!(
        RecapDocument::from_json_slice(&serde_json::to_vec(&future).unwrap()),
        Err(Error::UnsupportedVersion(u32::MAX))
    ));
    assert_eq!(serde_json::to_vec(&original).unwrap(), prior_bytes);
    assert_eq!(original.revision, 1);
}

#[test]
fn complete_item_words_do_not_excuse_known_false_additions() {
    for case in corpus().cases {
        for claim in &case.forbidden_claims {
            let mut document = case.expected_document(Depth::Detailed);
            document.sections[0]
                .blocks
                .push(bcode_recap::contracts::Block {
                    id: "unsupported-addition".into(),
                    // An existing reference is not proof of support for a new assertion.
                    evidence_ids: vec![case.observations[0].id.clone()],
                    content: BlockContent::Text {
                        text: claim.clone(),
                    },
                });
            document.validate().unwrap();
            let report = coverage(&case, &document, Depth::Detailed);
            assert!(report.missing.is_empty());
            assert_eq!(report.distorted.as_slice(), std::slice::from_ref(claim));
            assert!(!report.is_complete(), "{}: {claim}", case.id);
        }
    }
}

#[test]
fn interview_loses_meaning_when_numbers_attribution_or_qualifiers_change() {
    let corpus = corpus();
    let case = case(&corpus, "qualified_interview_disagreement");
    for (index, original, replacement) in [
        (0, "12-degree", "12-percent"),
        (1, "Reyes disputes", "Reyes endorses"),
        (
            2,
            "hypothesis, not a measured explanation",
            "measured explanation",
        ),
        (3, "if humidity exceeds 70 percent", "at any humidity"),
        (
            4,
            "because their sensors stopped logging",
            "because they succeeded",
        ),
    ] {
        let mut document = case.expected_document(Depth::Detailed);
        let BlockContent::Text { text } = &mut document.sections[index].blocks[0].content else {
            panic!("expected interview text");
        };
        assert!(text.contains(original));
        *text = text.replace(original, replacement);
        document.validate().unwrap();
        // IDs, evidence and every other fact remain intact; content still fails.
        assert_eq!(
            coverage(case, &document, Depth::Detailed).missing,
            [case.items[index].id.clone()]
        );
    }
}

#[test]
fn earlier_repair_cannot_be_lost_when_a_later_detail_is_restored() {
    let corpus = corpus();
    let case = case(&corpus, "qualified_interview_disagreement");
    let repaired = case.expected_document(Depth::Detailed);
    assert!(coverage(case, &repaired, Depth::Detailed).is_complete());
    let mut regression = repaired.clone();
    regression.sections[2].blocks[0].content = BlockContent::Text {
        text: "The fan explains the improvement.".into(),
    };
    // A later correction survives, but cannot compensate for the earlier causal caveat.
    assert_eq!(regression.sections[4], repaired.sections[4]);
    assert_eq!(
        coverage(case, &regression, Depth::Detailed).missing,
        ["causal_hypothesis"]
    );
}

#[test]
fn numeric_and_logical_mutations_fail_across_source_categories() {
    let corpus = corpus();
    for (case_id, item_id, original, replacement) in [
        (
            "dense_numbers_examples_caveats",
            "baseline_quantities",
            "median 18 ms",
            "median 180 ms",
        ),
        (
            "tutorial_order",
            "verify_both",
            "240 records",
            "2400 records",
        ),
        (
            "visual_only",
            "final_state",
            "cause is not established",
            "cause is established",
        ),
        (
            "repetition",
            "security_exception",
            "invalidate immediately",
            "invalidate eventually",
        ),
        (
            "late_video_details",
            "firmware_exception",
            "firmware 1.4 only",
            "all firmware",
        ),
    ] {
        let case = case(&corpus, case_id);
        let index = case
            .items
            .iter()
            .position(|item| item.id == item_id)
            .unwrap();
        let mut document = case.expected_document(Depth::Detailed);
        let text = match &mut document.sections[index].blocks[0].content {
            BlockContent::Text { text } => text,
            BlockContent::Image { caption, .. } => caption,
            _ => panic!("expected text or image"),
        };
        assert!(text.contains(original));
        *text = text.replace(original, replacement);
        document.validate().unwrap();
        assert_eq!(
            coverage(case, &document, Depth::Detailed).missing,
            [item_id],
            "{case_id}"
        );
    }
}

#[test]
fn cosmetic_case_whitespace_and_section_titles_do_not_change_coverage() {
    for case in corpus().cases {
        let mut document = case.expected_document(Depth::Detailed);
        for section in &mut document.sections {
            section.title = "Editorial heading".into();
            for block in &mut section.blocks {
                let text = match &mut block.content {
                    BlockContent::Text { text } => text,
                    BlockContent::Image { caption, .. } | BlockContent::Clip { caption, .. } => {
                        caption
                    }
                    BlockContent::Comparison { .. } => continue,
                };
                *text = text.to_uppercase().replace(' ', "  \n ");
            }
        }
        document.validate().unwrap();
        assert!(
            coverage(&case, &document, Depth::Detailed).is_complete(),
            "{}",
            case.id
        );
    }
}
