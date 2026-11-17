//! Guided review: a cited, versioned walkthrough of a review (see `docs/code-review-guide.md`).
//!
//! A [`ReviewGuide`] belongs to one review (by review key) and scope. Each [`GuideVersion`] is
//! pinned to the heads it was written against and holds an outline of [`Section`]s that share
//! [`Citation`]s. Fan-out children write [`GuideArea`]s; "ask" conversations are
//! [`GuideThread`]s; reviewed sections are [`GuideSectionMark`]s. [`Freshness`] is derived on
//! read and never stored.
//!
//! These are plain data. Checks, freshness, and planning live in `bcode_code_review::guide`.

use crate::{
    LayerPair, ReviewCompareBaseline, ReviewLineInterval, ReviewLineSide, ReviewScope,
    ReviewTarget, ReviewThreadSeverity, RevisionRef,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Current schema version of a stored [`GuideVersion`] document.
pub const REVIEW_GUIDE_SCHEMA_VERSION: u16 = 1;

/// Most sections (counted over the whole tree) in one version or area.
pub const MAX_GUIDE_SECTIONS: usize = 200;
/// Most citations in one version or area.
pub const MAX_GUIDE_CITATIONS: usize = 1_000;
/// Most bytes of one version's or area's document (outline and citations) as JSON.
pub const MAX_GUIDE_DOCUMENT_BYTES: usize = 1024 * 1024;
/// Most bytes of one section body.
pub const MAX_GUIDE_SECTION_BODY_BYTES: usize = 16 * 1024;
/// Most bytes of one citation excerpt.
pub const MAX_GUIDE_EXCERPT_BYTES: usize = 2 * 1024;
/// Most bytes of a guide, section, or finding title, and of a thread question's first line.
pub const MAX_GUIDE_TITLE_BYTES: usize = 1024;
/// Most bytes of one finding's text or one claim check's claim or note.
pub const MAX_GUIDE_FINDING_BYTES: usize = 4 * 1024;
/// Most bytes of one thread question.
pub const MAX_GUIDE_QUESTION_BYTES: usize = 16 * 1024;

/// Citation problem: the citation names a source the review does not have.
pub const GUIDE_CITE_UNKNOWN_LAYER: &str = "unknown_layer";
/// Citation problem: several sources contain the path; the citation must name one.
pub const GUIDE_CITE_AMBIGUOUS_SOURCE: &str = "ambiguous_source";
/// Citation problem: the review has no sources to cite.
pub const GUIDE_CITE_NO_SOURCES: &str = "no_sources";
/// Citation problem: the path does not exist at the cited revision.
pub const GUIDE_CITE_PATH_NOT_FOUND: &str = "path_not_found";
/// Citation problem: the diff renames the path; cite the new path.
pub const GUIDE_CITE_RENAMED: &str = "renamed";
/// Citation problem: the file does not exist on the cited side; cite the other side.
pub const GUIDE_CITE_DELETED_ON_SIDE: &str = "deleted_on_side";
/// Citation problem: the line range is empty or reversed.
pub const GUIDE_CITE_INVALID_RANGE: &str = "invalid_range";
/// Citation problem: the line range runs past the end of the file.
pub const GUIDE_CITE_RANGE_OUT_OF_BOUNDS: &str = "range_out_of_bounds";
/// Citation problem: the quote occurs several times in the file outside the cited range.
pub const GUIDE_CITE_QUOTE_AMBIGUOUS: &str = "quote_ambiguous";
/// Citation problem: the quote does not match; a close match was found.
pub const GUIDE_CITE_QUOTE_MISMATCH: &str = "quote_mismatch";
/// Citation problem: the quote was not found in the file.
pub const GUIDE_CITE_QUOTE_NOT_FOUND: &str = "quote_not_found";
/// Citation problem: the cited content could not be read (binary, too large, or a read error).
pub const GUIDE_CITE_UNREADABLE: &str = "unreadable";
/// Citation warning: the model said the range is in the diff, but no changed line is.
pub const GUIDE_CITE_NOT_IN_DIFF: &str = "not_in_diff";
/// Citation warning: the citation has no quote, so it cannot be verified.
pub const GUIDE_CITE_ADD_QUOTE: &str = "add_quote";
/// Citation note: the quote was found once elsewhere and the range moved to it.
pub const GUIDE_CITE_ADJUSTED: &str = "adjusted";

/// A guide: one per review and scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewGuide {
    /// Stable guide id.
    pub guide_id: String,
    /// The review it belongs to (the key AI exchanges and suggestions use).
    pub review_key: String,
    /// What it covers.
    pub scope: GuideScope,
    /// The stack guide a layer guide belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_guide_id: Option<String>,
    /// Title.
    pub title: String,
    /// The version shown by default.
    pub current_version: u32,
    /// When created, in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// When last changed, in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
    /// When archived, in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at_ms: Option<u64>,
}

