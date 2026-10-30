//! Deterministic mechanics tests; these do not measure model semantic quality.
use std::sync::Mutex;

use crate::{
    analysis_contracts::{AnalysisRequest, AnalysisResponse, AnalysisStage, DetailedPolicy},
    contracts::{Block, BlockContent, RecapDocument, TimeRange},
    model::{AnalysisFuture, ModelError, ModelExecutor, ModelFuture, ModelRequest},
    passage_contracts::{
        Passage, PassageComposition, PassageFinding, PassageFindingKind, PassagePatch,
        PassagePatchKind, PassagePatchOperation, PassageReview, PassageSource, PassageTarget,
        PassageTargetKind, SourceUnit,
    },
    process::Cancellation,
};

use super::{compose_passages, validation};

mod correction;

fn block(id: &str, text: &str) -> Block {
    Block {
        id: id.into(),
        evidence_ids: vec!["evidence".into()],
        content: BlockContent::Text { text: text.into() },
    }
}

fn fixture() -> (RecapDocument, SourceUnit, Passage) {
    let mut document = crate::contracts::tests::document();
    document.assets.clear();
    document.sections[0].blocks = vec![block("initial", "Hello")];
    let source = PassageSource {
        range: TimeRange {
            start_ms: 0,
            end_ms: 10_000,
        },
        transcript: document.transcript.clone(),
        evidence: document.evidence.clone(),
        assets: vec![],
    };
    let unit = SourceUnit {
        source_context: crate::source_context::SourceContext::default(),
        id: "unit-0".into(),
        primary: source,
        context: PassageSource {
            range: TimeRange {
                start_ms: 0,
                end_ms: 0,
            },
            transcript: vec![],
            evidence: vec![],
            assets: vec![],
        },
    };
    let passage = Passage {
        id: "passage-unit-0".into(),
        unit_id: unit.id.clone(),
        revision: 1,
        title: "Explanation".into(),
        blocks: vec![
            block("first", "Initial"),
            block("untouched", "Keep exactly."),
        ],
    };
    (document, unit, passage)
}

fn finding(target: PassageTarget) -> PassageFinding {
    PassageFinding {
        kind: PassageFindingKind::Omission,
        evidence_ids: vec!["evidence".into()],
        range: TimeRange {
            start_ms: 0,
            end_ms: 1000,
        },
        target,
        discrepancy: "Example absent".into(),
        correction: "Restore example".into(),
    }
}

fn target(id: &str) -> PassageTarget {
    PassageTarget {
        kind: PassageTargetKind::Block,
        block_id: id.into(),
    }
}

#[test]
fn patches_preserve_unaffected_content_and_reject_stale_conflicting_foreign_or_unchanged_edits() {
    let (document, unit, base) = fixture();
    let findings = vec![finding(target("first"))];
    let patch = PassagePatch {
        base_revision: 1,
        operations: vec![PassagePatchOperation {
            kind: PassagePatchKind::Replace,
            target: target("first"),
            blocks: vec![block("model-id-is-not-authority", "Full example")],
        }],
    };
    let candidate = validation::patch(&document, &unit, &base, &findings, &patch).unwrap();
    assert_eq!(candidate.revision, 2);
    assert_eq!(candidate.blocks[0].id, "first");
    assert_eq!(candidate.blocks[1], base.blocks[1]);
    assert_eq!(base.revision, 1);
    assert_eq!(base.blocks[0], block("first", "Initial"));
    assert!(validation::patch(&document, &unit, &candidate, &findings, &patch).is_err());
    let mut invalid = patch.clone();
    invalid.operations.push(patch.operations[0].clone());
    assert!(validation::patch(&document, &unit, &base, &findings, &invalid).is_err());
    invalid = patch.clone();
    invalid.operations[0].target = target("missing");
    assert!(validation::patch(&document, &unit, &base, &findings, &invalid).is_err());
    invalid = patch.clone();
    invalid.operations[0].blocks[0].evidence_ids = vec!["invented".into()];
    assert!(validation::patch(&document, &unit, &base, &findings, &invalid).is_err());
    invalid = patch;
    invalid.operations[0].blocks[0] = base.blocks[0].clone();
    assert!(validation::patch(&document, &unit, &base, &findings, &invalid).is_err());
}

