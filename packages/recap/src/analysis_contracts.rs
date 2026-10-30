//! Portable staged analysis and source-coverage contracts for Detailed recaps.
//!
//! These are operation-local contracts, not a new document format. Source text and
//! model output are untrusted data. Host-owned evidence and assets remain authoritative.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{
    contracts::{Section, TimeRange},
    model::{ImageAsset, ModelError},
};

/// Independently structured model operation; unsupported stages must not fall back to Standard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisStage {
    /// Organize accepted source passages without rewriting their content.
    EditionPlan,
    /// Plan source-grounded supporting illustrations.
    VisualPlan,
    /// Inspect an external candidate for relevance and truthful captioning.
    VisualReview,
    /// Resolve participants from explicit source attestations.
    Participants,
    /// Identify substantive source information within one bounded window.
    ExtractionInventory,
    /// Organize inventory and explicitly account for every item.
    Outline,
    /// Render one planned chapter using existing document blocks.
    ChapterComposition,
    /// Compare the composed document against source inventory.
    SourceAudit,
    /// Propose replacement chapters for explicit audit findings.
    SourceRepair,
    /// Inspect actual source frames for information unavailable in the transcript.
    VisualInspection,
    /// Compose a readable passage directly from one immutable source unit.
    PassageComposition,
    /// Compare one passage and its source in both directions.
    PassageReview,
    /// Propose revision-checked targeted edits to one passage.
    PassagePatch,
}

/// Provider-neutral stage request. Adapters must put instructions in a trusted
/// instruction channel and input/images in a separate untrusted source channel.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AnalysisRequest {
    /// Selects exactly one result schema.
    pub stage: AnalysisStage,
    /// Untrusted stage input; never interpolate this into trusted instructions.
    pub input: serde_json::Value,
    /// Trusted stage and editorial policy.
    pub instructions: String,
    /// Actual source imagery, with host-owned asset identities.
    pub images: Vec<ImageAsset>,
    /// Hard bound applied before response JSON decoding.
    pub max_response_bytes: usize,
}

/// Per-request constraints and optional operation allowances, not editorial caps.
/// Exhaustion must fail or report incomplete coverage, never silently truncate it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisAllowances {
    /// Optional maximum model invocations; zero disables the operation-wide ceiling.
    pub max_model_calls: usize,
    /// Maximum serialized input bytes per invocation.
    pub max_input_bytes: usize,
    /// Maximum serialized response bytes per invocation.
    pub max_response_bytes: usize,
    /// Maximum encoded image bytes across one invocation.
    pub max_image_bytes: usize,
    /// Optional maximum source windows (zero disables the ceiling) accepted by this operation.
    pub max_source_windows: usize,
    /// Maximum explicit audit/repair iterations, and additional correction attempts
    /// per semantically invalid completed passage patch. Does not retry provider errors.
    pub max_repair_rounds: usize,
}

impl Default for AnalysisAllowances {
    fn default() -> Self {
        Self {
            max_model_calls: 0,
            max_input_bytes: 256 * 1024,
            max_response_bytes: 256 * 1024,
            max_image_bytes: 16 * 1024 * 1024,
            max_source_windows: 0,
            max_repair_rounds: 2,
        }
    }
}

/// Portable Detailed policy. Detailed retains substantive information; Standard
/// remains a separate selective path. There is deliberately no chapter-count cap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetailedPolicy {
    /// Select source-preserving passages (the normal Detailed path).
    /// False retains the legacy library pipeline for existing integrators.
    #[serde(default = "default_source_passages")]
    pub source_passages: bool,
    /// Resource budget independently configurable by standalone or hosted callers.
    pub allowances: AnalysisAllowances,
    /// Maximum source duration per extraction window, in milliseconds.
    pub window_duration_ms: u64,
    /// Adjacent-window overlap, strictly less than the window duration.
    pub window_overlap_ms: u64,
    /// Whether to inspect source frames in addition to textual evidence.
    pub inspect_visuals: bool,
    /// Maximum independent model requests executing concurrently.
    #[serde(default = "default_concurrency")]
    pub max_concurrent_requests: usize,
    /// Maximum aggregate reserved request, image and response bytes in flight.
    #[serde(default = "default_in_flight_bytes")]
    pub max_in_flight_bytes: usize,
}

