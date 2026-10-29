#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Deterministic typed-executor tests: no provider, process or network is involved.
use std::collections::BTreeSet;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use bcode_recap::{
    analysis_contracts::{
        AnalysisRequest, AnalysisResponse, AnalysisStage, ChapterComposition, ChapterPlan,
        CoverageIssue, CoverageIssueKind, DetailedPolicy, DispositionKind, ExtractionInventory,
        InformationItem, InformationKind, ItemDisposition, Outline, SourceAudit, SourceRepair,
        VisualChoice, VisualInspection,
    },
    composition::{Composer, CompositionError, CompositionLimits},
    contracts::{
        Asset, AssetPath, Block, BlockContent, Depth, Evidence, GenerationIntent, RecapDocument,
        Section, Source, TimeRange, TranscriptSegment,
    },
    detailed::compose_detailed,
    model::{
        AnalysisFuture, ImageAsset, ModelError, ModelExecutor, ModelFuture, ModelRequest,
        ModelResponse,
    },
    process::Cancellation,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Scenario {
    OmitLate,
    RefuseRepair,
    Fabricate,
    Aggregate,
    UnseenPoint,
    OversizedChapter,
    Illustration,
    UnrelatedEvidence,
    UnknownEvidence,
    UnseenAsset,
    MissingVisualReason,
    Fanout,
    TwoFindings,
}

#[derive(Default)]
struct Typed {
    scenarios: BTreeSet<Scenario>,
    calls: Mutex<Vec<AnalysisStage>>,
    requests: Mutex<Vec<AnalysisRequest>>,
    standard_calls: AtomicUsize,
    cancel_stage: Option<AnalysisStage>,
    corrupt_stage: Option<AnalysisStage>,
}

fn block(id: &str, evidence: &str, text: &str) -> Block {
    Block {
        id: id.into(),
        evidence_ids: vec![evidence.into()],
        content: BlockContent::Text { text: text.into() },
    }
}

impl ModelExecutor for Typed {
    fn execute<'a>(&'a self, _: ModelRequest, _: &'a Cancellation) -> ModelFuture<'a> {
        Box::pin(async move {
            self.standard_calls.fetch_add(1, Ordering::SeqCst);
            Ok(ModelResponse {
                title: "Standard unchanged".into(),
                sections: vec![Section {
                    id: "standard".into(),
                    title: "Selective overview".into(),
                    blocks: vec![
                        block("standard-text", "e-early", "An overview only."),
                        Block {
                            id: "standard-image".into(),
                            evidence_ids: vec!["e-early".into()],
                            content: BlockContent::Image {
                                asset_id: "frame".into(),
                                caption: "Source frame".into(),
                            },
                        },
                    ],
                }],
                image_choices: [("frame".into(), "Shows the source state".into())].into(),
            })
        })
    }

    fn execute_analysis<'a>(
        &'a self,
        request: AnalysisRequest,
        cancel: &'a Cancellation,
    ) -> AnalysisFuture<'a> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(request.stage);
            self.requests.lock().unwrap().push(request.clone());
            if self.cancel_stage == Some(request.stage) {
                cancel.cancel();
            }
            let mut response = match request.stage {
                AnalysisStage::ExtractionInventory => self.stage_inventory(&request),
                AnalysisStage::VisualInspection => Self::stage_visual(&request),
                AnalysisStage::Outline => Self::stage_outline(&request),
                AnalysisStage::ChapterComposition => self.stage_chapter(&request),
                AnalysisStage::SourceAudit => self.stage_audit(&request),
                AnalysisStage::SourceRepair => self.stage_repair(&request),
            };
            if self.scenarios.contains(&Scenario::OversizedChapter)
                && let AnalysisResponse::ChapterComposition(chapter) = &mut response
            {
                let evidence = chapter.section.blocks[0].evidence_ids[0].clone();
                chapter.section.blocks.push(block(
                    &format!("{}-oversized", chapter.section.id),
                    &evidence,
                    &"Source-grounded explanation. ".repeat(2000),
                ));
            }
            if self.corrupt_stage == Some(request.stage) {
                match &mut response {
                    AnalysisResponse::ExtractionInventory(value) => {
                        value.items[0].range.end_ms = 999_999;
                    }
                    AnalysisResponse::VisualInspection(value) => {
                        value.inspected_asset_ids.push("fake-frame".into());
                    }
                    AnalysisResponse::Outline(value) => {
                        value.dispositions.pop();
                    }
                    AnalysisResponse::ChapterComposition(value) => {
                        value.covered_item_ids.push("fake-item".into());
                    }
                    AnalysisResponse::SourceAudit(value) => {
                        value.checked_item_ids.push("fake-item".into());
                    }
                    AnalysisResponse::SourceRepair(value) => {
                        value.chapters[0].section.id = "fake-chapter".into();
                    }
                }
            }
            Ok(response)
        })
    }
}