#[test]
fn late_failure_does_not_partially_apply_an_earlier_valid_operation() {
    let (document, unit, base) = fixture();
    let original = base.clone();
    let findings = vec![finding(target("first")), finding(target("untouched"))];
    let patch = PassagePatch {
        base_revision: 1,
        operations: vec![
            PassagePatchOperation {
                kind: PassagePatchKind::Replace,
                target: target("first"),
                blocks: vec![block("new", "Valid change")],
            },
            PassagePatchOperation {
                kind: PassagePatchKind::Replace,
                target: target("untouched"),
                blocks: vec![Block {
                    evidence_ids: vec!["foreign".into()],
                    ..block("bad", "Bad")
                }],
            },
        ],
    };
    assert!(validation::patch(&document, &unit, &base, &findings, &patch).is_err());
    assert_eq!(base, original);
}

#[test]
fn review_rejects_fabricated_disjoint_and_out_of_unit_provenance_and_targets() {
    let (_, unit, base) = fixture();
    let valid = PassageReview {
        findings: vec![finding(target("first"))],
        limitations: vec![],
    };
    validation::review(&unit, &base, &valid).unwrap();
    let mut invalid = valid.clone();
    invalid.findings[0].evidence_ids = vec!["foreign".into()];
    assert!(validation::review(&unit, &base, &invalid).is_err());
    invalid = valid.clone();
    invalid.findings[0].range = TimeRange {
        start_ms: 3000,
        end_ms: 4000,
    };
    assert!(validation::review(&unit, &base, &invalid).is_err());
    invalid = valid.clone();
    invalid.findings[0].range.end_ms = 10_001;
    assert!(validation::review(&unit, &base, &invalid).is_err());
    invalid = valid;
    invalid.findings[0].target = target("foreign");
    assert!(validation::review(&unit, &base, &invalid).is_err());
}

#[test]
fn insertions_use_base_anchors_and_deletions_preserve_other_blocks() {
    let (document, unit, base) = fixture();
    let anchor = PassageTarget {
        kind: PassageTargetKind::AfterBlock,
        block_id: "first".into(),
    };
    let findings = vec![finding(anchor.clone()), finding(target("untouched"))];
    let patch = PassagePatch {
        base_revision: 1,
        operations: vec![
            PassagePatchOperation {
                kind: PassagePatchKind::Insert,
                target: anchor,
                blocks: vec![block("ignored", "Inserted explanation")],
            },
            PassagePatchOperation {
                kind: PassagePatchKind::Delete,
                target: target("untouched"),
                blocks: vec![],
            },
        ],
    };
    let candidate = validation::patch(&document, &unit, &base, &findings, &patch).unwrap();
    assert_eq!(candidate.blocks.len(), 2);
    assert_eq!(candidate.blocks[0], base.blocks[0]);
    assert_eq!(
        candidate.blocks[1].content,
        BlockContent::Text {
            text: "Inserted explanation".into()
        }
    );
    let mut same_prose = candidate.clone();
    same_prose.revision = 99;
    same_prose.blocks[1].id = "different-host-identity".into();
    assert_eq!(
        validation::content_key(&candidate).unwrap(),
        validation::content_key(&same_prose).unwrap()
    );
}

#[tokio::test]
async fn exhausted_allowances_and_cancellation_admit_no_unreserved_followup() {
    let (document, _, _) = fixture();
    let executor = Scripted {
        stages: Mutex::new(vec![]),
        cycle: false,
    };
    let mut policy = DetailedPolicy {
        inspect_visuals: false,
        ..DetailedPolicy::default()
    };
    policy.allowances.max_model_calls = 1;
    assert!(
        compose_passages(
            &executor,
            &document,
            vec![],
            document.intent.clone(),
            None,
            &policy,
            &Cancellation::default()
        )
        .await
        .is_err()
    );
    assert_eq!(
        *executor.stages.lock().unwrap(),
        vec![AnalysisStage::PassageComposition]
    );
    let cancel = Cancellation::default();
    cancel.cancel();
    let executor = Scripted {
        stages: Mutex::new(vec![]),
        cycle: false,
    };
    assert!(
        compose_passages(
            &executor,
            &document,
            vec![],
            document.intent.clone(),
            None,
            &policy,
            &cancel
        )
        .await
        .is_err()
    );
    assert!(executor.stages.lock().unwrap().is_empty());
}

