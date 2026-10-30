//! Deterministic source annotations and a narrow exact-phrase coverage oracle.
//!
//! Reuse from another integration test with
//! `#[path = "fixtures/detailed/mod.rs"] mod detailed;`.
//! These helpers are test data, not a production quality scorer or model adapter.

use std::collections::BTreeSet;

use bcode_recap::contracts::{
    Asset, AssetPath, Block, BlockContent, DOCUMENT_SCHEMA_VERSION, Depth, Evidence,
    GenerationIntent, RecapDocument, Section, Source, TimeRange, TranscriptSegment,
};
use serde::Deserialize;

/// Versioned, bounded, synthetic source corpus. No network access is involved.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Corpus {
    /// Annotation format, independent of the document schema.
    pub schema_version: u32,
    /// Independent source scenarios.
    pub cases: Vec<Case>,
}

/// One annotated video timeline, not an acquired video.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// Stable scenario identity.
    pub id: String,
    /// Source title.
    pub title: String,
    /// Full source duration, including gaps between observations.
    pub duration_ms: u64,
    /// Transcript or visual observations, ordered by source time.
    pub observations: Vec<Observation>,
    /// Distinct substantive facts; repetitions share one expected item.
    pub items: Vec<Item>,
    /// Known false assertions, independently annotated from this source.
    /// Exact negative controls only; absence does not prove entailment.
    #[serde(default)]
    pub forbidden_claims: Vec<String>,
}

/// One source interval. Visual descriptions are annotations, never spoken words.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    /// Also used as its evidence ID.
    pub id: String,
    /// Inclusive timestamp.
    pub start_ms: u64,
    /// Exclusive timestamp.
    pub end_ms: u64,
    /// Exact synthetic spoken text, if present.
    pub transcript: Option<String>,
    /// Human-authored visible-state annotation, if present.
    pub visual: Option<Visual>,
}

/// Expected visual state; actual image bytes must come from the test media path.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Visual {
    /// Registry identity reserved for the real extracted frame.
    pub asset_id: String,
    /// Visible information and observation limits; not a transcript.
    pub description: String,
}

/// One expected substantive unit, not necessarily one chapter or paragraph.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    /// Stable annotation identity.
    pub id: String,
    /// Canonical deterministic wording. Never inject this answer key into model input.
    pub summary: String,
    /// All phrases must occur in a single content block for this narrow oracle.
    pub required_fragments: Vec<String>,
    /// Alternative source evidence IDs: at least one must support each matching block.
    pub evidence_ids: Vec<String>,
    /// A selective Standard response must preserve this item for this test scenario.
    pub standard_required: bool,
    /// Optional visible-state asset required alongside the factual description.
    pub visual_asset_id: Option<String>,
    /// Tutorial/visual predecessor, checked only when both items are present.
    pub after: Option<String>,
}

/// Load and check the committed corpus. Malformed test data is a test failure.
#[must_use]
pub fn corpus() -> Corpus {
    let mut corpus: Corpus = serde_json::from_str(include_str!("corpus.json")).unwrap();
    corpus
        .cases
        .push(serde_json::from_str(include_str!("interview.json")).unwrap());
    assert_eq!(corpus.schema_version, 1, "unsupported annotation version");
    let mut case_ids = BTreeSet::new();
    for case in &corpus.cases {
        assert!(case_ids.insert(&case.id), "duplicate case ID");
        case.validate_annotations();
    }
    corpus
}

impl Observation {
    const fn range(&self) -> TimeRange {
        TimeRange {
            start_ms: self.start_ms,
            end_ms: self.end_ms,
        }
    }
}

impl Item {
    /// Detailed requires every substantive unit; Standard is intentionally selective.
    #[must_use]
    pub const fn required(&self, depth: Depth) -> bool {
        match depth {
            Depth::Detailed => true,
            Depth::Standard | Depth::Brief => self.standard_required,
        }
    }
}