/// What a guide covers.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GuideScope {
    /// The whole review: a file set, a diff, or a whole-stack diff.
    #[default]
    Review,
    /// The story across stepped stack layers.
    Stack {
        /// Layer source ids, bottom first.
        layers: Vec<String>,
    },
    /// One pull request layer.
    Layer {
        /// The layer's source id.
        source_id: String,
    },
    /// Changes since a comparison baseline.
    Since {
        /// The baseline compared with.
        baseline: ReviewCompareBaseline,
        /// The layer pairs compared.
        #[serde(default)]
        pairs: Vec<LayerPair>,
    },
}

/// One version of a guide: pinned to the heads it was written against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideVersion {
    /// The guide.
    pub guide_id: String,
    /// Version number, from 1.
    pub version: u32,
    /// Schema version of this document; see [`REVIEW_GUIDE_SCHEMA_VERSION`].
    pub schema_version: u16,
    /// Pinned revisions, by source id.
    #[serde(default)]
    pub heads: BTreeMap<String, GuideHead>,
    /// The version this one was forked from (refresh or restructure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub based_on: Option<u32>,
    /// Who wrote it, and how.
    #[serde(default)]
    pub origin: GuideOrigin,
    /// Where writing it stands.
    pub status: GuideStatus,
    /// Fan-out progress, when it fans out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationState>,
    /// The section tree.
    #[serde(default)]
    pub outline: Vec<Section>,
    /// Citations shared by the sections, by id.
    #[serde(default)]
    pub citations: BTreeMap<String, Citation>,
    /// Ids of sections removed from this version, which it never uses again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired_sections: Vec<String>,
    /// When created, in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// When last changed, in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
}

impl GuideVersion {
    /// The version's document: its outline and citations.
    #[must_use]
    pub fn document(&self) -> GuideDocument {
        GuideDocument {
            outline: self.outline.clone(),
            citations: self.citations.clone(),
            retired_sections: self.retired_sections.clone(),
        }
    }

    /// The section with `id`, anywhere in the tree.
    #[must_use]
    pub fn section(&self, id: &str) -> Option<&Section> {
        find_section(&self.outline, id)
    }
}

/// The section with `id` in `sections` or their descendants.
#[must_use]
pub fn find_section<'a>(sections: &'a [Section], id: &str) -> Option<&'a Section> {
    sections.iter().find_map(|section| {
        if section.id == id {
            Some(section)
        } else {
            find_section(&section.children, id)
        }
    })
}

/// Every section of `sections`, depth first, parents before children.
#[must_use]
pub fn all_sections(sections: &[Section]) -> Vec<&Section> {
    let mut all = Vec::new();
    let mut stack: Vec<&Section> = sections.iter().rev().collect();
    while let Some(section) = stack.pop() {
        all.push(section);
        stack.extend(section.children.iter().rev());
    }
    all
}

/// A guide document as stored: an outline and the citations its sections share.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideDocument {
    /// The section tree.
    #[serde(default)]
    pub outline: Vec<Section>,
    /// Citations, by id.
    #[serde(default)]
    pub citations: BTreeMap<String, Citation>,
    /// Ids of sections removed, which are never used again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired_sections: Vec<String>,
}

/// One source's pinned revisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideHead {
    /// Base side.
    pub base: RevisionRef,
    /// Head side.
    pub head: RevisionRef,
    /// Display label, such as `#102` or a branch name.
    #[serde(default)]
    pub label: String,
}