struct Scripted {
    stages: Mutex<Vec<AnalysisStage>>,
    cycle: bool,
}

impl ModelExecutor for Scripted {
    fn execute<'a>(&'a self, _: ModelRequest, _: &'a Cancellation) -> ModelFuture<'a> {
        Box::pin(async { Err(ModelError::Unavailable) })
    }

    fn execute_analysis<'a>(
        &'a self,
        request: AnalysisRequest,
        _: &'a Cancellation,
    ) -> AnalysisFuture<'a> {
        Box::pin(async move {
            self.stages.lock().unwrap().push(request.stage);
            let task = match request.stage {
                AnalysisStage::PassageComposition => super::WRITE,
                AnalysisStage::PassageReview => super::REVIEW,
                AnalysisStage::PassagePatch => super::PATCH,
                _ => return Err(ModelError::InvalidResponse),
            };
            assert_eq!(
                request.instructions,
                format!("{}\n{task}", super::PASSAGE_POLICY)
            );
            let unit: SourceUnit =
                serde_json::from_value(request.input["source_unit"].clone()).unwrap();
            // The executable path must supply actual source, never an inventory proxy.
            assert_eq!(unit.primary.transcript[0].text, "Hello");
            let mut response = match request.stage {
                AnalysisStage::PassageComposition => {
                    AnalysisResponse::PassageComposition(PassageComposition {
                        title: "Source passage".into(),
                        blocks: vec![block("model-owned", "Incomplete")],
                    })
                }
                AnalysisStage::PassageReview => {
                    let passage: Passage =
                        serde_json::from_value(request.input["passage"].clone()).unwrap();
                    let findings = if passage.revision == 1 || self.cycle {
                        let mut discrepancy = finding(target(&passage.blocks[0].id));
                        discrepancy.range = unit.primary.transcript[0].range;
                        vec![discrepancy]
                    } else {
                        vec![]
                    };
                    AnalysisResponse::PassageReview(PassageReview {
                        findings,
                        limitations: vec!["Only supplied source was inspected.".into()],
                    })
                }
                AnalysisStage::PassagePatch => {
                    let passage: Passage =
                        serde_json::from_value(request.input["passage"].clone()).unwrap();
                    AnalysisResponse::PassagePatch(PassagePatch {
                        base_revision: passage.revision,
                        operations: vec![PassagePatchOperation {
                            kind: PassagePatchKind::Replace,
                            target: target(&passage.blocks[0].id),
                            blocks: vec![block(
                                "ignored",
                                if passage.revision == 1 {
                                    "Hello, with example"
                                } else {
                                    "Incomplete"
                                },
                            )],
                        }],
                    })
                }
                _ => return Err(ModelError::InvalidResponse),
            };
            let evidence_ids: Vec<_> = unit
                .primary
                .evidence
                .iter()
                .map(|item| item.id.clone())
                .collect();
            match &mut response {
                AnalysisResponse::PassageComposition(value) => {
                    for block in &mut value.blocks {
                        block.evidence_ids.clone_from(&evidence_ids);
                    }
                }
                AnalysisResponse::PassageReview(value) => {
                    for finding in &mut value.findings {
                        finding.evidence_ids.clone_from(&evidence_ids);
                    }
                }
                AnalysisResponse::PassagePatch(value) => {
                    for operation in &mut value.operations {
                        for block in &mut operation.blocks {
                            block.evidence_ids.clone_from(&evidence_ids);
                        }
                    }
                }
                _ => unreachable!(),
            }
            Ok(response)
        })
    }
}

