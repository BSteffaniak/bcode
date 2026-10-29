//! Independent public-pipeline tests with scripted semantic findings, not live inference.

#[path = "additional.rs"]
mod additional;

use std::sync::Mutex;

use bcode_recap::{
    analysis_contracts::{AnalysisRequest, AnalysisResponse, AnalysisStage, DetailedPolicy},
    composition::CompositionError,
    contracts::{
        Asset, AssetPath, Block, BlockContent, Depth, Evidence, GenerationIntent, RecapDocument,
        Source, TimeRange, TranscriptSegment,
    },
    model::{AnalysisFuture, ModelError, ModelExecutor, ModelFuture, ModelRequest},
    passage::compose_passages,
    passage_contracts::{
        Passage, PassageComposition, PassageFinding, PassageFindingKind, PassagePatch,
        PassagePatchKind, PassagePatchOperation, PassageReview, PassageTarget, PassageTargetKind,
        SourceUnit,
    },
    process::Cancellation,
};

use super::{passage, passage::SourceKind};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Repair,
    Stale,
    Unresolved,
    Boundary,
    BoundaryFinding,
    Omission,
    Unsupported,
    ForeignEvidence,
    UnknownTarget,
    OutsideRange,
}

struct Scripted {
    scenario: Scenario,
    requests: Mutex<Vec<AnalysisRequest>>,
    /// Independent fixture annotation, not derived from the composed candidate.
    correction: String,
    false_claim: String,
    expected_source: Vec<TranscriptSegment>,
    cancel_stage: Option<AnalysisStage>,
}

impl Scripted {
    fn new(scenario: Scenario, case: &passage::Case) -> Self {
        Self {
            scenario,
            requests: Mutex::new(vec![]),
            correction: case.acceptable.clone(),
            false_claim: match scenario {
                Scenario::Omission => "Queue capacity matters.".into(),
                Scenario::Unsupported => case.forbidden[1].clone(),
                _ => case.forbidden[0].clone(),
            },
            expected_source: source(case, false).transcript,
            cancel_stage: None,
        }
    }

