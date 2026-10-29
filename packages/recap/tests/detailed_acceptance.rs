#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Actual staged composition with annotation-backed, source-checking fake inference.
//! This proves request/retention mechanics, not live inference or visual recognition.

#[path = "fixtures/detailed/mod.rs"]
mod detailed;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

use base64::Engine as _;
use bcode_recap::{
    analysis_contracts::{
        AnalysisRequest, AnalysisResponse, AnalysisStage, ChapterComposition, ChapterPlan,
        CoverageIssue, CoverageIssueKind, DetailedPolicy, DispositionKind, ExtractionInventory,
        InformationItem, InformationKind, ItemDisposition, Outline, SourceAudit, SourceRepair,
        SourceWindow, VisualChoice, VisualInspection,
    },
    composition::{Composer, CompositionError, CompositionLimits},
    contracts::{Block, BlockContent, Depth, GenerationIntent, RecapDocument, Section, TimeRange},
    detailed::compose_detailed,
    model::{AnalysisFuture, ImageAsset, ModelExecutor, ModelFuture, ModelRequest, ModelResponse},
    process::Cancellation,
};
use detailed::{Case, Item, corpus, coverage};
use serde_json::Value;

#[derive(Default)]
struct Trace {
    source_seen: BTreeMap<String, BTreeSet<String>>,
    images_seen: BTreeMap<String, BTreeSet<String>>,
    findings: usize,
    repairs: usize,
    clean_audits_after_repair: usize,
    standard_calls: usize,
}

struct AnnotatedExecutor<'a> {
    case: &'a Case,
    source: RecapDocument,
    images: Vec<ImageAsset>,
    omit: Option<&'a str>,
    second_omit: Option<&'a str>,
    group_chapter: bool,
    ineffective_repair: bool,
    trace: Mutex<Trace>,
}

const fn intersects(a: TimeRange, b: TimeRange) -> bool {
    a.start_ms < b.end_ms && b.start_ms < a.end_ms
}

fn decode<T: serde::de::DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).unwrap()
}

fn synthetic_images(source: &RecapDocument) -> Vec<ImageAsset> {
    // Valid tiny PNG for transport/identity tests only. It does NOT depict the
    // annotated panel; no visual extraction or perception success is claimed.
    let png = base64::engine::general_purpose::STANDARD.decode(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aX1sAAAAASUVORK5CYII="
    ).unwrap();
    source
        .assets
        .iter()
        .map(|asset| ImageAsset {
            asset_id: asset.id.clone(),
            mime_type: asset.mime_type.clone(),
            bytes: png.clone(),
        })
        .collect()
}

impl<'a> AnnotatedExecutor<'a> {
    fn new(case: &'a Case, omit: Option<&'a str>, ineffective_repair: bool) -> Self {
        let source = case.grounded(Depth::Detailed);
        let images = synthetic_images(&source);
        Self {
            case,
            source,
            images,
            omit,
            second_omit: None,
            group_chapter: false,
            ineffective_repair,
            trace: Mutex::default(),
        }
    }

    fn annotation(&self, item: &InformationItem) -> &Item {
        self.case.items.iter().find(|annotation| {
            (item.id == annotation.id || item.id.ends_with(&format!("-{}", annotation.id)))
                && item.text == annotation.summary
        }).expect("stage must preserve the complete annotated information, including quantities and caveats")
    }

    // Independently derive expected content from timestamp overlap, rather than
    // trusting the incoming window's ID lists. Compare exact source contracts.
    fn verify_source(&self, source: &Value, stage: &str) -> BTreeSet<String> {
        let range: TimeRange = decode(&source["range"]);
        let transcript: Vec<_> = self
            .source
            .transcript
            .iter()
            .filter(|segment| intersects(segment.range, range))
            .collect();
        let evidence: Vec<_> = self
            .source
            .evidence
            .iter()
            .filter(|evidence| intersects(evidence.range, range))
            .collect();
        let assets: Vec<_> = self
            .source
            .assets
            .iter()
            .filter(|asset| intersects(asset.range, range))
            .collect();
        assert_eq!(
            source["transcript"],
            serde_json::to_value(&transcript).unwrap()
        );
        assert_eq!(source["evidence"], serde_json::to_value(&evidence).unwrap());
        assert_eq!(source["assets"], serde_json::to_value(&assets).unwrap());
        self.trace
            .lock()
            .unwrap()
            .source_seen
            .entry(stage.into())
            .or_default()
            .extend(evidence.iter().map(|evidence| evidence.id.clone()));
        assets.iter().map(|asset| asset.id.clone()).collect()
    }