/// Who wrote a version, and how.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideOrigin {
    /// The writing session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The model it used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning effort it used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// How the writing was split.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<GuideStrategy>,
    /// How deep.
    #[serde(default)]
    pub depth: GuideDepth,
    /// The template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// The user's ad-hoc directions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directions: Option<String>,
    /// Digest of the context the writer was given, so a refresh can tell when it changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_digest: Option<String>,
}

/// How writing a version was split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuideStrategy {
    /// One agent.
    Single,
    /// One child per stack layer, then a stack guide.
    PerLayer,
    /// One child per area of the change, then synthesis.
    Areas,
}

/// How deep a guide goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuideDepth {
    /// Core sections only, no context beyond the diff.
    Brief,
    /// Adaptive sections; context outside the diff when relevant.
    #[default]
    Standard,
    /// More steps, callers, implementations and tests, and a verifier pass.
    Deep,
}

/// Where writing a version stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GuideStatus {
    /// Planning the outline.
    Planning,
    /// Writing sections.
    Generating,
    /// Combining fan-out areas.
    Synthesizing,
    /// A verifier is checking claims.
    Verifying,
    /// Done.
    Complete,
    /// Writing failed; written sections stay usable.
    Failed {
        /// What failed.
        message: String,
    },
    /// Writing was stopped; written sections stay usable.
    Cancelled,
    /// A newer version replaced it.
    Superseded,
}

impl GuideStatus {
    /// Stable name of the status kind.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Planning => "planning",
            Self::Generating => "generating",
            Self::Synthesizing => "synthesizing",
            Self::Verifying => "verifying",
            Self::Complete => "complete",
            Self::Failed { .. } => "failed",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
        }
    }

    /// The status named `kind` (with `message` for a failure); `None` for an unknown name.
    #[must_use]
    pub fn parse(kind: &str, message: Option<String>) -> Option<Self> {
        Some(match kind {
            "planning" => Self::Planning,
            "generating" => Self::Generating,
            "synthesizing" => Self::Synthesizing,
            "verifying" => Self::Verifying,
            "complete" => Self::Complete,
            "failed" => Self::Failed {
                message: message.unwrap_or_default(),
            },
            "cancelled" => Self::Cancelled,
            "superseded" => Self::Superseded,
            _ => return None,
        })
    }

    /// The failure message, for a failure.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Failed { message } => Some(message),
            _ => None,
        }
    }

    /// Whether writing has ended (complete, failed, cancelled, or superseded).
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Complete | Self::Failed { .. } | Self::Cancelled | Self::Superseded
        )
    }
}

/// Fan-out progress of a version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationState {
    /// The areas, in plan order.
    #[serde(default)]
    pub areas: Vec<GenerationArea>,
    /// Tokens used so far.
    #[serde(default)]
    pub tokens_used: u64,
    /// The soft token budget, when one applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u64>,
}

/// One planned area of a fan-out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationArea {
    /// Area id, such as a package root or a layer's source id.
    pub area_id: String,
    /// Display title.
    pub title: String,
    /// The child session writing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Where it stands.
    pub status: GuideAreaStatus,
}

/// Where one fan-out area stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuideAreaStatus {
    /// Waiting for a free slot.
    Queued,
    /// A child is writing it.
    Running,
    /// Written.
    Complete,
    /// The child failed.
    Failed,
    /// Stopped.
    Cancelled,
}

impl GuideAreaStatus {
    /// Stable name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// The status named `name`; `None` for an unknown name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "complete" => Self::Complete,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    /// Whether the area is done (complete, failed, or cancelled).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Failed | Self::Cancelled)
    }
}

/// One section of a guide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    /// Stable id (`s3`, `s3.1`), never reused within a guide.
    pub id: String,
    /// What it is.
    pub kind: SectionKind,
    /// Title.
    pub title: String,
    /// Markdown body; citation tokens are `[[c7]]`.
    #[serde(default)]
    pub body: String,
    /// Risks, questions, suggestions, and notes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    /// Citation ids in reading order (each a stop in the walkthrough).
    #[serde(default)]
    pub citations: Vec<String>,
    /// Child sections.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Self>,
    /// Whether it is written yet.
    #[serde(default)]
    pub state: SectionState,
    /// The version it was copied from by a refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refreshed_from: Option<u32>,
    /// Verifier results.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<ClaimCheck>,
}