const fn default_concurrency() -> usize {
    3
}

const fn default_source_passages() -> bool {
    true
}

const fn default_in_flight_bytes() -> usize {
    24 * 1024 * 1024
}

impl Default for DetailedPolicy {
    fn default() -> Self {
        Self {
            source_passages: true,
            allowances: AnalysisAllowances::default(),
            window_duration_ms: 180_000,
            window_overlap_ms: 10_000,
            inspect_visuals: true,
            max_concurrent_requests: default_concurrency(),
            max_in_flight_bytes: default_in_flight_bytes(),
        }
    }
}

impl DetailedPolicy {
    /// Check executable policy bounds without imposing an editorial cap.
    ///
    /// # Errors
    /// Returns [`ModelError::InvalidResponse`] for zero essential allowances or invalid windows.
    pub const fn validate(&self) -> Result<(), ModelError> {
        let limits = &self.allowances;
        if self.max_concurrent_requests == 0
            || self.max_in_flight_bytes == 0
            || self.window_duration_ms == 0
            || self.window_overlap_ms >= self.window_duration_ms
            || limits.max_input_bytes == 0
            || limits.max_response_bytes == 0
            || (self.inspect_visuals && limits.max_image_bytes == 0)
        {
            return Err(ModelError::InvalidResponse);
        }
        Ok(())
    }
}

/// Host-owned, bounded view of source evidence, not model-authored provenance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceWindow {
    /// Stable window identity.
    pub id: String,
    /// Source interval covered by this window.
    pub range: TimeRange,
    /// Existing transcript segment identities.
    pub transcript_ids: Vec<String>,
    /// Existing evidence identities.
    pub evidence_ids: Vec<String>,
    /// Actual source-frame asset identities.
    pub asset_ids: Vec<String>,
}

/// Substantive information category, not a new rendered block type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InformationKind {
    /// A source assertion or explanation.
    Fact,
    /// A procedure, demonstration, or ordered instruction.
    Procedure,
    /// A substantive illustrative example.
    Example,
    /// A limitation, exception, or uncertainty.
    Caveat,
    /// A concrete conclusion or recommendation.
    Conclusion,
    /// Information read from actual source imagery.
    Visual,
}

/// Atomic substantive information to preserve or explicitly dispose of.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InformationItem {
    /// Stable identity, namespaced by the host across windows.
    pub id: String,
    /// Originating source window.
    pub window_id: String,
    /// Source interval supporting the item.
    pub range: TimeRange,
    /// Kind of substantive information.
    pub kind: InformationKind,
    /// Plain text retaining source specifics, qualifications, and quantities.
    pub text: String,
    /// Host-owned evidence references supporting this item.
    pub evidence_ids: Vec<String>,
}

/// Inventory from one source window. Empty inventories must explain their outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractionInventory {
    /// Window inspected.
    pub window_id: String,
    /// Substantive information discovered.
    pub items: Vec<InformationItem>,
    /// Explicit gaps or reasons no substantive information could be extracted.
    pub limitations: Vec<String>,
}

/// Allowed disposition; substantive information cannot be omitted merely for brevity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispositionKind {
    /// Must appear in one or more planned chapters.
    Retained,
    /// Same information is retained under another item identity.
    Duplicate,
    /// Non-substantive source material, such as housekeeping or filler.
    NonSubstantive,
    /// Evidence is insufficient; the coverage report must retain the limitation.
    Unsupported,
}

/// Exactly one outline decision per inventory item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemDisposition {
    /// Inventory identity being accounted for.
    pub item_id: String,
    /// Explicit retention or exclusion decision.
    pub disposition: DispositionKind,
    /// Target chapter identities for retained items; empty for exclusions.
    pub chapter_ids: Vec<String>,
    /// Retained item identities for duplicates; empty otherwise.
    pub duplicate_of: Vec<String>,
    /// Nonempty source-grounded rationale, never merely a length preference.
    pub reason: String,
}

/// One editorial chapter and its assigned source information.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChapterPlan {
    /// Stable section identity used by composition.
    pub id: String,
    /// Plain-text chapter heading.
    pub title: String,
    /// Inventory items this chapter must preserve.
    pub item_ids: Vec<String>,
}