impl Typed {
    fn stage_inventory(&self, request: &AnalysisRequest) -> AnalysisResponse {
        let input = &request.input;

        let window = &input["window"];
        let id = window["id"].as_str().unwrap();
        let range: TimeRange = serde_json::from_value(window["range"].clone()).unwrap();
        let late = !self.scenarios.contains(&Scenario::Aggregate) && range.start_ms >= 1000;
        if self.scenarios.contains(&Scenario::Fanout) {
            let items = (0..16)
                .map(|index| InformationItem {
                    id: format!("detail-{index}"),
                    window_id: id.into(),
                    range,
                    kind: InformationKind::Fact,
                    text: format!(
                        "Specific source fact {index}: retain all quantities and qualifications."
                    ),
                    evidence_ids: vec!["e-early".into()],
                })
                .collect();
            return AnalysisResponse::ExtractionInventory(ExtractionInventory {
                window_id: id.into(),
                items,
                limitations: vec![],
            });
        }
        if self.scenarios.contains(&Scenario::Aggregate) {
            return AnalysisResponse::ExtractionInventory(ExtractionInventory {
                window_id: id.into(),
                limitations: vec![],
                items: vec![InformationItem {
                    id: "detail".into(),
                    window_id: id.into(),
                    range: serde_json::from_value(
                        input["untrusted_source"]["transcript"][0]["range"].clone(),
                    )
                    .unwrap(),
                    kind: InformationKind::Fact,
                    text: input["untrusted_source"]["transcript"][0]["text"]
                        .as_str()
                        .unwrap()
                        .into(),
                    evidence_ids: serde_json::from_value(window["evidence_ids"].clone()).unwrap(),
                }],
            });
        }
        if late {
            return AnalysisResponse::ExtractionInventory(ExtractionInventory {
                window_id: id.into(),
                items: vec![],
                limitations: vec!["No transcript in this visual-only interval.".into()],
            });
        }
        AnalysisResponse::ExtractionInventory(ExtractionInventory {
            window_id: id.into(),
            items: vec![InformationItem {
                id: "detail".into(),
                window_id: id.into(),
                range,
                kind: if late {
                    InformationKind::Caveat
                } else {
                    InformationKind::Fact
                },
                text: if late {
                    "Late exception: never retry after commit."
                } else {
                    "First set the queue to 17."
                }
                .into(),
                evidence_ids: vec![
                    if self.scenarios.contains(&Scenario::Fabricate) {
                        "invented"
                    } else if late {
                        "e-late"
                    } else {
                        "e-early"
                    }
                    .into(),
                ],
            }],
            limitations: vec![],
        })
    }
    fn stage_visual(request: &AnalysisRequest) -> AnalysisResponse {
        let input = &request.input;

        AnalysisResponse::VisualInspection(VisualInspection {
            window_id: input["window"]["id"].as_str().unwrap().into(),
            inspected_asset_ids: request
                .images
                .iter()
                .map(|image| image.asset_id.clone())
                .collect(),
            items: if input["window"]["range"]["start_ms"].as_u64().unwrap() >= 1000 {
                vec![InformationItem {
                    id: "visual-detail".into(),
                    window_id: input["window"]["id"].as_str().unwrap().into(),
                    range: serde_json::from_value(input["window"]["range"].clone()).unwrap(),
                    kind: InformationKind::Visual,
                    text: "Late exception: never retry after commit.".into(),
                    evidence_ids: vec!["e-late".into()],
                }]
            } else {
                vec![]
            },
            visual_choices: vec![VisualChoice {
                asset_id: request.images[0].asset_id.clone(),
                reason: "Readable source state".into(),
            }],
            limitations: vec!["Small frame label is unreadable.".into()],
        })
    }
    fn stage_outline(request: &AnalysisRequest) -> AnalysisResponse {
        let input = &request.input;

        if input["outline_level"] == "hierarchical_root" {
            let children: Vec<Outline> = serde_json::from_value(input["children"].clone()).unwrap();
            return AnalysisResponse::Outline(Outline {
                title: "Aggregate recap".into(),
                chapters: children
                    .iter()
                    .flat_map(|child| child.chapters.clone())
                    .collect(),
                dispositions: children
                    .into_iter()
                    .flat_map(|child| child.dispositions)
                    .collect(),
            });
        }
        let items: Vec<InformationItem> = serde_json::from_value(input["items"].clone()).unwrap();
        let chapters: Vec<_> = items
            .iter()
            .enumerate()
            .map(|(index, item)| ChapterPlan {
                id: format!("chapter-{index}"),
                title: item.text.clone(),
                item_ids: vec![item.id.clone()],
            })
            .collect();
        AnalysisResponse::Outline(Outline {
            title: "Complete source account".into(),
            dispositions: items
                .iter()
                .zip(&chapters)
                .map(|(item, chapter)| ItemDisposition {
                    item_id: item.id.clone(),
                    disposition: DispositionKind::Retained,
                    chapter_ids: vec![chapter.id.clone()],
                    duplicate_of: vec![],
                    reason: "Substantive source detail".into(),
                })
                .collect(),
            chapters,
        })
    }
    fn stage_chapter(&self, request: &AnalysisRequest) -> AnalysisResponse {
        let input = &request.input;

        let plan: ChapterPlan = serde_json::from_value(input["chapter"].clone()).unwrap();
        let items: Vec<InformationItem> = serde_json::from_value(input["items"].clone()).unwrap();
        let item = &items[0];
        let text = if self.scenarios.contains(&Scenario::OmitLate) && item.range.start_ms >= 1000 {
            "A late topic is discussed."
        } else {
            &item.text
        };
        let mut chapter = ChapterComposition {
            section: Section {
                id: plan.id.clone(),
                title: plan.title,
                blocks: vec![block(
                    &format!("{}-text", plan.id),
                    &item.evidence_ids[0],
                    text,
                )],
            },
            covered_item_ids: plan.item_ids,
            image_choices: vec![],
        };
        if item.range.start_ms == 0 {
            if self.scenarios.contains(&Scenario::Illustration) {
                let asset_id = if self.scenarios.contains(&Scenario::UnseenAsset) {
                    "late-frame"
                } else {
                    "frame"
                };
                chapter.section.blocks.push(Block {
                    id: format!("{}-visual", chapter.section.id),
                    evidence_ids: vec!["e-frame".into()],
                    content: BlockContent::Image {
                        asset_id: asset_id.into(),
                        caption: "The source illustration".into(),
                    },
                });
                if !self.scenarios.contains(&Scenario::MissingVisualReason) {
                    chapter.image_choices.push(VisualChoice {
                        asset_id: asset_id.into(),
                        reason: "Explains the assigned step".into(),
                    });
                }
            }
            for (scenario, evidence) in [
                (Scenario::UnrelatedEvidence, "e-unrelated"),
                (Scenario::UnknownEvidence, "e-invented"),
            ] {
                if self.scenarios.contains(&scenario) {
                    chapter.section.blocks[0].evidence_ids.push(evidence.into());
                }
            }
        }
        if self.scenarios.contains(&Scenario::Fanout)
            && let BlockContent::Text { text } = &mut chapter.section.blocks[0].content
        {
            text.push_str(&" Additional grounded explanation.".repeat(40));
        }
        AnalysisResponse::ChapterComposition(chapter)
    }
    fn stage_audit(&self, request: &AnalysisRequest) -> AnalysisResponse {
        let input = &request.input;

        let items: Vec<InformationItem> = serde_json::from_value(input["items"].clone()).unwrap();
        let chapters: Vec<ChapterComposition> =
            serde_json::from_value(input["chapters"].clone()).unwrap();
        let mut issues = Vec::new();
        for item in &items {
            if !chapters.iter().flat_map(|chapter| &chapter.section.blocks).any(|block|
                            matches!(&block.content, BlockContent::Text { text } if text.contains(&item.text))) {
                            issues.push(CoverageIssue { id: format!("missing-{}", item.id), kind: CoverageIssueKind::Missing,
                                item_ids: vec![item.id.clone()], chapter_ids: vec![], description: item.text.clone() });
                        }
        }
        if self.scenarios.contains(&Scenario::UnseenPoint) && items.iter().any(|item| item.range.start_ms == 0)
                        && !chapters.iter().flat_map(|chapter| &chapter.section.blocks).any(|block|
                            matches!(&block.content, BlockContent::Text { text } if text.contains("Commit boundary is irreversible."))) {
                        issues.push(CoverageIssue { id: "missed-by-inventory".into(), kind: CoverageIssueKind::Missing,
                            item_ids: vec![], chapter_ids: vec![chapters[0].section.id.clone()],
                            description: "Commit boundary is irreversible.".into() });
                    }
        self.check_adversarial_audit(input, &items, &chapters, &mut issues);
        AnalysisResponse::SourceAudit(SourceAudit {
            checked_item_ids: items.iter().map(|item| item.id.clone()).collect(),
            issues,
            limitations: vec![],
        })
    }
    fn check_adversarial_audit(
        &self,
        input: &serde_json::Value,
        items: &[InformationItem],
        chapters: &[ChapterComposition],
        issues: &mut Vec<CoverageIssue>,
    ) {
        if self.scenarios.contains(&Scenario::Fanout) {
            let ledger = input["source_coverage_ledger"].as_array().unwrap();
            assert_eq!(
                ledger.len(),
                16,
                "source-wide ledger lost out-of-batch facts"
            );
            for item in items {
                assert!(
                    chapters
                        .iter()
                        .any(|chapter| chapter.covered_item_ids.contains(&item.id)),
                    "out-of-batch item audited as missing"
                );
            }
            for index in 0..16 {
                assert!(ledger.iter().any(|entry| {
                    entry["item"]["text"]
                        .as_str()
                        .unwrap()
                        .contains(&format!("fact {index}:"))
                }));
            }
        }
        if self.scenarios.contains(&Scenario::TwoFindings)
            && items.iter().any(|item| item.range.start_ms == 0)
        {
            for (index, fact) in [
                "First missing source qualification.",
                "Second missing source quantity: 19.",
            ]
            .into_iter()
            .enumerate()
            {
                if !chapters.iter().flat_map(|chapter| &chapter.section.blocks).any(|block|
                    matches!(&block.content, BlockContent::Text { text } if text.contains(fact))) {
                    issues.push(CoverageIssue { id: format!("unseen-{index}"), kind: CoverageIssueKind::Missing,
                        item_ids: vec![], chapter_ids: vec![chapters[0].section.id.clone()], description: fact.into() });
                }
            }
        }
    }