impl Section {
    /// Every citation id the section itself uses (its own and its findings'), first use first,
    /// without repeats. Child sections are not included.
    #[must_use]
    pub fn cited_ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = Vec::new();
        let findings = self.findings.iter().flat_map(|finding| &finding.citations);
        for id in self.citations.iter().chain(findings) {
            if !ids.contains(&id.as_str()) {
                ids.push(id);
            }
        }
        ids
    }
}

/// What a section is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SectionKind {
    /// What the change is for (core).
    Purpose,
    /// What the change covers (core).
    Scope,
    /// The walkthrough (core); its children are steps.
    ReadingOrder,
    /// One stop in the walkthrough.
    Step,
    /// An adaptive or template section, by catalog key (`risks`, `threat_model`).
    Topic {
        /// Catalog key.
        key: String,
    },
    /// Findings gathered in one place.
    Findings,
    /// A link to a layer's own guide from a stack guide.
    LayerLink {
        /// The layer guide.
        guide_id: String,
        /// The layer's source id.
        source_id: String,
    },
    /// An expansion of its parent.
    Expansion,
    /// A free-form note.
    Note,
}

/// Whether a section is written.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SectionState {
    /// Planned in the outline, not written yet.
    Planned,
    /// Written.
    #[default]
    Written,
    /// Added by an expand or ask; becomes written when promoted.
    Provisional {
        /// What added it.
        origin: ProvisionalOrigin,
        /// The ask thread, for an ask.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
    },
}

/// What added a provisional section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvisionalOrigin {
    /// An expand request.
    Expand,
    /// An ask thread.
    Ask,
}

/// A risk, question, suggestion, or note in a section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Id, unique within the version.
    pub id: String,
    /// What it is.
    pub kind: FindingKind,
    /// How severe, as a review thread would say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<ReviewThreadSeverity>,
    /// Text.
    pub text: String,
    /// Citation ids.
    #[serde(default)]
    pub citations: Vec<String>,
    /// The draft thread it became.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drafted_thread_id: Option<String>,
}

/// What a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// Something that may go wrong.
    Risk,
    /// Something to ask the author.
    Question,
    /// Something to change.
    Suggestion,
    /// Something to know.
    Note,
}

/// A cited line range at a pinned revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    /// Id within the version (`c1`).
    pub id: String,
    /// The source (layer) cited; `None` for repository context outside the sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// Repository-relative path.
    pub path: String,
    /// Side.
    pub side: ReviewLineSide,
    /// Lines.
    pub lines: ReviewLineInterval,
    /// The revision read.
    pub revision: RevisionRef,
    /// The blob read, when known (the identity of working-tree and index content).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_oid: Option<String>,
    /// SHA-256 (hex) of the range's normalized text; empty when the range could not be read.
    #[serde(default)]
    pub quote_hash: String,
    /// The range's first lines, at most [`MAX_GUIDE_EXCERPT_BYTES`].
    #[serde(default)]
    pub excerpt: String,
    /// How the range relates to the review.
    pub relation: CitationRelation,
    /// Whether it checked out.
    pub verification: Verification,
}

/// How a cited range relates to the review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CitationRelation {
    /// Some cited line is changed in the diff.
    Changed,
    /// In a hunk, but only context lines.
    HunkContext,
    /// In a diff file, outside its hunks.
    Unchanged,
    /// In a file the review shows without a diff (file-set and repository reviews).
    InReview,
    /// In a file the review does not show (callers, implementations, configuration, tests).
    OutsideReview,
}

/// Whether a citation checked out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verification {
    /// The quoted text is at the cited lines.
    Verified {
        /// When checked, in milliseconds since the Unix epoch.
        at_ms: u64,
        /// The range the model gave, when the quote was found elsewhere and the range moved.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        adjusted_from: Option<ReviewLineInterval>,
    },
    /// Not checked (no quote was given).
    Unverified,
    /// The check found a problem; the citation is kept.
    Problem {
        /// Machine-readable code (`GUIDE_CITE_*`).
        code: String,
        /// One actionable sentence.
        message: String,
    },
}