/// Uncapped editorial outline with complete item accounting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outline {
    /// Plain-text recap title.
    pub title: String,
    /// Ordered chapters; resource exhaustion is not an editorial cap.
    pub chapters: Vec<ChapterPlan>,
    /// One decision per input inventory item.
    pub dispositions: Vec<ItemDisposition>,
}

/// Existing renderable section plus explicit information coverage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChapterComposition {
    /// Composed chapter using existing block variants only.
    pub section: Section,
    /// Inventory items actually represented, not merely planned.
    pub covered_item_ids: Vec<String>,
    /// Grounded visual choices using the unchanged Standard response contract.
    pub image_choices: Vec<VisualChoice>,
}

/// A selected source asset and its reader-facing purpose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisualChoice {
    /// Existing asset identity.
    pub asset_id: String,
    /// Why the source image helps understand the chapter.
    pub reason: String,
}

/// Kind of source-grounding or information-loss failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageIssueKind {
    /// Substantive inventory information is absent.
    Missing,
    /// The document contradicts source evidence.
    Contradiction,
    /// Document claims lack supporting evidence.
    UnsupportedClaim,
    /// A visual is unreadable or not relevant to the explanation.
    InadequateVisual,
}

/// An actionable finding, never a provider-private diagnostic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageIssue {
    /// Stable issue identity for targeted repair.
    pub id: String,
    /// Type of grounding failure.
    pub kind: CoverageIssueKind,
    /// Relevant inventory identities; empty only for unsupported added claims.
    pub item_ids: Vec<String>,
    /// Existing chapters implicated; empty for wholly missing material.
    pub chapter_ids: Vec<String>,
    /// Source-grounded description and required correction.
    pub description: String,
}

/// Source comparison result. An empty issue list is not independently proof of coverage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceAudit {
    /// Items actually checked against source evidence.
    pub checked_item_ids: Vec<String>,
    /// Detected source-information or grounding problems.
    pub issues: Vec<CoverageIssue>,
    /// Evidence gaps preventing a conclusive audit.
    pub limitations: Vec<String>,
}

/// Proposed repairs; hosts validate and re-audit before accepting resolved claims.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRepair {
    /// Findings the proposal claims to address.
    pub addressed_issue_ids: Vec<String>,
    /// Full replacement chapters, not an implicit document mutation.
    pub chapters: Vec<ChapterComposition>,
    /// Findings not resolved by this proposal.
    pub unresolved_issue_ids: Vec<String>,
}

/// Information obtained by inspecting real source frames.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisualInspection {
    /// Source window inspected.
    pub window_id: String,
    /// Actual inspected asset identities.
    pub inspected_asset_ids: Vec<String>,
    /// Source-grounded visual information, including readable labels or code.
    pub items: Vec<InformationItem>,
    /// Useful source assets and selection rationale.
    pub visual_choices: Vec<VisualChoice>,
    /// Unreadable, missing, or ambiguous visual evidence.
    pub limitations: Vec<String>,
}

/// Host-produced operation coverage, separate from persisted document schema v1.
/// This is not a model assertion that an operation succeeded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageReport {
    /// Complete host inventory identities.
    pub inventory_item_ids: Vec<String>,
    /// Validated outline decisions.
    pub dispositions: Vec<ItemDisposition>,
    /// Items verified present after composition and source audit.
    pub covered_item_ids: Vec<String>,
    /// Remaining source-grounding failures.
    pub unresolved_issues: Vec<CoverageIssue>,
    /// Extraction, visual, audit, or resource limitations; never silently discarded.
    pub limitations: Vec<String>,
}