impl Case {
    fn validate_annotations(&self) {
        assert!(!self.observations.is_empty());
        assert!(!self.items.is_empty());
        let mut observations = BTreeSet::new();
        let mut assets = BTreeSet::new();
        let mut previous_start = 0;
        for observation in &self.observations {
            assert!(observations.insert(observation.id.as_str()));
            observation.range().validate(self.duration_ms).unwrap();
            assert!(observation.start_ms >= previous_start);
            previous_start = observation.start_ms;
            assert_ne!(
                observation.transcript.is_some(),
                observation.visual.is_some()
            );
            if let Some(text) = &observation.transcript {
                assert!(!text.trim().is_empty());
            }
            if let Some(visual) = &observation.visual {
                assert!(assets.insert(visual.asset_id.as_str()));
                assert!(!visual.description.trim().is_empty());
            }
        }
        let mut items = BTreeSet::new();
        for item in &self.items {
            assert!(items.insert(item.id.as_str()));
            assert!(!item.required_fragments.is_empty());
            assert!(item.required_fragments.iter().all(|fragment| {
                !fragment.trim().is_empty()
                    && item
                        .summary
                        .to_lowercase()
                        .contains(&fragment.to_lowercase())
            }));
            assert!(!item.evidence_ids.is_empty());
            assert!(
                item.evidence_ids
                    .iter()
                    .all(|id| observations.contains(id.as_str()))
            );
            if let Some(asset_id) = &item.visual_asset_id {
                assert!(assets.contains(asset_id.as_str()));
                assert!(self.observations.iter().any(|observation| {
                    item.evidence_ids.contains(&observation.id)
                        && observation
                            .visual
                            .as_ref()
                            .is_some_and(|visual| visual.asset_id == *asset_id)
                }));
            }
            if let Some(predecessor) = &item.after {
                // Earlier declarations make cycles and dangling predecessors impossible.
                assert_ne!(predecessor, &item.id);
                assert!(items.contains(predecessor.as_str()));
            }
        }
        assert!(self.forbidden_claims.iter().all(|claim| {
            !claim.trim().is_empty()
                && self
                    .items
                    .iter()
                    .all(|item| !normalized(&item.summary).contains(&normalized(claim)))
        }));
        assert!(self.items.iter().any(|item| item.standard_required));
        assert!(self.items.iter().any(|item| !item.standard_required));
        self.grounded(Depth::Detailed).validate().unwrap();
    }

    /// Construct current portable source contracts without leaking the answer key.
    /// Visual descriptions deliberately remain outside the transcript and document text.
    /// Asset paths are reservations, not evidence of on-disk media or extraction.
    #[must_use]
    pub fn grounded(&self, depth: Depth) -> RecapDocument {
        RecapDocument {
            schema_version: DOCUMENT_SCHEMA_VERSION,
            id: self.id.clone(),
            revision: 1,
            title: self.title.clone(),
            source: Source {
                context: bcode_recap::source_context::SourceContext::default(),
                id: format!("source_{}", self.id),
                title: self.title.clone(),
                url: None,
                duration_ms: self.duration_ms,
            },
            intent: GenerationIntent {
                depth,
                ..GenerationIntent::default()
            },
            transcript: self
                .observations
                .iter()
                .filter_map(|observation| {
                    observation
                        .transcript
                        .as_ref()
                        .map(|text| TranscriptSegment {
                            id: observation.id.clone(),
                            range: observation.range(),
                            text: text.clone(),
                        })
                })
                .collect(),
            evidence: self
                .observations
                .iter()
                .map(|observation| Evidence {
                    id: observation.id.clone(),
                    range: observation.range(),
                    transcript_segment_ids: if observation.transcript.is_some() {
                        vec![observation.id.clone()]
                    } else {
                        Vec::new()
                    },
                })
                .collect(),
            assets: self
                .observations
                .iter()
                .filter_map(|observation| {
                    observation.visual.as_ref().map(|visual| Asset {
                        illustration: None,
                        id: visual.asset_id.clone(),
                        path: AssetPath::new(format!("{}.png", visual.asset_id)).unwrap(),
                        mime_type: "image/png".into(),
                        range: observation.range(),
                    })
                })
                .collect(),
            sections: Vec::new(),
        }
    }

    /// Canonical precomposed answer for oracle/contract tests only. This is NOT
    /// evidence that a real pipeline or model generated a sufficiently detailed recap.
    /// One section per item deliberately exercises documents with many sections;
    /// production composition may group any number of items in a section.
    #[must_use]
    pub fn expected_document(&self, depth: Depth) -> RecapDocument {
        let mut document = self.grounded(depth);
        document.sections = self
            .items
            .iter()
            .filter(|item| item.required(depth))
            .map(|item| Section {
                id: format!("section_{}", item.id),
                title: item.id.replace('_', " "),
                blocks: vec![Block {
                    id: format!("block_{}", item.id),
                    evidence_ids: vec![item.evidence_ids[0].clone()],
                    content: item.visual_asset_id.as_ref().map_or_else(
                        || BlockContent::Text {
                            text: item.summary.clone(),
                        },
                        |asset_id| BlockContent::Image {
                            asset_id: asset_id.clone(),
                            caption: item.summary.clone(),
                        },
                    ),
                }],
            })
            .collect();
        document.validate().unwrap();
        document
    }
}

