//! A guide after new pushes and against comparisons (see `docs/code-review-guide.md`).
//!
//! * **Refresh forks.** [`refresh_fork`] starts the next version of a guide pinned to the
//!   review's heads now: every section is copied, and the citations that still hold (fresh or
//!   moved) are remapped to the current revisions and lines, so only the stale sections need
//!   rewriting. [`pinned_to`] tells whether a version is already pinned to the heads now (a later
//!   refresh then edits it in place).
//! * **Since guides.** A guide of what changed since a comparison baseline cites only lines new
//!   since then: [`SinceDelta`] holds each compared source's delta with the rebase taken out and
//!   says why a citation is outside it.

use super::freshness::{GuideFreshness, new_lines};
use crate::data::{DataError, ReviewData};
use bcode_code_review_models::{
    Citation, CitationRelation, DeltaKind, Freshness, GuideHead, GuideOrigin, GuideScope,
    GuideStatus, GuideVersion, LayerDelta, LayerPair, LineSpan, REVIEW_GUIDE_SCHEMA_VERSION,
    ResolvedSource, ReviewCompareLayersRequest, ReviewGuide, ReviewLineSide, RevisionRef, Section,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;

/// The heads `sources` pin now, by source id (the first part of a source that resolves to
/// several), labelled from `labels` (else the source id).
#[must_use]
pub fn heads_of(
    sources: &[ResolvedSource],
    labels: &BTreeMap<String, String>,
) -> BTreeMap<String, GuideHead> {
    let mut heads = BTreeMap::new();
    for source in sources {
        heads
            .entry(source.source_id.clone())
            .or_insert_with(|| GuideHead {
                base: source.base.clone(),
                head: source.head.clone(),
                label: labels
                    .get(&source.source_id)
                    .filter(|label| !label.is_empty())
                    .cloned()
                    .unwrap_or_else(|| source.source_id.clone()),
            });
    }
    heads
}

/// Whether `version` is pinned to exactly `heads` (the same sources at the same base and head;
/// labels aside).
#[must_use]
pub fn pinned_to(version: &GuideVersion, heads: &BTreeMap<String, GuideHead>) -> bool {
    version.heads.len() == heads.len()
        && version.heads.iter().all(|(id, pinned)| {
            heads
                .get(id)
                .is_some_and(|now| now.base == pinned.base && now.head == pinned.head)
        })
}

/// The next version of `base`'s guide after new pushes: based on `base`, pinned to `current`
/// (the review's sources now, primary first; labelled from `labels`), complete, with every
/// section copied.
///
/// Citations `freshness` calls fresh or moved are remapped to the revision their side reads now
/// (and moved ones to their new lines); stale, gone, or untold ones are kept as they were for
/// the refresh to rewrite. Sections that stay fresh record `refreshed_from`. Returns the version
/// and the ids of the stale sections, in outline order.
#[must_use]
pub fn refresh_fork(
    base: &GuideVersion,
    freshness: &GuideFreshness,
    current: &[ResolvedSource],
    labels: &BTreeMap<String, String>,
    now_ms: u64,
) -> (GuideVersion, Vec<String>) {
    let heads = heads_of(current, labels);
    let primary = current.first().map(|source| source.source_id.as_str());
    let mut citations = base.citations.clone();
    for (id, citation) in &mut citations {
        let lines = match freshness.citations.get(id) {
            Some(Freshness::Fresh) => citation.lines,
            Some(Freshness::Moved { lines }) => *lines,
            _ => continue,
        };
        let Some(head) = citation
            .source_id
            .as_deref()
            .or(primary)
            .and_then(|source| heads.get(source))
        else {
            continue;
        };
        let revision = match citation.side {
            ReviewLineSide::New => head.head.clone(),
            ReviewLineSide::Old => head.base.clone(),
        };
        if revision != citation.revision {
            citation.blob_oid = None;
            citation.revision = revision;
        }
        citation.lines = lines;
    }
    let stale = freshness.stale_sections.clone();
    let mut outline = base.outline.clone();
    mark_refreshed(&mut outline, &stale, base.version);
    let mut origin = base.origin.clone();
    origin.session_id = None;
    let version = GuideVersion {
        guide_id: base.guide_id.clone(),
        version: base.version.saturating_add(1),
        schema_version: base.schema_version,
        heads,
        based_on: Some(base.version),
        origin,
        status: GuideStatus::Complete,
        generation: None,
        outline,
        citations,
        retired_sections: base.retired_sections.clone(),
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    };
    (version, stale)
}

/// A new since guide's row and first version.
///
/// The guide of `scope` is titled `title` for the review keyed by `workspace_id`; its version is
/// pinned to `current` (labelled from `labels`), planning with no sections yet, with
/// `directions` recorded.
#[must_use]
pub fn since_guide(
    workspace_id: &str,
    scope: GuideScope,
    title: &str,
    (current, labels): (&[ResolvedSource], &BTreeMap<String, String>),
    directions: Option<String>,
    now_ms: u64,
) -> (ReviewGuide, GuideVersion) {
    let guide_id = new_guide_id(workspace_id, now_ms);
    let guide = ReviewGuide {
        guide_id: guide_id.clone(),
        review_key: String::new(),
        scope,
        parent_guide_id: None,
        title: title.to_owned(),
        current_version: 1,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
        archived_at_ms: None,
    };
    let version = GuideVersion {
        guide_id,
        version: 1,
        schema_version: REVIEW_GUIDE_SCHEMA_VERSION,
        heads: heads_of(current, labels),
        based_on: None,
        origin: GuideOrigin {
            directions,
            ..GuideOrigin::default()
        },
        status: GuideStatus::Planning,
        generation: None,
        outline: Vec::new(),
        citations: BTreeMap::new(),
        retired_sections: Vec::new(),
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    };
    (guide, version)
}

/// A fresh guide id (`g` and 16 hex digits) for the review keyed by `workspace_id`.
#[must_use]
pub fn new_guide_id(workspace_id: &str, now_ms: u64) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hasher = Sha256::new();
    hasher.update(workspace_id.as_bytes());
    hasher.update(now_ms.to_le_bytes());
    hasher.update(
        COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_le_bytes(),
    );
    hasher.update(std::process::id().to_le_bytes());
    hasher
        .finalize()
        .iter()
        .take(8)
        .fold(String::from("g"), |mut id, byte| {
            let _ = write!(id, "{byte:02x}");
            id
        })
}