    fn stage_repair(&self, request: &AnalysisRequest) -> AnalysisResponse {
        let input = &request.input;

        let issues: Vec<CoverageIssue> = serde_json::from_value(input["issues"].clone()).unwrap();
        let mut chapters: Vec<ChapterComposition> =
            serde_json::from_value(input["chapters"].clone()).unwrap();
        let items: Vec<InformationItem> = serde_json::from_value(input["items"].clone()).unwrap();
        if !self.scenarios.contains(&Scenario::RefuseRepair) {
            for chapter in &mut chapters {
                chapter.section.blocks = items
                    .iter()
                    .filter(|item| chapter.covered_item_ids.contains(&item.id))
                    .enumerate()
                    .map(|(index, item)| {
                        block(
                            &format!("{}-repair-{index}", chapter.section.id),
                            &item.evidence_ids[0],
                            &item.text,
                        )
                    })
                    .collect();
                if self.scenarios.contains(&Scenario::UnseenPoint)
                    || self.scenarios.contains(&Scenario::TwoFindings)
                {
                    for (index, issue) in issues.iter().enumerate() {
                        if issue.item_ids.is_empty() {
                            chapter.section.blocks.push(block(
                                &format!("{}-new-point-{index}", chapter.section.id),
                                &items[0].evidence_ids[0],
                                &issue.description,
                            ));
                        }
                    }
                }
            }
        }
        AnalysisResponse::SourceRepair(SourceRepair {
            addressed_issue_ids: issues.iter().map(|issue| issue.id.clone()).collect(),
            chapters,
            unresolved_issue_ids: vec![],
        })
    }
}