    fn inspect_source(&self, unit: &SourceUnit) {
        assert!(!unit.primary.transcript.is_empty());
        for cue in &unit.primary.transcript {
            let expected = self
                .expected_source
                .iter()
                .find(|source| source.id == cue.id)
                .expect("primary cue must originate in independently loaded fixture");
            assert_eq!(
                cue, expected,
                "source text/range may not be replaced by summaries"
            );
            assert!(
                unit.primary
                    .evidence
                    .iter()
                    .any(|evidence| evidence.transcript_segment_ids.contains(&cue.id))
            );
        }
        for cue in &unit.context.transcript {
            assert!(self.expected_source.contains(cue));
            assert!(
                !unit
                    .primary
                    .transcript
                    .iter()
                    .any(|primary| primary.id == cue.id)
            );
        }
        if unit.primary.transcript.iter().any(|cue| cue.id == "cause") {
            // Deliberately narrow fixture comparisons, not a general semantic scorer.
            let actual = unit
                .primary
                .transcript
                .iter()
                .map(|cue| cue.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            assert!(actual.contains("only delays overload"));
            assert!(actual.contains("not a measured throughput gain"));
            assert!(actual.contains("200 records per second"));
            assert!(actual.contains("600 records"));
        }
    }

    fn composition_blocks(&self, unit: &SourceUnit, evidence_ids: &[String]) -> Vec<Block> {
        if matches!(
            self.scenario,
            Scenario::Boundary | Scenario::BoundaryFinding
        ) {
            vec![text_block(
                &unit
                    .primary
                    .transcript
                    .iter()
                    .map(|cue| cue.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                evidence_ids,
            )]
        } else {
            vec![
                text_block(&self.false_claim, evidence_ids),
                text_block(
                    "Unrelated text sentinel: preserve this block exactly.",
                    evidence_ids,
                ),
                Block {
                    id: "model-image-id".into(),
                    evidence_ids: evidence_ids.to_vec(),
                    content: BlockContent::Image {
                        asset_id: "source-frame".into(),
                        caption: "Source illustration sentinel; not proof of causal improvement."
                            .into(),
                    },
                },
            ]
        }
    }

    fn response(&self, request: &AnalysisRequest) -> AnalysisResponse {
        let unit: SourceUnit =
            serde_json::from_value(request.input["source_unit"].clone()).unwrap();
        self.inspect_source(&unit);
        let evidence_ids: Vec<_> = unit.primary.evidence.iter().map(|e| e.id.clone()).collect();
        match request.stage {
            AnalysisStage::PassageComposition => {
                let blocks = self.composition_blocks(&unit, &evidence_ids);
                AnalysisResponse::PassageComposition(PassageComposition {
                    title: "Direct-source passage".into(),
                    blocks,
                })
            }
            AnalysisStage::PassageReview => {
                let passage: Passage =
                    serde_json::from_value(request.input["passage"].clone()).unwrap();
                let final_boundary = self.scenario == Scenario::BoundaryFinding
                    && !request.input["neighboring_passages_context_only"]
                        .as_array()
                        .unwrap()
                        .is_empty();
                let contains_false = passage.blocks.iter().any(|b| {
                    matches!(&b.content, BlockContent::Text { text } if text == &self.false_claim)
                });
                let mut findings = if contains_false || final_boundary {
                    let evidence = &unit.primary.evidence[0];
                    vec![PassageFinding {
                        kind: match self.scenario {
                            Scenario::Omission => PassageFindingKind::Omission,
                            Scenario::Unsupported => PassageFindingKind::Unsupported,
                            _ => PassageFindingKind::Distortion,
                        },
                        evidence_ids: vec![evidence.id.clone()],
                        range: evidence.range,
                        target: PassageTarget {
                            kind: PassageTargetKind::Block,
                            block_id: passage.blocks[0].id.clone(),
                        },
                        discrepancy: if final_boundary {
                            "Boundary condition requires original neighboring source; unresolved review hypothesis.".into()
                        } else {
                            match self.scenario {
                                Scenario::Omission => "Actual primary source includes 200 records per second, 3 seconds, 600 records, a non-measured-gain caveat and permanent-overload caveat; topic-only prose omits them.",
                                Scenario::Unsupported => "Actual primary source explicitly labels 600 records as capacity, not a measured throughput gain; the asserted rate gain has no support.",
                                _ => "Annotated source says a sustained rate mismatch is only delayed, not fixed; prose claims a permanent cure.",
                            }.into()
                        },
                        correction: self.correction.clone(),
                    }]
                } else {
                    vec![]
                };
                if let Some(finding) = findings.first_mut() {
                    match self.scenario {
                        Scenario::ForeignEvidence => {
                            finding.evidence_ids = vec!["invented-evidence".into()];
                        }
                        Scenario::UnknownTarget => {
                            finding.target.block_id = "nonexistent-block".into();
                        }
                        Scenario::OutsideRange => {
                            finding.range.end_ms = unit.primary.range.end_ms + 1;
                        }
                        _ => {}
                    }
                }
                AnalysisResponse::PassageReview(PassageReview {
                    findings,
                    limitations: vec![],
                })
            }
            AnalysisStage::PassagePatch => {
                let passage: Passage =
                    serde_json::from_value(request.input["passage"].clone()).unwrap();
                AnalysisResponse::PassagePatch(PassagePatch {
                    base_revision: if self.scenario == Scenario::Stale {
                        passage.revision.checked_sub(1).unwrap()
                    } else {
                        passage.revision
                    },
                    operations: vec![PassagePatchOperation {
                        kind: PassagePatchKind::Replace,
                        target: PassageTarget {
                            kind: PassageTargetKind::Block,
                            block_id: passage.blocks[0].id.clone(),
                        },
                        blocks: vec![text_block(&self.correction, &evidence_ids)],
                    }],
                })
            }
            _ => panic!("passage pipeline unexpectedly invoked {:?}", request.stage),
        }
    }
}

impl ModelExecutor for Scripted {
    fn execute<'a>(&'a self, _: ModelRequest, _: &'a Cancellation) -> ModelFuture<'a> {
        Box::pin(async { panic!("Standard/global composition must not run") })
    }

    fn execute_analysis<'a>(
        &'a self,
        request: AnalysisRequest,
        cancel: &'a Cancellation,
    ) -> AnalysisFuture<'a> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(ModelError::Cancelled);
            }
            let stage = request.stage;
            let response = self.response(&request);
            self.requests.lock().unwrap().push(request);
            if self.cancel_stage == Some(stage) {
                // Simulate an adapter completing a response concurrently with cancellation.
                // The host must still refuse to commit it or start another stage.
                cancel.cancel();
            }
            Ok(response)
        })
    }
}