/// Exact canonical coverage failures. This detects deterministic regressions,
/// not factual entailment, arbitrary paraphrases, or human readability.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Missing required substantive items.
    pub missing: Vec<String>,
    /// Canonically described items lacking the annotated support or visual asset.
    pub unsupported: Vec<String>,
    /// Same item fully restated in multiple blocks (not merely cited twice).
    pub repeated: Vec<String>,
    /// Known false assertions appearing in reader-visible content.
    pub distorted: Vec<String>,
    /// Present tutorial/visual items emitted before their present prerequisite.
    pub out_of_order: Vec<String>,
}

impl Coverage {
    /// Whether all narrow deterministic quality checks passed.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.missing.is_empty()
            && self.unsupported.is_empty()
            && self.repeated.is_empty()
            && self.out_of_order.is_empty()
            && self.distorted.is_empty()
    }
}

/// Evaluate canonical content blocks and their evidence, ignoring headings and
/// unused source transcript. Reject invalid documents before calling this helper.
#[must_use]
pub fn coverage(case: &Case, document: &RecapDocument, depth: Depth) -> Coverage {
    let blocks: Vec<_> = document
        .sections
        .iter()
        .flat_map(|section| &section.blocks)
        .collect();
    let mut report = Coverage::default();
    for claim in &case.forbidden_claims {
        if blocks
            .iter()
            .any(|block| normalized(&block_text(&block.content)).contains(&normalized(claim)))
        {
            report.distorted.push(claim.clone());
        }
    }
    let mut positions = std::collections::BTreeMap::new();
    for item in &case.items {
        let matches: Vec<_> = blocks
            .iter()
            .enumerate()
            .filter_map(|(index, block)| {
                let text = normalized(&block_text(&block.content));
                if !item
                    .required_fragments
                    .iter()
                    .all(|fragment| text.contains(&normalized(fragment)))
                {
                    return None;
                }
                Some((
                    index,
                    text.find(&normalized(&item.required_fragments[0])).unwrap(),
                    *block,
                ))
            })
            .collect();
        if matches.is_empty() && item.required(depth) {
            report.missing.push(item.id.clone());
        }
        if matches.len() > 1 {
            report.repeated.push(item.id.clone());
        }
        if let Some((block_index, offset, _)) = matches.first() {
            positions.insert(item.id.as_str(), (*block_index, *offset));
        }
        if matches.iter().any(|(_, _, block)| {
            !item
                .evidence_ids
                .iter()
                .any(|id| block.evidence_ids.contains(id))
                || item
                    .visual_asset_id
                    .as_ref()
                    .is_some_and(|id| !uses_asset(&block.content, id))
        }) {
            report.unsupported.push(item.id.clone());
        }
    }
    for item in &case.items {
        if let Some(predecessor) = &item.after
            && let (Some(current), Some(prior)) = (
                positions.get(item.id.as_str()),
                positions.get(predecessor.as_str()),
            )
            && current < prior
        {
            report.out_of_order.push(item.id.clone());
        }
    }
    report
}

fn normalized(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn block_text(content: &BlockContent) -> String {
    match content {
        BlockContent::Text { text } | BlockContent::Heading { text } => text.clone(),
        BlockContent::List { items, .. } => items.join("\n"),
        BlockContent::Diagram { caption, steps } => format!("{caption}\n{}", steps.join("\n")),
        BlockContent::Image { caption, .. } | BlockContent::Clip { caption, .. } => caption.clone(),
        BlockContent::Comparison { items } => items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn uses_asset(content: &BlockContent, id: &str) -> bool {
    match content {
        BlockContent::Text { .. }
        | BlockContent::Heading { .. }
        | BlockContent::List { .. }
        | BlockContent::Diagram { .. } => false,
        BlockContent::Image { asset_id, .. } | BlockContent::Clip { asset_id, .. } => {
            asset_id == id
        }
        BlockContent::Comparison { items } => items.iter().any(|item| item.asset_id == id),
    }
}