#[tokio::test]
async fn direct_source_write_review_patch_rereview_final_review_assembles_without_rewrite() {
    let (document, _, _) = fixture();
    let original = document.clone();
    let executor = Scripted {
        stages: Mutex::new(vec![]),
        cycle: false,
    };
    let policy = DetailedPolicy {
        inspect_visuals: false,
        ..DetailedPolicy::default()
    };
    let result = compose_passages(
        &executor,
        &document,
        vec![],
        document.intent.clone(),
        None,
        &policy,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(document, original);
    assert_eq!(
        result.sections[0].blocks[0].content,
        BlockContent::Text {
            text: "Hello, with example".into()
        }
    );
    assert_eq!(result.sections.len(), 1);
    assert_eq!(
        result.sections[0].blocks.len(),
        1,
        "review limitations must not become recap blocks"
    );
    assert_ne!(result.sections[0].blocks[0].id, "model-owned");
    assert_eq!(
        *executor.stages.lock().unwrap(),
        vec![
            AnalysisStage::PassageComposition,
            AnalysisStage::PassageReview,
            AnalysisStage::PassagePatch,
            AnalysisStage::PassageReview,
            AnalysisStage::PassageReview
        ]
    );
}

struct ConcurrentWrites {
    inner: Scripted,
    active: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
    source_units: Vec<SourceUnit>,
    boundary_sources_seen: Mutex<Vec<String>>,
}

impl ModelExecutor for ConcurrentWrites {
    fn execute<'a>(&'a self, request: ModelRequest, cancel: &'a Cancellation) -> ModelFuture<'a> {
        self.inner.execute(request, cancel)
    }

    fn execute_analysis<'a>(
        &'a self,
        request: AnalysisRequest,
        cancel: &'a Cancellation,
    ) -> AnalysisFuture<'a> {
        Box::pin(async move {
            use std::sync::atomic::Ordering;
            let writing = request.stage == AnalysisStage::PassageComposition;
            if request.stage == AnalysisStage::PassageReview {
                let unit: SourceUnit =
                    serde_json::from_value(request.input["source_unit"].clone()).unwrap();
                let neighbor_prose = request.input["neighboring_passages_context_only"]
                    .as_array()
                    .unwrap();
                let sources: Vec<SourceUnit> = serde_json::from_value(
                    request.input["neighboring_source_units_context_only"].clone(),
                )
                .unwrap();
                if neighbor_prose.is_empty() {
                    assert!(
                        sources.is_empty(),
                        "local reviews must not acquire neighbor source authority"
                    );
                } else {
                    let index = self
                        .source_units
                        .iter()
                        .position(|original| original.id == unit.id)
                        .unwrap();
                    let expected: Vec<_> = self.source_units
                        [index.saturating_sub(1)..(index + 2).min(self.source_units.len())]
                        .iter()
                        .filter(|other| other.id != unit.id)
                        .cloned()
                        .collect();
                    assert_eq!(
                        sources, expected,
                        "boundary review must retain exact original source records"
                    );
                    self.boundary_sources_seen.lock().unwrap().push(unit.id);
                }
            }
            if writing {
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(active, Ordering::SeqCst);
                tokio::task::yield_now().await;
            }
            let result = self.inner.execute_analysis(request, cancel).await;
            if writing {
                self.active.fetch_sub(1, Ordering::SeqCst);
            }
            result
        })
    }
}

#[tokio::test]
async fn multiple_units_assemble_in_source_order_and_each_receive_final_source_review() {
    let (mut document, _, _) = fixture();
    document
        .transcript
        .push(crate::contracts::TranscriptSegment {
            id: "late-words".into(),
            range: TimeRange {
                start_ms: 6000,
                end_ms: 7000,
            },
            text: "Hello".into(),
        });
    document.evidence[0].range.end_ms = 10_000;
    document.evidence[0]
        .transcript_segment_ids
        .push("late-words".into());
    let policy = DetailedPolicy {
        inspect_visuals: false,
        window_duration_ms: 6000,
        window_overlap_ms: 1000,
        ..DetailedPolicy::default()
    };
    let units = super::source::units(&document, &policy, &Cancellation::default()).unwrap();
    assert_eq!(units.len(), 2);
    let executor = ConcurrentWrites {
        inner: Scripted {
            stages: Mutex::new(vec![]),
            cycle: false,
        },
        active: std::sync::atomic::AtomicUsize::new(0),
        peak: std::sync::atomic::AtomicUsize::new(0),
        source_units: units.clone(),
        boundary_sources_seen: Mutex::new(vec![]),
    };
    let result = compose_passages(
        &executor,
        &document,
        vec![],
        document.intent.clone(),
        None,
        &policy,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        result
            .sections
            .iter()
            .map(|section| section.id.clone())
            .collect::<Vec<_>>(),
        units
            .iter()
            .map(|unit| format!("passage-{}", unit.id))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        *executor.boundary_sources_seen.lock().unwrap(),
        units.iter().map(|unit| unit.id.clone()).collect::<Vec<_>>()
    );
    assert_eq!(executor.peak.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        executor
            .inner
            .stages
            .lock()
            .unwrap()
            .iter()
            .filter(|stage| **stage == AnalysisStage::PassageReview)
            .count(),
        6
    );
}