    fn verify_images(&self, request: &AnalysisRequest, expected: &BTreeSet<String>, stage: &str) {
        let actual: BTreeSet<_> = request
            .images
            .iter()
            .map(|image| image.asset_id.clone())
            .collect();
        assert_eq!(
            &actual, expected,
            "stage must receive every relevant registered image"
        );
        assert_eq!(actual.len(), request.images.len());
        for image in &request.images {
            assert_eq!(
                Some(image),
                self.images
                    .iter()
                    .find(|original| original.asset_id == image.asset_id)
            );
        }
        self.trace
            .lock()
            .unwrap()
            .images_seen
            .entry(stage.into())
            .or_default()
            .extend(actual);
    }

    fn verify_sources(&self, request: &AnalysisRequest, stage: &str) {
        let mut assets = BTreeSet::new();
        let sources = request.input["untrusted_sources"].as_array().unwrap();
        assert!(!sources.is_empty());
        for entry in sources {
            assets.extend(self.verify_source(&entry["source"], stage));
        }
        self.verify_images(request, &assets, stage);
        if let Some(items) = request.input.get("items") {
            for item in decode::<Vec<InformationItem>>(items) {
                self.annotation(&item);
                assert!(
                    sources.iter().any(|entry| {
                        entry["source"]["evidence"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|evidence| {
                                item.evidence_ids.iter().any(|id| evidence["id"] == *id)
                            })
                    }),
                    "assigned item needs its direct source, not just an inventory summary"
                );
            }
        }
    }

    fn inventory(&self, window: &SourceWindow, visual: bool) -> Vec<InformationItem> {
        self.case
            .items
            .iter()
            .filter(|item| item.visual_asset_id.is_some() == visual)
            .filter_map(|item| {
                let evidence_id = item
                    .evidence_ids
                    .iter()
                    .find(|id| window.evidence_ids.contains(id))?;
                let evidence = self
                    .source
                    .evidence
                    .iter()
                    .find(|evidence| evidence.id == *evidence_id)
                    .unwrap();
                Some(InformationItem {
                    id: item.id.clone(),
                    window_id: window.id.clone(),
                    range: evidence.range,
                    kind: if visual {
                        InformationKind::Visual
                    } else {
                        InformationKind::Fact
                    },
                    text: item.summary.clone(),
                    evidence_ids: vec![evidence_id.clone()],
                })
            })
            .collect()
    }