fn source() -> (RecapDocument, Vec<ImageAsset>, DetailedPolicy) {
    let early = TimeRange {
        start_ms: 0,
        end_ms: 1000,
    };
    let late = TimeRange {
        start_ms: 1000,
        end_ms: 2000,
    };
    let document = RecapDocument {
        schema_version: 1,
        id: "recap".into(),
        revision: 1,
        title: "Grounded".into(),
        source: Source {
            id: "video".into(),
            title: "Video".into(),
            url: None,
            duration_ms: 2000,
        },
        intent: GenerationIntent::default(),
        transcript: vec![TranscriptSegment {
            id: "words".into(),
            range: early,
            text: "Set queue to 17.".into(),
        }],
        evidence: vec![
            Evidence {
                id: "e-early".into(),
                range: early,
                transcript_segment_ids: vec!["words".into()],
            },
            Evidence {
                id: "e-late".into(),
                range: late,
                transcript_segment_ids: vec![],
            },
        ],
        assets: vec![
            Asset {
                id: "frame".into(),
                path: AssetPath::new("frame.png").unwrap(),
                mime_type: "image/png".into(),
                range: early,
            },
            Asset {
                id: "late-frame".into(),
                path: AssetPath::new("late.png").unwrap(),
                mime_type: "image/png".into(),
                range: late,
            },
        ],
        sections: vec![],
    };
    let images = vec![
        ImageAsset {
            asset_id: "frame".into(),
            mime_type: "image/png".into(),
            bytes: vec![1, 2, 3],
        },
        ImageAsset {
            asset_id: "late-frame".into(),
            mime_type: "image/png".into(),
            bytes: vec![4, 5, 6],
        },
    ];
    let policy = DetailedPolicy {
        window_duration_ms: 1000,
        window_overlap_ms: 0,
        ..DetailedPolicy::default()
    };
    (document, images, policy)
}

const fn intent() -> GenerationIntent {
    GenerationIntent {
        depth: Depth::Detailed,
        focus: None,
    }
}