#[tokio::test]
async fn cyclic_prose_is_rejected_without_mutating_source_or_spending_remaining_rounds() {
    let (document, _, _) = fixture();
    let original = document.clone();
    let executor = Scripted {
        stages: Mutex::new(vec![]),
        cycle: true,
    };
    let policy = DetailedPolicy {
        inspect_visuals: false,
        ..DetailedPolicy::default()
    };
    assert!(
        compose_passages(
            &executor,
            &document,
            vec![],
            document.intent.clone(),
            None,
            &policy,
            &Cancellation::default()
        )
        .await
        .is_err()
    );
    assert_eq!(document, original);
    assert_eq!(executor.stages.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn long_source_completes_all_audits_past_old_call_and_window_ceilings() {
    let (mut document, _, _) = fixture();
    let count = 130_u64;
    document.source.duration_ms = count * 10_000;
    document.sections.clear();
    document.transcript.clear();
    let template = document.evidence[0].clone();
    document.evidence.clear();
    for index in 0..count {
        let id = format!("cue-{index}");
        let range = TimeRange {
            start_ms: index * 10_000,
            end_ms: index * 10_000 + 1000,
        };
        document
            .transcript
            .push(crate::contracts::TranscriptSegment {
                id: id.clone(),
                range,
                text: "Hello".into(),
            });
        let mut evidence = template.clone();
        evidence.id = format!("evidence-{index}");
        evidence.range = range;
        evidence.transcript_segment_ids = vec![id];
        document.evidence.push(evidence);
    }
    let policy = DetailedPolicy {
        inspect_visuals: false,
        window_duration_ms: 10_000,
        window_overlap_ms: 0,
        ..DetailedPolicy::default()
    };
    let units = super::source::units(&document, &policy, &Cancellation::default()).unwrap();
    assert_eq!(units.len(), 130);
    let executor = ConcurrentWrites {
        inner: Scripted {
            stages: Mutex::new(vec![]),
            cycle: false,
        },
        active: std::sync::atomic::AtomicUsize::new(0),
        peak: std::sync::atomic::AtomicUsize::new(0),
        source_units: units,
        boundary_sources_seen: Mutex::new(vec![]),
    };
    let result = compose_passages(
        &executor,
        &document,
        vec![],
        document.intent.clone(),
        None,
        &policy,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.sections.len(), 130);
    assert_eq!(executor.boundary_sources_seen.lock().unwrap().len(), 130);
    assert!(executor.inner.stages.lock().unwrap().len() > 128);
    assert!(
        executor.peak.load(std::sync::atomic::Ordering::SeqCst) <= policy.max_concurrent_requests
    );
    result.validate().unwrap();
}

#[test]
fn oversized_review_partition_preserves_every_source_and_target_without_splitting_blocks() {
    let (_, unit, passage) = fixture();
    let input = serde_json::json!({"source_unit":unit,"passage":passage,"neighboring_passages_context_only":[passage.clone(),passage],"neighboring_source_units_context_only":[unit.clone(),unit]});
    let mut pending = vec![input.clone()];
    let mut leaves = vec![];
    while let Some(value) = pending.pop() {
        if let Some((a, b)) = super::split_review(&value) {
            pending.push(a);
            pending.push(b);
        } else {
            leaves.push(value);
        }
    }
    assert_eq!(leaves.len(), 8);
    for value in &leaves {
        assert_eq!(value["source_unit"], input["source_unit"]);
        assert_eq!(
            value["neighboring_source_units_context_only"],
            input["neighboring_source_units_context_only"]
        );
        assert_eq!(value["passage"]["blocks"].as_array().unwrap().len(), 1);
        assert_eq!(
            value["neighboring_passages_context_only"][0]["blocks"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    for target in input["passage"]["blocks"].as_array().unwrap() {
        for neighbor in input["neighboring_passages_context_only"]
            .as_array()
            .unwrap()
        {
            for block in neighbor["blocks"].as_array().unwrap() {
                assert!(
                    leaves
                        .iter()
                        .any(|leaf| leaf["passage"]["blocks"][0] == *target
                            && leaf["neighboring_passages_context_only"][0]["blocks"][0] == *block)
                );
            }
        }
    }
}