    fn render(
        &self,
        plan: &ChapterPlan,
        items: &[InformationItem],
        omit: bool,
    ) -> ChapterComposition {
        let mut blocks = Vec::new();
        let mut choices = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let annotation = self.annotation(item);
            let dropped =
                omit && [self.omit, self.second_omit].contains(&Some(annotation.id.as_str()));
            let content = annotation.visual_asset_id.as_ref().map_or_else(
                || BlockContent::Text {
                    text: if dropped {
                        "An explanation is provided.".into()
                    } else {
                        item.text.clone()
                    },
                },
                |asset_id| BlockContent::Image {
                    asset_id: asset_id.clone(),
                    caption: item.text.clone(),
                },
            );
            blocks.push(Block {
                id: format!("{}-block-{index}", plan.id),
                evidence_ids: item.evidence_ids.clone(),
                content,
            });
            if let Some(asset_id) = &annotation.visual_asset_id {
                choices.push(VisualChoice {
                    asset_id: asset_id.clone(),
                    reason:
                        "Shows the annotated source panel state; synthetic transport fixture only"
                            .into(),
                });
            }
        }
        if !choices.is_empty() {
            blocks.push(Block {
                id: format!("{}-context", plan.id),
                evidence_ids: items[0].evidence_ids.clone(),
                content: BlockContent::Text {
                    text: "Source panel observation; no speech is available.".into(),
                },
            });
        }
        ChapterComposition {
            section: Section {
                id: plan.id.clone(),
                title: plan.title.clone(),
                blocks,
            },
            covered_item_ids: items.iter().map(|item| item.id.clone()).collect(),
            image_choices: choices,
        }
    }

    fn respond(&self, request: &AnalysisRequest) -> AnalysisResponse {
        // Source text is product input here, never repository source-code inspection.
        assert!(!request.instructions.contains("UNTRUSTED_FIXTURE_DIRECTIVE"));
        assert!(!request.instructions.trim().is_empty());
        assert!(request.max_response_bytes > 0);
        match request.stage {
            AnalysisStage::ExtractionInventory | AnalysisStage::VisualInspection => {
                self.extract(request)
            }
            AnalysisStage::Outline => self.outline(request),
            AnalysisStage::ChapterComposition => {
                self.verify_sources(request, "chapter");
                let plan = decode(&request.input["chapter"]);
                let items: Vec<InformationItem> = decode(&request.input["items"]);
                AnalysisResponse::ChapterComposition(self.render(&plan, &items, true))
            }
            AnalysisStage::SourceAudit => self.audit(request),
            AnalysisStage::SourceRepair => self.repair(request),
            AnalysisStage::PassageComposition
            | AnalysisStage::PassageReview
            | AnalysisStage::PassagePatch => {
                panic!("passage stage reached the old Detailed acceptance adapter")
            }
        }
    }

    fn extract(&self, request: &AnalysisRequest) -> AnalysisResponse {
        let window: SourceWindow = decode(&request.input["window"]);
        let visual = request.stage == AnalysisStage::VisualInspection;
        let stage = if visual { "visual" } else { "inventory" };
        let assets = self.verify_source(&request.input["untrusted_source"], stage);
        self.verify_images(
            request,
            &if visual { assets } else { BTreeSet::new() },
            stage,
        );
        let items = self.inventory(&window, visual);
        let limitations = if items.is_empty() {
            vec!["No substantive information of this modality in this window.".into()]
        } else {
            vec![]
        };
        if visual {
            AnalysisResponse::VisualInspection(VisualInspection {
                window_id: window.id,
                inspected_asset_ids: request
                    .images
                    .iter()
                    .map(|image| image.asset_id.clone())
                    .collect(),
                items,
                visual_choices: vec![],
                limitations,
            })
        } else {
            AnalysisResponse::ExtractionInventory(ExtractionInventory {
                window_id: window.id,
                items,
                limitations,
            })
        }
    }

    fn outline(&self, request: &AnalysisRequest) -> AnalysisResponse {
        assert_eq!(request.input["intent"]["depth"], "detailed");
        assert!(request.images.is_empty());
        let items: Vec<InformationItem> = decode(&request.input["items"]);
        for item in &items {
            self.annotation(item);
        }
        // Chronology follows annotation order, not window text/visual execution order.
        let mut items: Vec<_> = items.iter().collect();
        items.sort_by_key(|item| {
            self.case
                .items
                .iter()
                .position(|annotation| annotation.id == self.annotation(item).id)
                .unwrap()
        });
        let chapters: Vec<_> = if self.group_chapter {
            vec![ChapterPlan {
                id: "interview".into(),
                title: self.case.title.clone(),
                item_ids: items.iter().map(|item| item.id.clone()).collect(),
            }]
        } else {
            items
                .iter()
                .enumerate()
                .map(|(index, item)| ChapterPlan {
                    id: format!("chapter-{index}"),
                    title: self.annotation(item).id.clone(),
                    item_ids: vec![item.id.clone()],
                })
                .collect()
        };
        let dispositions = items
            .iter()
            .map(|item| ItemDisposition {
                item_id: item.id.clone(),
                disposition: DispositionKind::Retained,
                chapter_ids: chapters
                    .iter()
                    .filter(|chapter| chapter.item_ids.contains(&item.id))
                    .map(|chapter| chapter.id.clone())
                    .collect(),
                duplicate_of: vec![],
                reason: "Distinct substantive annotated source information".into(),
            })
            .collect();
        AnalysisResponse::Outline(Outline {
            title: self.case.title.clone(),
            chapters,
            dispositions,
        })
    }

    fn audit(&self, request: &AnalysisRequest) -> AnalysisResponse {
        self.verify_sources(request, "audit");
        let items: Vec<InformationItem> = decode(&request.input["items"]);
        let chapters: Vec<ChapterComposition> = decode(&request.input["chapters"]);
        let mut candidate = self.source.clone();
        candidate.sections = chapters
            .iter()
            .map(|chapter| chapter.section.clone())
            .collect();
        candidate.validate().unwrap();
        let report = coverage(self.case, &candidate, Depth::Detailed);
        let issues: Vec<_> = items
            .iter()
            .filter_map(|item| {
                let annotation = self.annotation(item);
                if !report.missing.contains(&annotation.id) {
                    return None;
                }
                let chapter = chapters
                    .iter()
                    .find(|chapter| chapter.covered_item_ids.contains(&item.id))
                    .unwrap();
                Some(CoverageIssue {
                    id: format!("missing-{}", annotation.id),
                    kind: CoverageIssueKind::Missing,
                    item_ids: vec![item.id.clone()],
                    chapter_ids: vec![chapter.section.id.clone()],
                    description: format!(
                        "Restore the complete source information: {}",
                        annotation.summary
                    ),
                })
            })
            // Reveal one independent defect per audit to exercise sequential repairs.
            .take(if self.group_chapter { 1 } else { usize::MAX })
            .collect();
        let mut trace = self.trace.lock().unwrap();
        trace.findings += issues.len();
        if trace.repairs > 0 && issues.is_empty() {
            trace.clean_audits_after_repair += 1;
        }
        drop(trace);
        AnalysisResponse::SourceAudit(SourceAudit {
            checked_item_ids: items.iter().map(|item| item.id.clone()).collect(),
            issues,
            limitations: vec![],
        })
    }

    fn repair(&self, request: &AnalysisRequest) -> AnalysisResponse {
        self.verify_sources(request, "repair");
        let issues: Vec<CoverageIssue> = decode(&request.input["issues"]);
        let plans: Vec<ChapterPlan> = decode(&request.input["chapter_plans"]);
        let items: Vec<InformationItem> = decode(&request.input["items"]);
        let current: Vec<ChapterComposition> = decode(&request.input["chapters"]);
        assert!(!issues.is_empty());
        assert_eq!(plans.len(), 1, "repair must be narrowly targeted");
        assert_eq!(current.len(), 1);
        assert!(
            issues[0]
                .item_ids
                .iter()
                .all(|id| plans[0].item_ids.contains(id))
        );
        let chapters = plans
            .iter()
            .map(|plan| {
                let assigned: Vec<_> = items
                    .iter()
                    .filter(|item| plan.item_ids.contains(&item.id))
                    .cloned()
                    .collect();
                let mut proposal = current
                    .iter()
                    .find(|chapter| chapter.section.id == plan.id)
                    .expect("repair must receive the current complete chapter")
                    .clone();
                if !self.ineffective_repair {
                    let corrected = self.render(plan, &assigned, false);
                    // Preserve the current chapter, including earlier corrections and
                    // unrelated text. Only active issues authorize new corrections;
                    // historical preservation context is not a request to rebuild.
                    for (item, block) in assigned.iter().zip(corrected.section.blocks) {
                        if issues.iter().any(|issue| issue.item_ids.contains(&item.id)) {
                            let target = proposal
                                .section
                                .blocks
                                .iter_mut()
                                .find(|current| current.id == block.id)
                                .expect("active correction must target an existing block");
                            *target = block;
                        }
                    }
                }
                proposal
            })
            .collect();
        self.trace.lock().unwrap().repairs += 1;
        AnalysisResponse::SourceRepair(SourceRepair {
            addressed_issue_ids: issues.iter().map(|issue| issue.id.clone()).collect(),
            chapters,
            unresolved_issue_ids: vec![],
        })
    }
}