impl Verification {
    /// Whether the citation is verified.
    #[must_use]
    pub const fn is_verified(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }
}

/// A verifier's judgement of one claim against its citations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimCheck {
    /// The claim, as written.
    pub claim: String,
    /// The judgement.
    pub status: ClaimStatus,
    /// Why.
    #[serde(default)]
    pub note: String,
    /// Citation ids checked.
    #[serde(default)]
    pub citations: Vec<String>,
    /// The verifying session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_session: Option<String>,
}

/// A verifier's judgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    /// The citations support it.
    Supported,
    /// The citations contradict it or do not support it.
    Unsupported,
    /// Cannot tell.
    Unclear,
}

/// Whether a citation still matches the review's current revisions. Derived, never stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Freshness {
    /// Unchanged, at the same lines.
    Fresh,
    /// Unchanged, at other lines.
    Moved {
        /// The lines now.
        lines: ReviewLineInterval,
    },
    /// The cited lines changed.
    Stale {
        /// What changed.
        reason: String,
    },
    /// The cited text or file is gone.
    Gone,
}

impl Freshness {
    /// Whether the citation no longer holds (stale or gone).
    #[must_use]
    pub const fn is_stale(&self) -> bool {
        matches!(self, Self::Stale { .. } | Self::Gone)
    }
}

/// One fan-out child's result for one area of one version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideArea {
    /// The guide.
    pub guide_id: String,
    /// The version.
    pub version: u32,
    /// Area id.
    pub area_id: String,
    /// The child session writing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Where it stands.
    pub status: GuideAreaStatus,
    /// The failure message, for a failed area.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The area's sections.
    #[serde(default)]
    pub outline: Vec<Section>,
    /// The area's citations, by id.
    #[serde(default)]
    pub citations: BTreeMap<String, Citation>,
    /// When created, in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// When last changed, in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
}

/// An "ask" conversation pinned to a section. It does not bump the version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideThread {
    /// Thread id.
    pub thread_id: String,
    /// The guide.
    pub guide_id: String,
    /// The version asked about.
    pub version: u32,
    /// The section asked about.
    pub section_id: String,
    /// The answering session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Where it stands.
    pub status: GuideThreadStatus,
    /// The question.
    pub question: String,
    /// When created, in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// When last changed, in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
}

/// Where an ask thread stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuideThreadStatus {
    /// Shown under its section.
    Open,
    /// Its answer became a written section.
    Promoted,
    /// Hidden.
    Discarded,
}

impl GuideThreadStatus {
    /// Stable name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Promoted => "promoted",
            Self::Discarded => "discarded",
        }
    }

    /// The status named `name`; `None` for an unknown name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "open" => Self::Open,
            "promoted" => Self::Promoted,
            "discarded" => Self::Discarded,
            _ => return None,
        })
    }
}

/// A section the reviewer marked reviewed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideSectionMark {
    /// The guide.
    pub guide_id: String,
    /// The section.
    pub section_id: String,
    /// When marked, in milliseconds since the Unix epoch.
    pub reviewed_at_ms: u64,
    /// Fingerprint of the section's citations when marked; a mismatch un-reviews it.
    pub fingerprint: String,
}

/// What one session may change in a guide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideGrant {
    /// The guide.
    pub guide_id: String,
    /// The version it may change; `None` for the guide's current version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// What it may do.
    pub mode: GrantMode,
}

/// What a grant allows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GrantMode {
    /// Write a version: everything on its own version.
    Generate,
    /// Write one fan-out area's result.
    Area {
        /// The area.
        area_id: String,
    },
    /// Add expansions under one section.
    Expand {
        /// The section.
        section_id: String,
    },
    /// Add expansions under one section and promote them.
    Ask {
        /// The section.
        section_id: String,
    },
    /// Rewrite these sections.
    Refresh {
        /// The sections.
        section_ids: Vec<String>,
    },
    /// Everything, including moving and removing written sections.
    Restructure,
    /// Record claim checks.
    Verify,
}