/// Whether `version` was written for code `pair` compares from: some source of it pinned to
/// the pair's baseline head.
#[must_use]
pub fn written_for_baseline(version: &GuideVersion, pairs: &[LayerPair]) -> bool {
    pairs
        .iter()
        .filter_map(|pair| pair.from.as_ref())
        .any(|from| {
            version
                .heads
                .values()
                .any(|head| head.head.commit_oid() == Some(from.head_oid.as_str()))
        })
}

fn mark_refreshed(sections: &mut [Section], stale: &[String], from: u32) {
    for section in sections {
        if !stale.contains(&section.id) {
            section.refreshed_from = Some(from);
        }
        mark_refreshed(&mut section.children, stale, from);
    }
}

/// One compared source's delta since the baseline.
#[derive(Debug, Clone)]
struct SourceDelta {
    delta: LayerDelta,
    fork: Option<String>,
    head: String,
}

/// Lines new since the baseline, by source id and path (`None`: not known).
type NewLines = BTreeMap<(String, String), Option<Vec<LineSpan>>>;

/// What is new since a comparison baseline, by source, for checking a since guide's citations.
#[derive(Debug)]
pub struct SinceDelta {
    by_source: BTreeMap<String, SourceDelta>,
    lines: RefCell<NewLines>,
}

impl SinceDelta {
    /// The deltas of `pairs`, each applied to the sources `numbers` (source id to pull request
    /// number) give its number.
    ///
    /// # Errors
    ///
    /// * [`DataError`] when the comparison cannot be computed.
    pub fn compute(
        data: &ReviewData,
        repo_path: &Path,
        pairs: &[LayerPair],
        numbers: &BTreeMap<String, u64>,
    ) -> Result<Self, DataError> {
        let deltas = if pairs.is_empty() {
            Vec::new()
        } else {
            data.compare_layers(&ReviewCompareLayersRequest {
                repo_path: repo_path.to_path_buf(),
                pairs: pairs.to_vec(),
            })?
            .deltas
        };
        Ok(Self::from_deltas(pairs, deltas, numbers))
    }