impl ModelExecutor for AnnotatedExecutor<'_> {
    fn execute<'a>(&'a self, request: ModelRequest, _cancel: &'a Cancellation) -> ModelFuture<'a> {
        Box::pin(async move {
            let input: Value = serde_json::from_str(&request.input_text).unwrap();
            assert_eq!(input["intent"]["depth"], "standard");
            let source: RecapDocument = decode(&input["untrusted_source"]);
            assert_eq!(source, self.source);
            assert_eq!(request.images, self.images);
            assert!(!request.instructions.contains("UNTRUSTED_FIXTURE_DIRECTIVE"));
            let expected = self.case.expected_document(Depth::Standard);
            let mut sections = expected.sections;
            // Standard also requires readable text alongside selected source images.
            let context_evidence = sections[0].blocks[0].evidence_ids.clone();
            sections[0].blocks.push(Block {
                id: "standard-context".into(),
                evidence_ids: context_evidence,
                content: BlockContent::Text {
                    text: "Selected source panel observation.".into(),
                },
            });
            let image_choices = self
                .case
                .items
                .iter()
                .filter(|item| item.standard_required)
                .filter_map(|item| {
                    item.visual_asset_id
                        .as_ref()
                        .map(|id| (id.clone(), "Shows a selected source state".into()))
                })
                .collect();
            self.trace.lock().unwrap().standard_calls += 1;
            Ok(ModelResponse {
                title: self.case.title.clone(),
                sections,
                image_choices,
            })
        })
    }

    fn execute_analysis<'a>(
        &'a self,
        request: AnalysisRequest,
        _cancel: &'a Cancellation,
    ) -> AnalysisFuture<'a> {
        Box::pin(async move { Ok(self.respond(&request)) })
    }
}