/// A guide row and its revision, which every write changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideRecord {
    /// The guide.
    pub guide: ReviewGuide,
    /// Row revision.
    pub revision: u64,
}

/// A version row and its revision, which every write changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideVersionRecord {
    /// The version.
    pub version: GuideVersion,
    /// Row revision.
    pub revision: u64,
}

/// A version as listed, without its document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideVersionSummary {
    /// The guide.
    pub guide_id: String,
    /// Version number.
    pub version: u32,
    /// Schema version of its document; a version with another than
    /// [`REVIEW_GUIDE_SCHEMA_VERSION`] cannot be read by this build.
    pub schema_version: u16,
    /// Where writing it stands; `None` for a status this build does not know.
    #[serde(default)]
    pub status: Option<GuideStatus>,
    /// The version it was forked from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub based_on: Option<u32>,
    /// Row revision.
    pub revision: u64,
    /// When created, in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// When last changed, in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
}

/// An area row and its revision, which every write changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideAreaRecord {
    /// The area.
    pub area: GuideArea,
    /// Row revision.
    pub revision: u64,
}

/// A citation as a writer asks for it, before it is checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitationRequest {
    /// Source id to cite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Layer to cite, as `#102` or a source id (an alternative to `source`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    /// Repository-relative path.
    pub path: String,
    /// Side; the new side by default.
    #[serde(default = "new_side")]
    pub side: ReviewLineSide,
    /// First line (1-based, inclusive).
    pub start_line: u32,
    /// Last line (1-based, inclusive).
    pub end_line: u32,
    /// Text expected at the lines; it verifies the citation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    /// Symbol the citation is about; used to suggest where it is when the quote is not found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// What the writer believes the range is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation_hint: Option<CitationRelationHint>,
}

const fn new_side() -> ReviewLineSide {
    ReviewLineSide::New
}

/// What a writer believes a cited range is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CitationRelationHint {
    /// Changed lines of the diff.
    Diff,
    /// Code around or outside the change.
    Context,
}

/// One problem or warning a citation check found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitationProblem {
    /// Machine-readable code (`GUIDE_CITE_*`).
    pub code: String,
    /// One actionable sentence.
    pub message: String,
    /// Structured help: candidates, suggestions, bounds.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub suggestion: serde_json::Value,
}

/// How a citation check ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CitationCheckStatus {
    /// The quote is at the cited lines.
    Verified,
    /// The quote was found once elsewhere; the range moved to it.
    Adjusted,
    /// No quote; kept unverified.
    Unverified,
    /// A problem was found; kept with the problem.
    Problem,
}

/// The result of checking one citation: the citation to store (never dropped), how the check
/// ended, and what it found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitationCheck {
    /// The citation to store; its id is the caller's to assign.
    pub citation: Citation,
    /// How the check ended.
    pub status: CitationCheckStatus,
    /// Problems (the first is the citation's verification problem).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<CitationProblem>,
    /// Warnings and notes that do not make the citation a problem.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<CitationProblem>,
}

/// Request payload to create a guide with its first version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateReviewGuideRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// Review scope; the guide is kept under the review key it names.
    #[serde(default)]
    pub scope: Option<ReviewScope>,
    /// The guide; its review key and current version are set by the store.
    pub guide: ReviewGuide,
    /// The first version; its number is set to 1.
    pub version: GuideVersion,
}

/// Response payload for [`CreateReviewGuideRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateReviewGuideResponse {
    /// The guide as stored.
    pub guide: GuideRecord,
    /// The first version as stored.
    pub version: GuideVersionRecord,
}

/// Request payload to list a review's guides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListReviewGuidesRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Review target.
    pub target: ReviewTarget,
    /// Review scope.
    #[serde(default)]
    pub scope: Option<ReviewScope>,
    /// Include archived guides.
    #[serde(default)]
    pub include_archived: bool,
}

/// Response payload for [`ListReviewGuidesRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListReviewGuidesResponse {
    /// The guides, most recently changed first.
    pub guides: Vec<GuideRecord>,
}

/// Request payload to read one guide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReviewGuideRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The guide.
    pub guide_id: String,
}