fn text_block(text: &str, evidence_ids: &[String]) -> Block {
    Block {
        id: "model-text-id".into(),
        evidence_ids: evidence_ids.to_vec(),
        content: BlockContent::Text { text: text.into() },
    }
}

fn source(case: &passage::Case, with_image: bool) -> RecapDocument {
    let transcript: Vec<_> = case
        .units
        .iter()
        .filter(|unit| unit.kind == SourceKind::Transcript)
        .map(|unit| TranscriptSegment {
            id: unit.id.clone(),
            range: TimeRange {
                start_ms: unit.start_ms,
                end_ms: unit.end_ms,
            },
            text: unit.text.clone(),
        })
        .collect();
    let duration_ms = case.units.last().unwrap().end_ms;
    let document = RecapDocument {
        schema_version: 1,
        id: "independent-passage-quality".into(),
        revision: 1,
        title: case.id.clone(),
        source: Source {
            id: case.id.clone(),
            title: case.id.clone(),
            url: None,
            duration_ms,
        },
        intent: GenerationIntent {
            depth: Depth::Detailed,
            focus: Some("Emphasize the mechanism, without excluding caveats".into()),
        },
        evidence: transcript
            .iter()
            .map(|cue| Evidence {
                id: format!("e-{}", cue.id),
                range: cue.range,
                transcript_segment_ids: vec![cue.id.clone()],
            })
            .collect(),
        transcript,
        assets: if with_image {
            vec![Asset {
                id: "source-frame".into(),
                path: AssetPath::new("source-frame.png").unwrap(),
                mime_type: "image/png".into(),
                range: TimeRange {
                    start_ms: 0,
                    end_ms: duration_ms,
                },
            }]
        } else {
            vec![]
        },
        sections: vec![],
    };
    document.validate().unwrap();
    document
}

fn policy(window_duration_ms: u64) -> DetailedPolicy {
    DetailedPolicy {
        window_duration_ms,
        window_overlap_ms: 0,
        inspect_visuals: false,
        ..DetailedPolicy::default()
    }
}

async fn compose(
    model: &Scripted,
    source: &RecapDocument,
    policy: &DetailedPolicy,
) -> Result<RecapDocument, CompositionError> {
    compose_passages(
        model,
        source,
        vec![],
        source.intent.clone(),
        None,
        policy,
        &Cancellation::default(),
    )
    .await
}

#[tokio::test]
async fn production_pipeline_rejects_stale_patch_without_changing_source() {
    let corpus = passage::corpus();
    let case = corpus
        .cases
        .iter()
        .find(|c| c.id == "causal_caveats")
        .unwrap();
    let source = source(case, true);
    let before = source.clone();
    let model = Scripted::new(Scenario::Stale, case);
    let error = compose(&model, &source, &policy(180_000))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        CompositionError::Invalid("stale passage patch revision")
    ));
    assert_eq!(source, before);
    let requests = model.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.stage == AnalysisStage::PassagePatch)
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.stage == AnalysisStage::PassageReview)
            .count(),
        1
    );
}

#[tokio::test]
async fn production_local_repair_preserves_unrelated_block_and_image_exactly() {
    let corpus = passage::corpus();
    let case = corpus
        .cases
        .iter()
        .find(|c| c.id == "causal_caveats")
        .unwrap();
    let source = source(case, true);
    let before = source.clone();
    let model = Scripted::new(Scenario::Repair, case);
    let result = compose(&model, &source, &policy(180_000)).await.unwrap();
    assert_eq!(source, before);
    assert_eq!(result.transcript, source.transcript);
    assert_eq!(result.evidence, source.evidence);
    assert_eq!(result.assets, source.assets);
    let requests = model.requests.lock().unwrap().clone();
    let reviews: Vec<_> = requests
        .iter()
        .filter(|r| r.stage == AnalysisStage::PassageReview)
        .collect();
    assert_eq!(
        reviews.len(),
        3,
        "initial, repaired local, independent final source review"
    );
    let initial: Passage = serde_json::from_value(reviews[0].input["passage"].clone()).unwrap();
    let repaired: Passage = serde_json::from_value(reviews[1].input["passage"].clone()).unwrap();
    assert_eq!(repaired.revision, initial.revision + 1);
    assert_eq!(initial.blocks[1..], repaired.blocks[1..]);
    assert_eq!(result.sections[0].blocks[1..3], initial.blocks[1..3]);
    assert_eq!(result.sections[0].blocks[0].id, initial.blocks[0].id);
    assert_eq!(
        result.sections[0].blocks[0].content,
        BlockContent::Text {
            text: case.acceptable.clone()
        }
    );
    for review in reviews {
        let unit: SourceUnit = serde_json::from_value(review.input["source_unit"].clone()).unwrap();
        assert_eq!(unit.primary.transcript, source.transcript);
    }
}