impl CoverageReport {
    /// Whether every inventory item is retained or explicitly justified, with no
    /// unresolved issues or limitations. Hosts must first validate source references
    /// and actual chapter content; model-declared item IDs alone are not evidence.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        let inventory: BTreeSet<_> = self.inventory_item_ids.iter().collect();
        let covered: BTreeSet<_> = self.covered_item_ids.iter().collect();
        let decisions: BTreeSet<_> = self.dispositions.iter().map(|item| &item.item_id).collect();
        let retained: BTreeSet<_> = self
            .dispositions
            .iter()
            .filter(|item| item.disposition == DispositionKind::Retained)
            .map(|item| &item.item_id)
            .collect();
        !inventory.is_empty()
            && inventory.len() == self.inventory_item_ids.len()
            && covered.len() == self.covered_item_ids.len()
            && covered == retained
            && decisions == inventory
            && decisions.len() == self.dispositions.len()
            && self.unresolved_issues.is_empty()
            && self.limitations.is_empty()
            && self.dispositions.iter().all(|item| {
                !item.reason.trim().is_empty()
                    && match item.disposition {
                        DispositionKind::Retained => {
                            covered.contains(&item.item_id)
                                && !item.chapter_ids.is_empty()
                                && item.duplicate_of.is_empty()
                        }
                        DispositionKind::Duplicate => {
                            item.chapter_ids.is_empty()
                                && !item.duplicate_of.is_empty()
                                && item
                                    .duplicate_of
                                    .iter()
                                    .all(|id| id != &item.item_id && retained.contains(id))
                        }
                        DispositionKind::NonSubstantive => {
                            item.chapter_ids.is_empty() && item.duplicate_of.is_empty()
                        }
                        DispositionKind::Unsupported => false,
                    }
            })
    }
}

/// Stage-specific typed result. Wire responses are the inner object only, selected
/// by the request stage; adapters must never guess the stage from returned content.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AnalysisResponse {
    /// Validated contiguous editorial grouping.
    EditionPlan(crate::editorial::EditionPlan),
    /// Supporting illustration opportunities.
    VisualPlan(crate::visual_context::VisualPlan),
    /// Multimodal candidate assessment.
    VisualReview(crate::visual_context::VisualReview),
    /// Source-attested participant candidates.
    Participants(crate::source_context::Participants),
    /// Window information inventory.
    ExtractionInventory(ExtractionInventory),
    /// Item-accounted chapter plan.
    Outline(Outline),
    /// One composed chapter.
    ChapterComposition(ChapterComposition),
    /// Source coverage audit.
    SourceAudit(SourceAudit),
    /// Targeted replacement proposal.
    SourceRepair(SourceRepair),
    /// Actual-frame inspection.
    VisualInspection(VisualInspection),
    /// Direct-source readable passage.
    PassageComposition(crate::passage_contracts::PassageComposition),
    /// Local two-way source review.
    PassageReview(crate::passage_contracts::PassageReview),
    /// Revision-checked targeted edit proposal.
    PassagePatch(crate::passage_contracts::PassagePatch),
}

impl AnalysisResponse {
    /// Decode precisely one bounded, strict stage response, without fence stripping
    /// or fallback. Source identities and coverage require subsequent host validation.
    ///
    /// # Errors
    /// Returns [`ModelError::InvalidResponse`] for oversized or malformed data,
    /// unknown fields/variants, or a response for a different stage.
    pub fn from_json_slice(
        stage: AnalysisStage,
        bytes: &[u8],
        max_bytes: usize,
    ) -> Result<Self, ModelError> {
        if bytes.len() > max_bytes {
            return Err(ModelError::InvalidResponse);
        }
        match stage {
            AnalysisStage::EditionPlan => serde_json::from_slice(bytes).map(Self::EditionPlan),
            AnalysisStage::VisualPlan => serde_json::from_slice(bytes).map(Self::VisualPlan),
            AnalysisStage::VisualReview => serde_json::from_slice(bytes).map(Self::VisualReview),
            AnalysisStage::Participants => serde_json::from_slice(bytes).map(Self::Participants),
            AnalysisStage::ExtractionInventory => {
                serde_json::from_slice(bytes).map(Self::ExtractionInventory)
            }
            AnalysisStage::Outline => serde_json::from_slice(bytes).map(Self::Outline),
            AnalysisStage::ChapterComposition => {
                serde_json::from_slice(bytes).map(Self::ChapterComposition)
            }
            AnalysisStage::SourceAudit => serde_json::from_slice(bytes).map(Self::SourceAudit),
            AnalysisStage::SourceRepair => serde_json::from_slice(bytes).map(Self::SourceRepair),
            AnalysisStage::VisualInspection => {
                serde_json::from_slice(bytes).map(Self::VisualInspection)
            }
            AnalysisStage::PassageComposition => {
                serde_json::from_slice(bytes).map(Self::PassageComposition)
            }
            AnalysisStage::PassageReview => serde_json::from_slice(bytes).map(Self::PassageReview),
            AnalysisStage::PassagePatch => serde_json::from_slice(bytes).map(Self::PassagePatch),
        }
        .map_err(|_| ModelError::InvalidResponse)
    }