/// Response payload for [`GetReviewGuideRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReviewGuideResponse {
    /// The guide, when it exists.
    pub guide: Option<GuideRecord>,
}

/// Request payload to replace a guide row (title, parent, current version, archive time).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateReviewGuideRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The guide as it should be; its review key is kept.
    pub guide: ReviewGuide,
    /// The revision it was read at.
    pub expected_revision: u64,
}

/// Response payload for [`UpdateReviewGuideRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateReviewGuideResponse {
    /// The new revision.
    pub revision: u64,
}

/// Request payload to delete a guide with its versions, areas, threads, and marks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteReviewGuideRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The guide.
    pub guide_id: String,
}

/// Response payload for [`DeleteReviewGuideRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteReviewGuideResponse {
    /// Whether it existed.
    pub deleted: bool,
}

/// Request payload to move a review's guides to another review key (an unsaved review was
/// saved).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RekeyReviewGuidesRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The scope they are kept under now.
    pub from: ReviewScope,
    /// The scope to keep them under.
    pub to: ReviewScope,
}

/// Response payload for [`RekeyReviewGuidesRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RekeyReviewGuidesResponse {
    /// Guides moved.
    pub moved: usize,
}

/// Request payload to read one version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetGuideVersionRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The guide.
    pub guide_id: String,
    /// The version; `None` for the guide's current version.
    #[serde(default)]
    pub version: Option<u32>,
}

/// Response payload for [`GetGuideVersionRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetGuideVersionResponse {
    /// The version, when it exists.
    pub version: Option<GuideVersionRecord>,
}

/// Request payload to list a guide's versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListGuideVersionsRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The guide.
    pub guide_id: String,
}

/// Response payload for [`ListGuideVersionsRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListGuideVersionsResponse {
    /// The versions, oldest first.
    pub versions: Vec<GuideVersionSummary>,
}

/// Request payload to replace a version when its row is still at a revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveGuideVersionRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The version as it should be.
    pub version: GuideVersion,
    /// The revision it was read at.
    pub expected_revision: u64,
}

/// Response payload for [`SaveGuideVersionRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveGuideVersionResponse {
    /// The new revision.
    pub revision: u64,
}

/// Request payload to add the next version of a guide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkGuideVersionRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The new version; its number is set to the next free one.
    pub version: GuideVersion,
    /// Make it the guide's current version.
    #[serde(default = "yes")]
    pub make_current: bool,
}

const fn yes() -> bool {
    true
}

/// Response payload for [`ForkGuideVersionRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkGuideVersionResponse {
    /// The guide as it is now.
    pub guide: GuideRecord,
    /// The new version as stored.
    pub version: GuideVersionRecord,
}

/// Request payload to write a fan-out area.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveGuideAreaRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The area as it should be.
    pub area: GuideArea,
    /// The revision it was read at; `None` creates it (failing when it exists).
    #[serde(default)]
    pub expected_revision: Option<u64>,
}

/// Response payload for [`SaveGuideAreaRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveGuideAreaResponse {
    /// The new revision.
    pub revision: u64,
}

/// Request payload to list a version's fan-out areas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListGuideAreasRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The guide.
    pub guide_id: String,
    /// The version.
    pub version: u32,
}

/// Response payload for [`ListGuideAreasRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListGuideAreasResponse {
    /// The areas, by area id.
    pub areas: Vec<GuideAreaRecord>,
}

/// Request payload to create or replace an ask thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveGuideThreadRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The thread.
    pub thread: GuideThread,
}

/// Response payload for [`SaveGuideThreadRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveGuideThreadResponse {
    /// The thread as stored.
    pub thread: GuideThread,
}

/// Request payload to list a guide's ask threads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListGuideThreadsRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The guide.
    pub guide_id: String,
    /// Only threads about this version.
    #[serde(default)]
    pub version: Option<u32>,
}

/// Response payload for [`ListGuideThreadsRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListGuideThreadsResponse {
    /// The threads, oldest first.
    pub threads: Vec<GuideThread>,
}

/// Request payload to delete an ask thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteGuideThreadRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The thread.
    pub thread_id: String,
}

/// Response payload for [`DeleteGuideThreadRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteGuideThreadResponse {
    /// Whether it existed.
    pub deleted: bool,
}