    /// The deltas of `pairs` as computed (in order).
    #[must_use]
    pub fn from_deltas(
        pairs: &[LayerPair],
        deltas: Vec<LayerDelta>,
        numbers: &BTreeMap<String, u64>,
    ) -> Self {
        let mut by_source = BTreeMap::new();
        for (pair, delta) in pairs.iter().zip(deltas) {
            for (source_id, _) in numbers.iter().filter(|(_, number)| **number == pair.number) {
                by_source.insert(
                    source_id.clone(),
                    SourceDelta {
                        delta: delta.clone(),
                        fork: pair.to.fork_oid.clone(),
                        head: pair.to.head_oid.clone(),
                    },
                );
            }
        }
        Self {
            by_source,
            lines: RefCell::new(BTreeMap::new()),
        }
    }

    /// Why `citation` (its source `primary` when it names none) is not about what changed since
    /// the baseline, or `None` when it is.
    ///
    /// New-side citations must overlap a line new since the baseline (with the rebase taken
    /// out). When those lines cannot be told (the layer is new to the reviewer, or its delta
    /// kept the rebase), a citation of changed lines counts. Old-side citations (code removed)
    /// are about the change whenever they cite changed lines.
    #[must_use]
    pub fn outside(
        &self,
        data: &ReviewData,
        repo_path: &Path,
        citation: &Citation,
        primary: Option<&str>,
    ) -> Option<String> {
        let place = format!(
            "{}:{}-{}",
            citation.path, citation.lines.start, citation.lines.end
        );
        let Some(source_id) = citation.source_id.as_deref().or(primary) else {
            return Some(format!(
                "{place} is not in a source compared with the baseline; cite lines new since then"
            ));
        };
        let Some(source) = self.by_source.get(source_id) else {
            return Some(format!(
                "{place} is in {source_id}, which is not compared with the baseline; cite lines new since then"
            ));
        };
        if citation.side == ReviewLineSide::Old {
            return (citation.relation != CitationRelation::Changed).then(|| {
                format!("{place} (old side) is not code this change removed; cite changed lines")
            });
        }
        if matches!(
            source.delta.kind,
            DeltaKind::Unchanged | DeltaKind::RebasedOnly
        ) {
            return Some(format!(
                "nothing in {source_id} is new since the baseline (it is {}); leave it out",
                if source.delta.kind == DeltaKind::Unchanged {
                    "unchanged"
                } else {
                    "only rebased"
                }
            ));
        }
        let spans = self
            .lines
            .borrow_mut()
            .entry((source_id.to_owned(), citation.path.clone()))
            .or_insert_with(|| {
                new_lines(
                    data,
                    repo_path,
                    &source.delta,
                    (source.fork.as_deref()?, &source.head),
                    &citation.path,
                )
            })
            .clone();
        let Some(spans) = spans else {
            return (citation.relation != CitationRelation::Changed).then(|| {
                format!("{place} is not changed code; cite lines new since the baseline")
            });
        };
        if spans
            .iter()
            .any(|(start, end)| *start <= citation.lines.end && citation.lines.start <= *end)
        {
            return None;
        }
        Some(if spans.is_empty() {
            format!(
                "nothing in {} is new since the baseline; leave it out",
                citation.path
            )
        } else {
            format!(
                "{place} is not new since the baseline; new lines there: {}",
                spans_text(&spans)
            )
        })
    }
}

/// `spans` as `4–6, 9` (the first few).
fn spans_text(spans: &[LineSpan]) -> String {
    const SHOWN: usize = 8;
    let mut text: Vec<String> = spans
        .iter()
        .take(SHOWN)
        .map(|(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}\u{2013}{end}")
            }
        })
        .collect();
    if spans.len() > SHOWN {
        text.push("…".to_owned());
    }
    text.join(", ")
}

/// A short form of `revision` for banners (`1a2b3c4`, `working tree`).
#[must_use]
pub fn short_revision(revision: &RevisionRef) -> String {
    match revision {
        RevisionRef::Commit { oid } => oid.chars().take(7).collect(),
        RevisionRef::EmptyTree => "empty tree".to_owned(),
        RevisionRef::Index => "index".to_owned(),
        RevisionRef::WorkingTree => "working tree".to_owned(),
    }
}

#[cfg(test)]
mod tests;
