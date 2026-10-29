#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Portable fixture/oracle and scripted public-pipeline tests; no live quality claim.

#[path = "fixtures/passage/pipeline.rs"]
mod pipeline;

#[path = "fixtures/passage/mod.rs"]
mod passage;

use std::collections::BTreeSet;

use passage::{ControlKind, SourceKind, corpus, phrase_controls};

#[test]
fn annotations_have_ordered_source_and_independent_meaning_obligations() {
    let corpus = corpus();
    assert_eq!(corpus.schema_version, 1);
    assert_eq!(corpus.cases.len(), 5);
    let mut cases = BTreeSet::new();
    for case in &corpus.cases {
        assert!(cases.insert(&case.id));
        let mut sources = BTreeSet::new();
        let mut previous_end = 0;
        assert!(!case.units.is_empty());
        for unit in &case.units {
            assert!(sources.insert(&unit.id));
            assert!(unit.start_ms >= previous_end);
            assert!(unit.end_ms > unit.start_ms);
            assert!(!unit.text.trim().is_empty());
            previous_end = unit.end_ms;
        }
        let mut obligations = BTreeSet::new();
        assert!(!case.obligations.is_empty());
        for obligation in &case.obligations {
            assert!(obligations.insert(&obligation.id));
            assert!(!obligation.meaning.trim().is_empty());
            assert!(!obligation.phrases.is_empty());
            assert!(obligation.phrases.iter().all(|p| !p.trim().is_empty()));
            assert!(!obligation.source_ids.is_empty());
            assert!(obligation.source_ids.iter().all(|id| sources.contains(id)));
        }
        let mut controls = BTreeSet::new();
        for control in &case.controls {
            assert!(controls.insert(&control.id));
            assert!(!control.source_ids.is_empty());
            assert!(control.source_ids.iter().all(|id| sources.contains(id)));
            assert!(control.missing.iter().all(|id| obligations.contains(id)));
            assert!(
                control
                    .unsupported
                    .iter()
                    .all(|s| case.forbidden.contains(s))
            );
        }
    }
}

#[test]
fn annotated_positive_and_negative_phrase_controls_match_their_labels() {
    for case in corpus().cases {
        let report = phrase_controls(&case, &case.acceptable);
        assert!(report.missing.is_empty(), "{}: {report:?}", case.id);
        assert!(report.unsupported.is_empty(), "{}: {report:?}", case.id);
        for control in &case.controls {
            let report = phrase_controls(&case, &control.text);
            assert_eq!(report.missing, control.missing, "{}", control.id);
            assert_eq!(report.unsupported, control.unsupported, "{}", control.id);
            assert!(
                !report.missing.is_empty() || !report.unsupported.is_empty(),
                "negative control must exercise the narrow oracle"
            );
        }
    }
}

#[test]
fn negative_controls_cover_requested_content_failures_with_valid_references() {
    let corpus = corpus();
    let kinds: BTreeSet<_> = corpus
        .cases
        .iter()
        .flat_map(|case| &case.controls)
        .map(|control| &control.kind)
        .collect();
    assert_eq!(
        kinds,
        BTreeSet::from([
            &ControlKind::OmissionDespiteIds,
            &ControlKind::UnsupportedAddition,
            &ControlKind::Contradiction,
            &ControlKind::LostCaveatNumber,
            &ControlKind::MisleadingCaption,
            &ControlKind::CrossBoundaryFact,
        ])
    );
    // Reference integrity is tested separately above; it does not imply meaning survives.
}

#[test]
fn phrase_oracle_deliberately_cannot_certify_semantics() {
    for case in corpus().cases {
        // Every expected phrase survives, but its assertion is negated. A clean
        // report must never be presented as a semantic acceptance decision.
        let negated = format!("The following account is false: {}", case.acceptable);
        let report = phrase_controls(&case, &negated);
        assert!(report.missing.is_empty());
        assert!(report.unsupported.is_empty());
    }
    let corpus = corpus();
    let causal = &corpus.cases[0];
    let paraphrase = "Buffering lets senders proceed while the receiver briefly stops. Two hundred items each second over three seconds require six hundred slots. This arithmetic does not measure speed. If arrivals keep exceeding service, capacity postpones rather than cures saturation.";
    // A human can judge this against the independent rubric. The exact-phrase
    // oracle misses a faithful paraphrase and cannot be used as a quality score.
    assert!(!phrase_controls(causal, paraphrase).missing.is_empty());
}

#[test]
fn visual_and_boundary_annotations_do_not_masquerade_as_transcript_coverage() {
    let corpus = corpus();
    let visual = corpus.cases.iter().find(|c| c.id == "visual_only").unwrap();
    let motion = visual.units.iter().find(|u| u.id == "motion").unwrap();
    assert_eq!(motion.kind, SourceKind::VisualAnnotation);
    let spoken = visual.units.iter().find(|u| u.id == "spoken").unwrap();
    assert_eq!(spoken.kind, SourceKind::Transcript);
    assert_eq!(
        phrase_controls(visual, &spoken.text).missing,
        ["visual_sequence"]
    );
    let boundary = corpus
        .cases
        .iter()
        .find(|c| c.id == "boundary_explanation")
        .unwrap();
    let condition = boundary
        .obligations
        .iter()
        .find(|o| o.id == "cross_boundary_condition")
        .unwrap();
    assert_eq!(condition.source_ids, ["boundary_left", "boundary_right"]);
}

#[test]
fn patch_fixture_supplies_stale_revision_and_untouched_content_sentinels() {
    let corpus = corpus();
    let patch = corpus.patch_scenario;
    assert!(patch.expected_revision < patch.current_revision);
    assert!(!patch.target_block.is_empty());
    assert!(!patch.replacement_text.is_empty());
    assert!(!patch.untouched_text.is_empty());
    assert!(!patch.untouched_image.asset_id.is_empty());
    assert!(!patch.untouched_image.caption.is_empty());
    assert_eq!(
        patch.untouched_image.evidence_ids,
        ["boundary_left", "boundary_right"]
    );
    // Data validation only. Actual stale-patch rejection, atomicity and byte-for-byte
    // preservation must be asserted through the production patch API once available.
}