/// Request payload to mark a section reviewed (replacing an earlier mark).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetGuideSectionMarkRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The mark.
    pub mark: GuideSectionMark,
}

/// Response payload for [`SetGuideSectionMarkRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetGuideSectionMarkResponse {}

/// Request payload to un-review a section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClearGuideSectionMarkRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The guide.
    pub guide_id: String,
    /// The section.
    pub section_id: String,
}

/// Response payload for [`ClearGuideSectionMarkRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClearGuideSectionMarkResponse {
    /// Whether it was marked.
    pub cleared: bool,
}

/// Request payload to list a guide's reviewed sections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListGuideSectionMarksRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The guide.
    pub guide_id: String,
}

/// Response payload for [`ListGuideSectionMarksRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListGuideSectionMarksResponse {
    /// The marks, by section id.
    pub marks: Vec<GuideSectionMark>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(id: &str, children: Vec<Section>) -> Section {
        Section {
            id: id.to_owned(),
            kind: SectionKind::Step,
            title: id.to_owned(),
            body: String::new(),
            findings: Vec::new(),
            citations: Vec::new(),
            children,
            state: SectionState::Written,
            refreshed_from: None,
            checks: Vec::new(),
        }
    }

    #[test]
    fn sections_are_found_anywhere_in_the_tree_parents_first() {
        let outline = vec![
            section("s1", Vec::new()),
            section(
                "s2",
                vec![section("s2.1", vec![section("s2.1.1", Vec::new())])],
            ),
            section("s3", Vec::new()),
        ];
        assert_eq!(
            find_section(&outline, "s2.1.1").map(|section| section.id.as_str()),
            Some("s2.1.1")
        );
        assert!(find_section(&outline, "s4").is_none());
        let order: Vec<&str> = all_sections(&outline)
            .into_iter()
            .map(|section| section.id.as_str())
            .collect();
        assert_eq!(order, ["s1", "s2", "s2.1", "s2.1.1", "s3"]);
    }

    #[test]
    fn a_section_cites_its_own_and_its_findings_citations_once() {
        let mut step = section("s1", Vec::new());
        step.citations = vec!["c1".to_owned(), "c2".to_owned()];
        step.findings.push(Finding {
            id: "f1".to_owned(),
            kind: FindingKind::Risk,
            severity: Some(ReviewThreadSeverity::Warning),
            text: "races".to_owned(),
            citations: vec!["c2".to_owned(), "c3".to_owned()],
            drafted_thread_id: None,
        });
        assert_eq!(step.cited_ids(), ["c1", "c2", "c3"]);
    }

    #[test]
    fn statuses_round_trip_by_name_and_unknown_names_are_not_guessed() {
        for status in [
            GuideStatus::Planning,
            GuideStatus::Generating,
            GuideStatus::Synthesizing,
            GuideStatus::Verifying,
            GuideStatus::Complete,
            GuideStatus::Failed {
                message: "boom".to_owned(),
            },
            GuideStatus::Cancelled,
            GuideStatus::Superseded,
        ] {
            let parsed = GuideStatus::parse(status.as_str(), status.message().map(str::to_owned));
            assert_eq!(parsed, Some(status));
        }
        assert_eq!(GuideStatus::parse("dreaming", None), None);
        assert_eq!(GuideAreaStatus::parse("later"), None);
        assert_eq!(GuideThreadStatus::parse("archived"), None);
    }

    #[test]
    fn a_citation_request_cites_the_new_side_unless_told_otherwise() {
        let request: CitationRequest = serde_json::from_value(serde_json::json!({
            "path": "src/lib.rs", "start_line": 3, "end_line": 4
        }))
        .expect("request");
        assert_eq!(request.side, ReviewLineSide::New);
        let section: Section = serde_json::from_value(serde_json::json!({
            "id": "s1", "kind": {"kind": "topic", "key": "risks"}, "title": "Risks"
        }))
        .expect("section");
        assert_eq!(section.state, SectionState::Written);
        assert_eq!(
            section.kind,
            SectionKind::Topic {
                key: "risks".to_owned()
            }
        );
    }
}