fn policy() -> DetailedPolicy {
    DetailedPolicy {
        source_passages: false,
        window_duration_ms: 600_000,
        window_overlap_ms: 0,
        ..DetailedPolicy::default()
    }
}

#[tokio::test]
async fn corpus_survives_actual_staged_composition_and_targeted_repair() {
    for case in corpus().cases {
        let omit = (case.id == "dense_numbers_examples_caveats").then_some("scope_limit");
        let executor = AnnotatedExecutor::new(&case, omit, false);
        let before = executor.source.clone();
        let output = compose_detailed(
            &executor,
            &executor.source,
            executor.images.clone(),
            GenerationIntent {
                depth: Depth::Detailed,
                ..GenerationIntent::default()
            },
            None,
            &policy(),
            &Cancellation::default(),
        )
        .await
        .unwrap();
        output.validate().unwrap();
        assert!(
            coverage(&case, &output, Depth::Detailed).is_complete(),
            "{}",
            case.id
        );
        assert_eq!(executor.source, before, "operation must not mutate source");
        assert_eq!(output.transcript, before.transcript);
        assert_eq!(output.evidence, before.evidence);
        assert_eq!(output.assets, before.assets);
        let trace = executor.trace.lock().unwrap();
        let all: BTreeSet<_> = case
            .observations
            .iter()
            .map(|observation| observation.id.clone())
            .collect();
        for stage in ["inventory", "chapter", "audit"] {
            assert_eq!(
                trace.source_seen.get(stage),
                Some(&all),
                "{} {stage}: full source including late spans and untrusted text",
                case.id
            );
        }
        if case.id == "late_video_details" {
            assert_eq!(output.sections.len(), 16);
        }
        if omit.is_some() {
            assert_eq!(trace.findings, 1);
            assert_eq!(trace.repairs, 1);
            assert!(
                trace.clean_audits_after_repair >= 2,
                "repair must be checked again against source and final consistency"
            );
            assert!(trace.source_seen["repair"].contains("caveat"));
        }
        if !executor.images.is_empty() {
            let all_images = executor
                .images
                .iter()
                .map(|image| image.asset_id.clone())
                .collect();
            for stage in ["visual", "chapter", "audit"] {
                assert_eq!(trace.images_seen.get(stage), Some(&all_images));
            }
            assert!(output.transcript.is_empty());
        }
    }
}