#[tokio::test]
async fn annotated_contradiction_cannot_assemble_when_review_remains_unresolved() {
    let corpus = passage::corpus();
    let case = corpus
        .cases
        .iter()
        .find(|c| c.id == "causal_caveats")
        .unwrap();
    let source = source(case, true);
    let model = Scripted::new(Scenario::Unresolved, case);
    let mut policy = policy(180_000);
    policy.allowances.max_repair_rounds = 0;
    assert!(matches!(
        compose(&model, &source, &policy).await,
        Err(CompositionError::Invalid("unresolved passage findings"))
    ));
    assert!(
        !model
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.stage == AnalysisStage::PassagePatch)
    );
}

#[tokio::test]
async fn production_boundary_reviews_keep_original_source_and_neighbor_prose_separate() {
    let corpus = passage::corpus();
    let case = corpus
        .cases
        .iter()
        .find(|c| c.id == "boundary_explanation")
        .unwrap();
    let source = source(case, false);
    let before = source.clone();
    let model = Scripted::new(Scenario::Boundary, case);
    // The second indivisible source cue lasts 11 seconds. Allow it in full;
    // the planner can still split at the preceding cue's 10-second boundary.
    let result = compose(&model, &source, &policy(11_000)).await.unwrap();
    assert_eq!(source, before);
    assert_eq!(result.sections.len(), 2);
    let requests = model.requests.lock().unwrap().clone();
    let writes: Vec<_> = requests
        .iter()
        .filter(|r| r.stage == AnalysisStage::PassageComposition)
        .collect();
    assert_eq!(writes.len(), 2);
    let mut units: Vec<SourceUnit> = writes
        .iter()
        .map(|r| serde_json::from_value(r.input["source_unit"].clone()).unwrap())
        .collect();
    // Initial requests may complete concurrently; source order is a document
    // invariant, not a promise about executor observation/completion order.
    units.sort_by_key(|unit| unit.primary.range.start_ms);
    let primary: Vec<_> = units
        .iter()
        .flat_map(|unit| unit.primary.transcript.clone())
        .collect();
    assert_eq!(
        primary, source.transcript,
        "no dropped or duplicated primary cues"
    );
    assert_eq!(
        units[1].context.transcript,
        vec![source.transcript[0].clone()]
    );
    let mut finals: Vec<_> = requests
        .iter()
        .filter(|r| {
            r.stage == AnalysisStage::PassageReview
                && !r.input["neighboring_passages_context_only"]
                    .as_array()
                    .unwrap()
                    .is_empty()
        })
        .collect();
    finals.sort_by_key(|review| {
        review.input["source_unit"]["primary"]["range"]["start_ms"]
            .as_u64()
            .unwrap()
    });
    assert_eq!(
        finals.len(),
        2,
        "independent final review must inspect both sides"
    );
    for (index, review) in finals.iter().enumerate() {
        let reviewed_source: SourceUnit =
            serde_json::from_value(review.input["source_unit"].clone()).unwrap();
        assert_eq!(reviewed_source, units[index]);
        let neighbor: Passage =
            serde_json::from_value(review.input["neighboring_passages_context_only"][0].clone())
                .unwrap();
        assert_eq!(
            neighbor.blocks,
            result.sections[1 - index]
                .blocks
                .iter()
                .filter(|b| !b.evidence_ids.is_empty())
                .cloned()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            result.sections[index].blocks[0].content,
            BlockContent::Text {
                text: source.transcript[index].text.clone()
            }
        );
    }
}

#[tokio::test]
async fn clean_local_reviews_do_not_bypass_unresolved_final_boundary_review() {
    let corpus = passage::corpus();
    let case = corpus
        .cases
        .iter()
        .find(|c| c.id == "boundary_explanation")
        .unwrap();
    let source = source(case, false);
    let before = source.clone();
    let model = Scripted::new(Scenario::BoundaryFinding, case);
    assert!(matches!(
        compose(&model, &source, &policy(11_000)).await,
        Err(CompositionError::Invalid(
            "unresolved final source or boundary findings"
        ))
    ));
    assert_eq!(source, before);
}