    /// Stage matching this typed result.
    #[must_use]
    pub const fn stage(&self) -> AnalysisStage {
        match self {
            Self::EditionPlan(_) => AnalysisStage::EditionPlan,
            Self::VisualPlan(_) => AnalysisStage::VisualPlan,
            Self::VisualReview(_) => AnalysisStage::VisualReview,
            Self::Participants(_) => AnalysisStage::Participants,
            Self::ExtractionInventory(_) => AnalysisStage::ExtractionInventory,
            Self::Outline(_) => AnalysisStage::Outline,
            Self::ChapterComposition(_) => AnalysisStage::ChapterComposition,
            Self::SourceAudit(_) => AnalysisStage::SourceAudit,
            Self::SourceRepair(_) => AnalysisStage::SourceRepair,
            Self::VisualInspection(_) => AnalysisStage::VisualInspection,
            Self::PassageComposition(_) => AnalysisStage::PassageComposition,
            Self::PassageReview(_) => AnalysisStage::PassageReview,
            Self::PassagePatch(_) => AnalysisStage::PassagePatch,
        }
    }
}

// Keep the existing section schema authoritative instead of duplicating block shapes.
/// Strict provider-portable response schema for one analysis stage. All properties
/// are required, objects are closed, and the root is always an object.
#[must_use]
pub fn analysis_response_schema(stage: AnalysisStage) -> serde_json::Value {
    use serde_json::json;
    let text = json!({"type":"string"});
    let strings = array(text.clone());
    let range = object(json!({"start_ms":{"type":"integer"},"end_ms":{"type":"integer"}}));
    let item = object(json!({
        "id":text,"window_id":text,"range":range,
        "kind":{"type":"string","enum":["fact","procedure","example","caveat","conclusion","visual"]},
        "text":text,"evidence_ids":strings
    }));
    let choice = object(json!({"asset_id":text,"reason":text}));
    let section = crate::model::response_schema()["properties"]["sections"]["items"].clone();
    let chapter = object(
        json!({"section":section,"covered_item_ids":strings,"image_choices":array(choice.clone())}),
    );
    let issue = object(json!({
        "id":text,"kind":{"type":"string","enum":["missing","contradiction","unsupported_claim","inadequate_visual"]},
        "item_ids":strings,"chapter_ids":strings,"description":text
    }));
    match stage {
        AnalysisStage::EditionPlan => {
            object(json!({"chapters":array(object(json!({"title":text,"section_ids":strings})))}))
        }
        AnalysisStage::VisualPlan => object(
            json!({"opportunities":array(object(json!({"section_id":text,"subject":text,"purpose":text})))}),
        ),
        AnalysisStage::VisualReview => object(json!({"useful":{"type":"boolean"},"caption":text})),
        AnalysisStage::Participants => object(
            json!({"participants":array(object(json!({"name":text,"role":text,"evidence":text})))}),
        ),
        AnalysisStage::PassageComposition => crate::passage_contracts::composition_schema(),
        AnalysisStage::PassageReview => crate::passage_contracts::review_schema(),
        AnalysisStage::PassagePatch => crate::passage_contracts::patch_schema(),
        AnalysisStage::ExtractionInventory => {
            object(json!({"window_id":text,"items":array(item),"limitations":strings}))
        }
        AnalysisStage::Outline => object(json!({
            "title":text,
            "chapters":array(object(json!({"id":text,"title":text,"item_ids":strings}))),
            "dispositions":array(object(json!({
                "item_id":text,
                "disposition":{"type":"string","enum":["retained","duplicate","non_substantive","unsupported"]},
                "chapter_ids":strings,"duplicate_of":strings,"reason":text
            })))
        })),
        AnalysisStage::ChapterComposition => chapter,
        AnalysisStage::SourceAudit => {
            object(json!({"checked_item_ids":strings,"issues":array(issue),"limitations":strings}))
        }
        AnalysisStage::SourceRepair => object(
            json!({"addressed_issue_ids":strings,"chapters":array(chapter),"unresolved_issue_ids":strings}),
        ),
        AnalysisStage::VisualInspection => object(
            json!({"window_id":text,"inspected_asset_ids":strings,"items":array(item),"visual_choices":array(choice),"limitations":strings}),
        ),
    }
}