#[tokio::test]
async fn claimed_repair_without_restored_content_is_rejected() {
    let corpus = corpus();
    let case = corpus
        .cases
        .iter()
        .find(|case| case.id == "dense_numbers_examples_caveats")
        .unwrap();
    let executor = AnnotatedExecutor::new(case, Some("scope_limit"), true);
    let before = executor.source.clone();
    let error = compose_detailed(
        &executor,
        &executor.source,
        executor.images.clone(),
        GenerationIntent {
            depth: Depth::Detailed,
            ..GenerationIntent::default()
        },
        None,
        &policy(),
        &Cancellation::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(executor.source, before);
    let trace = executor.trace.lock().unwrap();
    match error {
        CompositionError::Invalid("source repair made no progress or repeated a candidate") => {
            // A verified identical candidate cannot restore the missing meaning.
            // Rejecting it immediately legitimately avoids another source audit.
            assert_eq!(trace.repairs, 1);
            assert_eq!(trace.findings, 1);
        }
        CompositionError::Invalid("unresolved source coverage gaps") => {
            // Re-auditing until the repair allowance is exhausted is also safe.
            assert_eq!(trace.repairs, policy().allowances.max_repair_rounds);
            assert!(trace.findings > trace.repairs);
        }
        other => {
            panic!("expected source-coverage or verified no-progress rejection, got {other:?}")
        }
    }
    assert_eq!(trace.clean_audits_after_repair, 0);
}

#[tokio::test]
async fn sequential_repairs_preserve_the_earlier_fix_and_nonfocus_content() {
    let corpus = corpus();
    let case = corpus
        .cases
        .iter()
        .find(|case| case.id == "qualified_interview_disagreement")
        .unwrap();
    let mut executor = AnnotatedExecutor::new(case, Some("causal_hypothesis"), false);
    executor.second_omit = Some("corrected_denominator");
    executor.group_chapter = true;
    let mut allowance = policy();
    allowance.window_duration_ms = case.duration_ms;
    allowance.allowances.max_repair_rounds = 3;
    let output = compose_detailed(
        &executor,
        &executor.source,
        executor.images.clone(),
        GenerationIntent {
            depth: Depth::Detailed,
            focus: Some("Emphasize the temperature measurements".into()),
        },
        None,
        &allowance,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    output.validate().unwrap();
    assert!(coverage(case, &output, Depth::Detailed).is_complete());
    let trace = executor.trace.lock().unwrap();
    assert!(trace.repairs >= 2);
    assert_eq!(trace.findings, 2);
    assert!(trace.clean_audits_after_repair >= 2);
    drop(trace);
}

#[tokio::test]
async fn composer_routes_standard_selectively_and_detailed_comprehensively() {
    let corpus = corpus();
    let case = corpus
        .cases
        .iter()
        .find(|case| case.id == "visual_only")
        .unwrap();
    let executor = AnnotatedExecutor::new(case, None, false);
    let composer = Composer {
        executor: &executor,
        limits: CompositionLimits::default(),
    };
    let standard = composer
        .compose(
            &executor.source,
            executor.images.clone(),
            GenerationIntent {
                depth: Depth::Standard,
                ..GenerationIntent::default()
            },
            None,
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert!(coverage(case, &standard, Depth::Standard).is_complete());
    assert_eq!(
        coverage(case, &standard, Depth::Detailed).missing,
        ["initial_state"]
    );
    assert!(executor.trace.lock().unwrap().source_seen.is_empty());
    let detailed = composer
        .compose_with_policy(
            &executor.source,
            executor.images.clone(),
            GenerationIntent {
                depth: Depth::Detailed,
                ..GenerationIntent::default()
            },
            None,
            &policy(),
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert!(coverage(case, &detailed, Depth::Detailed).is_complete());
    assert_eq!(executor.trace.lock().unwrap().standard_calls, 1);
    assert!(
        executor
            .trace
            .lock()
            .unwrap()
            .source_seen
            .contains_key("visual")
    );
}