#[tokio::test]
async fn missing_late_visual_only_window_is_repaired_and_prior_is_immutable() {
    let (grounded, images, policy) = source();
    let model = Typed {
        scenarios: BTreeSet::from([Scenario::OmitLate]),
        ..Typed::default()
    };
    let before = grounded.clone();
    let result = compose_detailed(
        &model,
        &grounded,
        images.clone(),
        intent(),
        None,
        &policy,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(grounded, before);
    assert!(result.sections.iter().flat_map(|section| &section.blocks).any(|block|
        matches!(&block.content, BlockContent::Text { text } if text.contains("never retry after commit"))));
    assert!(result.sections.iter().flat_map(|section| &section.blocks).any(|block|
        matches!(&block.content, BlockContent::Text { text } if text.contains("Coverage limitations") && text.contains("unreadable"))));
    assert!(
        model
            .calls
            .lock()
            .unwrap()
            .contains(&AnalysisStage::SourceRepair)
    );
    let prior = result.clone();
    let refined = compose_detailed(
        &model,
        &result,
        images,
        intent(),
        Some(&result),
        &policy,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(result, prior);
    assert_eq!(refined.revision, 2);
    assert_eq!(
        refined
            .sections
            .iter()
            .map(|section| &section.id)
            .collect::<Vec<_>>(),
        result
            .sections
            .iter()
            .map(|section| &section.id)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn repair_claim_without_corrected_text_is_not_success() {
    let (grounded, images, policy) = source();
    let model = Typed {
        scenarios: BTreeSet::from([Scenario::OmitLate, Scenario::RefuseRepair]),
        ..Typed::default()
    };
    assert!(
        compose_detailed(
            &model,
            &grounded,
            images,
            intent(),
            None,
            &policy,
            &Cancellation::default()
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn inventory_accepts_only_complete_relevant_host_interval_unions() {
    // Includes shuffled adjacent cues, overlapping cues, clipped enclosing ranges,
    // a one-millisecond interior gap, uncovered endpoints and irrelevant references.
    let cases = [
        (vec![(500, 1000), (0, 500)], true),
        (vec![(0, 600), (400, 1000)], true),
        (vec![(0, 300), (200, 700), (700, 1000)], true),
        (vec![(0, 1000)], true),
        (vec![(0, 1500)], true),
        (vec![(0, 499), (500, 1000)], false),
        (vec![(1, 500), (500, 1000)], false),
        (vec![(0, 500), (500, 999)], false),
        (vec![(0, 1000), (1000, 1500)], false),
    ];
    for (ranges, expected_success) in cases {
        let (mut grounded, _, mut policy) = source();
        grounded.assets.clear();
        grounded.transcript[0].range = TimeRange {
            start_ms: 0,
            end_ms: 1000,
        };
        grounded.transcript[0].text = "One fact jointly explained across several cues.".into();
        grounded.evidence = ranges
            .into_iter()
            .enumerate()
            .map(|(index, (start_ms, end_ms))| Evidence {
                id: format!("cue-{index}"),
                range: TimeRange { start_ms, end_ms },
                transcript_segment_ids: vec![],
            })
            .collect();
        // One analysis window with a range of [0,1000). Keep a longer source only
        // for the unrelated reference, which must not establish inventory coverage.
        grounded.source.duration_ms = 1000;
        if grounded
            .evidence
            .iter()
            .any(|evidence| evidence.range.end_ms > 1000)
        {
            grounded.source.duration_ms = 1500;
            policy.window_duration_ms = 1500;
        }
        let model = Typed {
            scenarios: BTreeSet::from([Scenario::Aggregate]),
            ..Typed::default()
        };
        let result = compose_detailed(
            &model,
            &grounded,
            vec![],
            intent(),
            None,
            &policy,
            &Cancellation::default(),
        )
        .await;
        assert_eq!(
            result.is_ok(),
            expected_success,
            "unexpected evidence union result: {result:?}"
        );
    }
}

#[tokio::test]
async fn chapter_scope_accepts_supplied_illustration_evidence_not_in_inventory() {
    for invalid in [
        None,
        Some(Scenario::UnrelatedEvidence),
        Some(Scenario::UnknownEvidence),
        Some(Scenario::UnseenAsset),
        Some(Scenario::MissingVisualReason),
    ] {
        let (mut grounded, images, policy) = source();
        // The inventory fact refers only to e-early. e-frame is independently
        // registered illustration support in the same source window.
        grounded.evidence.push(Evidence {
            id: "e-frame".into(),
            range: grounded.assets[0].range,
            transcript_segment_ids: vec![],
        });
        grounded.evidence.push(Evidence {
            id: "e-unrelated".into(),
            range: TimeRange {
                start_ms: 700,
                end_ms: 800,
            },
            transcript_segment_ids: vec![],
        });
        let mut scenarios = BTreeSet::from([Scenario::Illustration]);
        if let Some(scenario) = invalid {
            scenarios.insert(scenario);
        }
        let model = Typed {
            scenarios,
            ..Typed::default()
        };
        let result = compose_detailed(
            &model,
            &grounded,
            images,
            intent(),
            None,
            &policy,
            &Cancellation::default(),
        )
        .await;
        assert_eq!(
            result.is_ok(),
            invalid.is_none(),
            "unexpected chapter scope result: {result:?}"
        );
        let requests = model.requests.lock().unwrap().clone();
        let chapter = requests
            .iter()
            .find(|request| request.stage == AnalysisStage::ChapterComposition)
            .unwrap();
        let allowed: Vec<String> =
            serde_json::from_value(chapter.input["allowed_evidence_ids"].clone()).unwrap();
        assert!(allowed.contains(&"e-early".into()));
        assert!(allowed.contains(&"e-frame".into()));
        assert!(!allowed.contains(&"e-unrelated".into()));
        assert!(!allowed.contains(&"e-invented".into()));
        assert_eq!(
            chapter
                .images
                .iter()
                .map(|frame| frame.asset_id.as_str())
                .collect::<Vec<_>>(),
            ["frame"]
        );
    }
}

#[tokio::test]
async fn fanout_audits_scope_items_but_keep_complete_source_ledger() {
    let (mut grounded, _, mut policy) = source();
    grounded.source.duration_ms = 1000;
    grounded.assets.clear();
    grounded.evidence.truncate(1);
    grounded.transcript[0].text = (0..16)
        .map(|index| {
            format!("Specific source fact {index}: retain all quantities and qualifications.")
        })
        .collect::<Vec<_>>()
        .join("\n");
    policy.allowances.max_input_bytes = 24_000;
    policy.allowances.max_model_calls = 256;
    let model = Typed {
        scenarios: BTreeSet::from([Scenario::Fanout]),
        ..Typed::default()
    };
    let result = compose_detailed(
        &model,
        &grounded,
        vec![],
        intent(),
        None,
        &policy,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.sections.len(), 16);
    let requests = model.requests.lock().unwrap().clone();
    let audits: Vec<_> = requests
        .iter()
        .filter(|request| request.input["purpose"] == "direct_source_coverage")
        .collect();
    assert!(audits.len() > 1, "fixture did not force audit partitioning");
    let checked: BTreeSet<_> = audits
        .iter()
        .flat_map(|request| {
            request.input["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["id"].as_str().unwrap())
        })
        .collect();
    assert_eq!(checked.len(), 16);
    for request in audits {
        assert!(request.input["chapters"].as_array().unwrap().len() < 16);
        assert_eq!(
            request.input["source_coverage_ledger"]
                .as_array()
                .unwrap()
                .len(),
            16
        );
        assert!(
            serde_json::to_vec(&request.input).unwrap().len() + request.instructions.len()
                <= policy.allowances.max_input_bytes
        );
    }
    assert!(
        !model
            .calls
            .lock()
            .unwrap()
            .contains(&AnalysisStage::SourceRepair)
    );
}

#[tokio::test]
async fn shared_chapter_findings_are_repaired_together_without_overwriting() {
    let (mut grounded, images, policy) = source();
    grounded.transcript[0]
        .text
        .push_str(" First missing source qualification. Second missing source quantity: 19.");
    let model = Typed {
        scenarios: BTreeSet::from([Scenario::TwoFindings]),
        ..Typed::default()
    };
    let result = compose_detailed(
        &model,
        &grounded,
        images,
        intent(),
        None,
        &policy,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    let text = result
        .sections
        .iter()
        .flat_map(|section| &section.blocks)
        .filter_map(|block| {
            if let BlockContent::Text { text } = &block.content {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("First missing source qualification."));
    assert!(text.contains("Second missing source quantity: 19."));
    let requests = model.requests.lock().unwrap().clone();
    let repairs: Vec<_> = requests
        .iter()
        .filter(|request| request.stage == AnalysisStage::SourceRepair)
        .collect();
    assert_eq!(repairs.len(), 1);
    assert_eq!(repairs[0].input["issues"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn image_registry_validation_rejects_duplicates_wrong_identity_and_mime() {
    for kind in 0..4 {
        let (grounded, mut images, policy) = source();
        match kind {
            0 => images.push(images[0].clone()),
            1 => images[0].asset_id = "unknown-frame".into(),
            2 => images[0].mime_type = "image/jpeg".into(),
            _ => images[0].bytes.clear(),
        }
        let model = Typed::default();
        assert!(
            compose_detailed(
                &model,
                &grounded,
                images,
                intent(),
                None,
                &policy,
                &Cancellation::default()
            )
            .await
            .is_err()
        );
        assert!(model.calls.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn fabricated_inventory_ids_fail_before_outlining() {
    let (grounded, images, policy) = source();
    let model = Typed {
        scenarios: BTreeSet::from([Scenario::Fabricate]),
        ..Typed::default()
    };
    assert!(
        compose_detailed(
            &model,
            &grounded,
            images,
            intent(),
            None,
            &policy,
            &Cancellation::default()
        )
        .await
        .is_err()
    );
    assert_eq!(
        *model.calls.lock().unwrap(),
        [AnalysisStage::ExtractionInventory]
    );
}

#[tokio::test]
async fn cancellation_before_and_during_stages_never_succeeds() {
    for stage in [
        None,
        Some(AnalysisStage::ExtractionInventory),
        Some(AnalysisStage::SourceAudit),
    ] {
        let (grounded, images, policy) = source();
        let cancel = Cancellation::default();
        let model = Typed {
            cancel_stage: stage,
            ..Typed::default()
        };
        if stage.is_none() {
            cancel.cancel();
        }
        assert!(matches!(
            compose_detailed(&model, &grounded, images, intent(), None, &policy, &cancel).await,
            Err(CompositionError::Model(ModelError::Cancelled))
        ));
    }
}

#[tokio::test]
async fn explicit_call_window_input_output_and_image_allowances_fail_closed() {
    for kind in 0..5 {
        let (grounded, images, mut policy) = source();
        match kind {
            0 => policy.allowances.max_model_calls = 1,
            1 => policy.allowances.max_source_windows = 1,
            2 => policy.allowances.max_input_bytes = 1,
            3 => policy.allowances.max_response_bytes = 1,
            _ => policy.allowances.max_image_bytes = 1,
        }
        assert!(matches!(
            compose_detailed(
                &Typed::default(),
                &grounded,
                images,
                intent(),
                None,
                &policy,
                &Cancellation::default()
            )
            .await,
            Err(CompositionError::Budget)
        ));
    }
}

#[tokio::test]
async fn coordinator_keeps_standard_on_original_executor_path() {
    let (grounded, images, _) = source();
    let model = Typed::default();
    let composer = Composer {
        executor: &model,
        limits: CompositionLimits::default(),
    };
    let result = composer
        .compose(
            &grounded,
            images,
            GenerationIntent::default(),
            None,
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.title, "Standard unchanged");
    assert_eq!(model.standard_calls.load(Ordering::SeqCst), 1);
    assert!(model.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn every_stage_rejects_fabricated_or_incomplete_identity_sets() {
    for stage in [
        AnalysisStage::ExtractionInventory,
        AnalysisStage::VisualInspection,
        AnalysisStage::Outline,
        AnalysisStage::ChapterComposition,
        AnalysisStage::SourceAudit,
        AnalysisStage::SourceRepair,
    ] {
        let (grounded, images, policy) = source();
        let model = Typed {
            corrupt_stage: Some(stage),
            scenarios: if stage == AnalysisStage::SourceRepair {
                BTreeSet::from([Scenario::OmitLate])
            } else {
                BTreeSet::new()
            },
            ..Typed::default()
        };
        assert!(
            compose_detailed(
                &model,
                &grounded,
                images,
                intent(),
                None,
                &policy,
                &Cancellation::default()
            )
            .await
            .is_err(),
            "accepted corrupt {stage:?}"
        );
    }
}

#[tokio::test]
async fn coordinator_routes_detailed_without_calling_standard() {
    let (grounded, images, policy) = source();
    let model = Typed::default();
    let composer = Composer {
        executor: &model,
        limits: CompositionLimits::default(),
    };
    let result = composer
        .compose_with_policy(
            &grounded,
            images,
            intent(),
            None,
            &policy,
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.intent.depth, Depth::Detailed);
    assert_eq!(model.standard_calls.load(Ordering::SeqCst), 0);
    assert!(
        model
            .calls
            .lock()
            .unwrap()
            .contains(&AnalysisStage::SourceAudit)
    );
}

#[derive(Debug, Default)]
struct Progress(Mutex<Vec<(bcode_recap::diagnostics::Phase, u64)>>);

impl bcode_recap::diagnostics::Observer for Progress {
    fn observe(&self, event: bcode_recap::diagnostics::Event) {
        if let bcode_recap::diagnostics::Event::AnalysisProgress {
            phase, completed, ..
        } = event
        {
            self.0.lock().unwrap().push((phase, completed));
        }
    }
}

#[tokio::test]
async fn stage_progress_is_content_free_and_monotonic_per_phase() {
    use bcode_recap::diagnostics::Phase;
    let (grounded, images, policy) = source();
    let progress = Arc::new(Progress::default());
    let cancel = Cancellation::default().observed(progress.clone());
    let model = Typed {
        scenarios: BTreeSet::from([Scenario::OmitLate]),
        ..Typed::default()
    };
    compose_detailed(&model, &grounded, images, intent(), None, &policy, &cancel)
        .await
        .unwrap();
    let events = progress.0.lock().unwrap().clone();
    for phase in [Phase::Analysis, Phase::Writing, Phase::Audit, Phase::Repair] {
        let values: Vec<_> = events
            .iter()
            .filter(|(candidate, _)| *candidate == phase)
            .map(|(_, completed)| *completed)
            .collect();
        assert_eq!(values.first(), Some(&0));
        assert!(values.last().unwrap() > &0);
        assert!(values.windows(2).all(|pair| pair[0] <= pair[1]));
    }
}

#[tokio::test]
async fn missing_source_point_without_inventory_identity_is_repaired() {
    let (mut grounded, images, policy) = source();
    grounded.transcript[0]
        .text
        .push_str(" Commit boundary is irreversible.");
    let model = Typed {
        scenarios: BTreeSet::from([Scenario::UnseenPoint]),
        ..Typed::default()
    };
    let result = compose_detailed(
        &model,
        &grounded,
        images,
        intent(),
        None,
        &policy,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert!(result.sections.iter().flat_map(|section| &section.blocks).any(|block|
        matches!(&block.content, BlockContent::Text { text } if text.contains("Commit boundary is irreversible."))));
    let requests = model.requests.lock().unwrap().clone();
    let repair = requests
        .iter()
        .find(|request| request.stage == AnalysisStage::SourceRepair)
        .unwrap();
    assert_eq!(repair.input["issues"][0]["item_ids"], serde_json::json!([]));
    assert!(
        !repair.input["untrusted_sources"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn indivisible_chapter_is_rejected_before_an_oversized_audit_dispatch() {
    let (grounded, images, mut policy) = source();
    policy.allowances.max_input_bytes = 24_000;
    let model = Typed {
        scenarios: BTreeSet::from([Scenario::OversizedChapter]),
        ..Typed::default()
    };
    assert!(matches!(
        compose_detailed(
            &model,
            &grounded,
            images,
            intent(),
            None,
            &policy,
            &Cancellation::default()
        )
        .await,
        Err(CompositionError::Budget)
    ));
    assert!(
        model
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| serde_json::to_vec(&request.input).unwrap().len()
                + request.instructions.len()
                <= policy.allowances.max_input_bytes)
    );
}

#[tokio::test]
async fn aggregate_document_larger_than_a_request_completes_with_bounded_consistency() {
    let (mut grounded, _, mut policy) = source();
    grounded.source.duration_ms = 24_000;
    grounded.assets.clear();
    grounded.evidence.clear();
    grounded.transcript.clear();
    for index in 0..24 {
        let range = TimeRange {
            start_ms: index * 1000,
            end_ms: (index + 1) * 1000,
        };
        grounded.transcript.push(TranscriptSegment {
            id: format!("words-{index}"),
            range,
            text: format!(
                "Detail {index}: {}",
                "specific source information; ".repeat(45)
            ),
        });
        grounded.evidence.push(Evidence {
            id: format!("e-{index}"),
            range,
            transcript_segment_ids: vec![format!("words-{index}")],
        });
    }
    policy.allowances.max_input_bytes = 24_000;
    policy.allowances.max_model_calls = 256;
    let model = Typed {
        scenarios: BTreeSet::from([Scenario::Aggregate]),
        ..Typed::default()
    };
    let result = compose_detailed(
        &model,
        &grounded,
        vec![],
        intent(),
        None,
        &policy,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.sections.len(), 24);
    assert!(serde_json::to_vec(&result).unwrap().len() > policy.allowances.max_input_bytes);
    let requests = model.requests.lock().unwrap().clone();
    assert!(
        requests
            .iter()
            .all(|request| serde_json::to_vec(&request.input).unwrap().len()
                + request.instructions.len()
                <= policy.allowances.max_input_bytes)
    );
    let final_checks: Vec<_> = requests
        .iter()
        .filter(|request| {
            request.input["purpose"]
                .as_str()
                .is_some_and(|purpose| purpose.starts_with("independent_final_consistency"))
        })
        .collect();
    assert_eq!(final_checks.len(), 24 + 24 + 23);
    for section in &result.sections {
        assert!(final_checks.iter().any(|request| {
            request.input["chapters"]
                .as_array()
                .unwrap()
                .iter()
                .any(|chapter| chapter["section"]["id"] == section.id)
        }));
    }
    for pair in result.sections.windows(2) {
        assert!(final_checks.iter().any(|request| {
            let chapters = request.input["chapters"].as_array().unwrap();
            chapters.len() == 2
                && chapters[0]["section"]["id"] == pair[0].id
                && chapters[1]["section"]["id"] == pair[1].id
        }));
    }
    for index in 0..24 {
        assert!(final_checks.iter().any(|request| {
            request.input["untrusted_sources"]
                .as_array()
                .unwrap()
                .iter()
                .any(|source| source["window"]["id"] == format!("window-{index}"))
        }));
    }
}