fn object(properties: serde_json::Value) -> serde_json::Value {
    let required: Vec<_> = properties
        .as_object()
        .into_iter()
        .flat_map(|fields| fields.keys())
        .cloned()
        .collect();
    serde_json::Value::Object(serde_json::Map::from_iter([
        ("type".into(), serde_json::Value::String("object".into())),
        ("properties".into(), properties),
        ("required".into(), serde_json::json!(required)),
        (
            "additionalProperties".into(),
            serde_json::Value::Bool(false),
        ),
    ]))
}

fn array(items: serde_json::Value) -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::from_iter([
        ("type".into(), serde_json::Value::String("array".into())),
        ("items".into(), items),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn assert_strict(value: &Value) {
        match value {
            Value::Object(fields) => {
                if fields.get("type").is_some_and(|kind| kind == "object") {
                    assert_eq!(fields["additionalProperties"], false);
                    let properties = fields["properties"].as_object().unwrap();
                    let required = fields["required"].as_array().unwrap();
                    assert_eq!(properties.len(), required.len());
                    assert!(properties.keys().all(|key| required.contains(&json!(key))));
                }
                for child in fields.values() {
                    assert_strict(child);
                }
            }
            Value::Array(values) => {
                for child in values {
                    assert_strict(child);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn every_stage_has_closed_portable_objects() {
        for stage in [
            AnalysisStage::ExtractionInventory,
            AnalysisStage::Outline,
            AnalysisStage::ChapterComposition,
            AnalysisStage::SourceAudit,
            AnalysisStage::SourceRepair,
            AnalysisStage::VisualInspection,
            AnalysisStage::PassageComposition,
            AnalysisStage::PassageReview,
            AnalysisStage::PassagePatch,
        ] {
            let schema = analysis_response_schema(stage);
            assert_eq!(schema["type"], "object");
            assert_strict(&schema);
        }
    }

    #[test]
    fn decoding_is_bounded_and_never_guesses_a_stage() {
        let bytes = br#"{"window_id":"window-1","items":[],"limitations":["no transcript"]}"#;
        let response = AnalysisResponse::from_json_slice(
            AnalysisStage::ExtractionInventory,
            bytes,
            bytes.len(),
        )
        .unwrap();
        assert_eq!(response.stage(), AnalysisStage::ExtractionInventory);
        assert!(
            AnalysisResponse::from_json_slice(
                AnalysisStage::ExtractionInventory,
                bytes,
                bytes.len() - 1,
            )
            .is_err()
        );
        assert!(
            AnalysisResponse::from_json_slice(AnalysisStage::SourceAudit, bytes, bytes.len(),)
                .is_err()
        );
        let mut unknown: Value = serde_json::from_slice(bytes).unwrap();
        unknown["extra"] = json!(true);
        let unknown = serde_json::to_vec(&unknown).unwrap();
        assert!(
            AnalysisResponse::from_json_slice(
                AnalysisStage::ExtractionInventory,
                &unknown,
                unknown.len(),
            )
            .is_err()
        );
        // Even fields serde would otherwise default (e.g. Option) must remain required.
        assert!(
            AnalysisResponse::from_json_slice(
                AnalysisStage::Outline,
                br#"{"title":"Missing decisions","chapters":[]}"#,
                1024,
            )
            .is_err()
        );
    }

    #[test]
    fn all_stage_payloads_decode_without_a_provider_envelope() {
        let payloads = [
            (
                AnalysisStage::Outline,
                json!({"title":"Source","chapters":[],"dispositions":[]}),
            ),
            (
                AnalysisStage::ChapterComposition,
                json!({"section":{"id":"chapter-1","title":"Chapter","blocks":[]},"covered_item_ids":[],"image_choices":[]}),
            ),
            (
                AnalysisStage::SourceAudit,
                json!({"checked_item_ids":[],"issues":[],"limitations":[]}),
            ),
            (
                AnalysisStage::SourceRepair,
                json!({"addressed_issue_ids":[],"chapters":[],"unresolved_issue_ids":[]}),
            ),
            (
                AnalysisStage::VisualInspection,
                json!({"window_id":"window-1","inspected_asset_ids":[],"items":[],"visual_choices":[],"limitations":[]}),
            ),
        ];
        for (stage, value) in payloads {
            let bytes = serde_json::to_vec(&value).unwrap();
            assert_eq!(
                AnalysisResponse::from_json_slice(stage, &bytes, bytes.len())
                    .unwrap()
                    .stage(),
                stage
            );
        }
    }

    #[test]
    fn coverage_rejects_missing_decisions_false_duplicates_and_limits() {
        let mut report = CoverageReport {
            inventory_item_ids: vec!["a".into(), "b".into()],
            dispositions: vec![ItemDisposition {
                item_id: "a".into(),
                disposition: DispositionKind::Retained,
                chapter_ids: vec!["chapter-1".into()],
                duplicate_of: vec![],
                reason: "Specific source explanation".into(),
            }],
            covered_item_ids: vec!["a".into()],
            unresolved_issues: vec![],
            limitations: vec![],
        };
        assert!(!report.is_complete());
        report.dispositions.push(ItemDisposition {
            item_id: "b".into(),
            disposition: DispositionKind::Duplicate,
            chapter_ids: vec![],
            duplicate_of: vec!["a".into()],
            reason: "Same source explanation repeated".into(),
        });
        assert!(report.is_complete());
        report.dispositions[1].duplicate_of = vec!["b".into()];
        assert!(!report.is_complete());
        report.dispositions[1].duplicate_of = vec!["a".into()];
        report
            .limitations
            .push("Model-call budget exhausted".into());
        assert!(!report.is_complete());
        report.limitations.clear();
        report.dispositions[1].disposition = DispositionKind::Unsupported;
        assert!(!report.is_complete());
        report.dispositions[1] = report.dispositions[0].clone();
        assert!(!report.is_complete());
    }

    #[test]
    fn source_passages_is_default_for_new_and_old_policy_inputs() {
        let policy = DetailedPolicy::default();
        assert!(policy.source_passages);
        let mut value = serde_json::to_value(&policy).unwrap();
        value.as_object_mut().unwrap().remove("source_passages");
        let old_policy: DetailedPolicy = serde_json::from_value(value.clone()).unwrap();
        assert!(old_policy.source_passages);
        value["source_passages"] = json!(true);
        let experimental: DetailedPolicy = serde_json::from_value(value.clone()).unwrap();
        assert!(experimental.source_passages);
        assert!(experimental.validate().is_ok());
        value["source_passages"] = json!("true");
        assert!(serde_json::from_value::<DetailedPolicy>(value).is_err());
    }

    #[test]
    fn policy_defaults_old_serialized_inputs_and_rejects_zero_scheduling_limits() {
        let mut value = serde_json::to_value(DetailedPolicy::default()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("max_concurrent_requests");
        value.as_object_mut().unwrap().remove("max_in_flight_bytes");
        let mut policy: DetailedPolicy = serde_json::from_value(value).unwrap();
        assert_eq!(policy.max_concurrent_requests, 3);
        assert_eq!(policy.max_in_flight_bytes, 24 * 1024 * 1024);
        policy.max_concurrent_requests = 0;
        assert!(policy.validate().is_err());
        policy.max_concurrent_requests = 1;
        policy.max_in_flight_bytes = 0;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn operation_ceilings_default_to_unlimited_but_persist_explicit_allowances() {
        let mut policy = DetailedPolicy::default();
        assert_eq!(policy.allowances.max_model_calls, 0);
        assert_eq!(policy.allowances.max_source_windows, 0);
        policy.allowances.max_model_calls = 128;
        policy.allowances.max_source_windows = 128;
        let restored: DetailedPolicy =
            serde_json::from_value(serde_json::to_value(&policy).unwrap()).unwrap();
        assert_eq!(restored, policy);
        restored.validate().unwrap();
    }

    #[test]
    fn policy_separates_resources_from_editorial_structure() {
        let mut policy = DetailedPolicy::default();
        assert!(policy.validate().is_ok());
        policy.window_overlap_ms = policy.window_duration_ms;
        assert!(policy.validate().is_err());
        policy.window_overlap_ms = 0;
        policy.allowances.max_model_calls = 0;
        assert!(policy.validate().is_ok());
    }
}
