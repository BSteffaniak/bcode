//! Freshness: whether a version's citations still hold at the review's current revisions.
//!
//! [`citation_freshness`] decides one citation from what is known now: whether its content is
//! the same, the current file's lines, and (for commit heads with fork points) which lines are
//! new since the pinned version with the rebase taken out. [`assess_version`] gathers those
//! through [`ReviewData`], caching results for immutable revisions in the review cache.

use super::cite::read_file;
use super::text::{normalized, quote_hash};
use crate::cache::CacheKind;
use crate::data::{DataError, ReviewData};
use bcode_code_review_models::{
    Citation, DeltaKind, FileSince, Freshness, GuideVersion, LayerDelta, LayerPair, LineSpan,
    ResolvedSource, ReviewCompareLayersRequest, ReviewCompareMarksRequest, ReviewLineInterval,
    ReviewLineSide, RevisionRef, Section, VersionPoint, all_sections,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// What is known now about one citation's text.
#[derive(Debug, Clone, Copy, Default)]
pub struct FreshnessInput<'a> {
    /// The cited content is known to be unchanged (same commit and path, or same blob).
    pub same_content: bool,
    /// The current file's lines; `None` when the file no longer exists.
    pub lines: Option<&'a [String]>,
    /// Current lines new since the pinned version, with the rebase taken out (sorted, disjoint);
    /// `None` when no such delta is known.
    pub changed: Option<&'a [LineSpan]>,
}

/// Whether `citation` still holds, from what is known now.
///
/// * Unchanged content is fresh.
/// * A missing file is gone.
/// * With a delta, the citation is found again by its excerpt and quote hash: a changed span over
///   its lines (or text no longer found) makes it stale; otherwise it is fresh, or moved when its
///   lines moved.
/// * Without a delta, text found again is fresh or moved, and text not found is gone.
#[must_use]
pub fn citation_freshness(citation: &Citation, input: FreshnessInput<'_>) -> Freshness {
    if input.same_content {
        return Freshness::Fresh;
    }
    let Some(lines) = input.lines else {
        return Freshness::Gone;
    };
    let found = refind(citation, lines);
    match (input.changed, found) {
        (Some(changed), Some(at)) => {
            let overlapping = overlaps(changed, at);
            if overlapping.is_empty() {
                settled(citation, at)
            } else {
                Freshness::Stale {
                    reason: changed_reason(&overlapping),
                }
            }
        }
        (Some(changed), None) => {
            let overlapping = overlaps(changed, citation.lines);
            Freshness::Stale {
                reason: if overlapping.is_empty() {
                    "the cited text changed".to_owned()
                } else {
                    changed_reason(&overlapping)
                },
            }
        }
        (None, Some(at)) => settled(citation, at),
        (None, None) => Freshness::Gone,
    }
}

fn settled(citation: &Citation, at: ReviewLineInterval) -> Freshness {
    if at == citation.lines {
        Freshness::Fresh
    } else {
        Freshness::Moved { lines: at }
    }
}

fn overlaps(changed: &[LineSpan], lines: ReviewLineInterval) -> Vec<LineSpan> {
    changed
        .iter()
        .filter(|(start, end)| *start <= lines.end && lines.start <= *end)
        .copied()
        .collect()
}

fn changed_reason(spans: &[LineSpan]) -> String {
    let named: Vec<String> = spans
        .iter()
        .take(3)
        .map(|(start, end)| {
            if start == end {
                format!("line {start}")
            } else {
                format!("lines {start}\u{2013}{end}")
            }
        })
        .collect();
    let more = if spans.len() > 3 { " and more" } else { "" };
    format!(
        "{}{more} changed since the guide was written",
        named.join(", ")
    )
}

/// Where the citation's text is in `lines`: its excerpt's lines at a position whose range hashes
/// to its quote hash, nearest its old lines first.
#[must_use]
pub fn refind(citation: &Citation, lines: &[String]) -> Option<ReviewLineInterval> {
    let length = usize::try_from(citation.lines.end.checked_sub(citation.lines.start)?)
        .ok()?
        .checked_add(1)?;
    let excerpt: Vec<&str> = citation.excerpt.lines().take(length).collect();
    let (complete, last) = match excerpt.split_last() {
        Some((last, complete)) => (complete, *last),
        None => return None,
    };
    if length > lines.len() {
        return None;
    }
    let matches_at = |start: usize| {
        complete
            .iter()
            .zip(&lines[start..])
            .all(|(want, have)| normalized(have) == normalized(want))
            && lines
                .get(start + complete.len())
                .is_some_and(|have| have.starts_with(normalized(last)))
            && (citation.quote_hash.is_empty()
                || quote_hash(&lines[start..start + length]) == citation.quote_hash)
    };
    let origin = usize::try_from(citation.lines.start).ok()?.checked_sub(1)?;
    let last_start = lines.len() - length;
    // Nearest the old position first, earlier before later at equal distance.
    for distance in 0..=origin.max(last_start) {
        let before = origin.checked_sub(distance).filter(|at| *at <= last_start);
        let after = (distance > 0)
            .then(|| origin + distance)
            .filter(|at| *at <= last_start);
        for at in [before, after].into_iter().flatten() {
            if matches_at(at) {
                let start = u32::try_from(at + 1).ok()?;
                let end = u32::try_from(at + length).ok()?;
                return Some(ReviewLineInterval { start, end });
            }
        }
    }
    None
}

/// The freshness of a version's citations, and the sections they make stale.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideFreshness {
    /// Each citation's freshness, by id.
    pub citations: BTreeMap<String, Freshness>,
    /// Sections with a stale or gone citation of their own (or their findings'), in outline
    /// order.
    pub stale_sections: Vec<String>,
}

/// Sections of `outline` that use a citation `freshness` calls stale or gone.
#[must_use]
pub fn stale_sections(outline: &[Section], freshness: &BTreeMap<String, Freshness>) -> Vec<String> {
    all_sections(outline)
        .into_iter()
        .filter(|section| {
            section
                .cited_ids()
                .into_iter()
                .any(|id| freshness.get(id).is_some_and(Freshness::is_stale))
        })
        .map(|section| section.id.clone())
        .collect()
}

/// The freshness of every citation of `version` against `current`, the review's sources as they
/// resolve now (primary first).
///
/// A citation reads the current source with its source id (the primary source for repository
/// context); one whose source is no longer in the review is gone. Commit heads with fork points
/// use the layer's delta with the rebase taken out, so a rebase-only push keeps the guide fresh.
///
/// # Errors
///
/// * [`DataError`] when the repository cannot be read or compared. A file that cannot be read
///   at the current revision makes its citations gone rather than failing.
pub fn assess_version(
    data: &ReviewData,
    repo_path: &Path,
    version: &GuideVersion,
    current: &[ResolvedSource],
) -> Result<GuideFreshness, DataError> {
    let by_id: BTreeMap<&str, &ResolvedSource> = current
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect();
    let primary = current.first();
    let deltas = layer_deltas(data, repo_path, version, &by_id)?;
    let disk = data.disk();
    let mut contents: BTreeMap<(RevisionRef, String), Option<Vec<String>>> = BTreeMap::new();
    let mut marks: BTreeMap<(String, String), Option<Vec<LineSpan>>> = BTreeMap::new();
    let mut citations = BTreeMap::new();
    for (id, citation) in &version.citations {
        let source = citation
            .source_id
            .as_ref()
            .map_or(primary, |source_id| by_id.get(source_id.as_str()).copied());
        let Some(source) = source else {
            citations.insert(id.clone(), Freshness::Gone);
            continue;
        };
        let now = match citation.side {
            ReviewLineSide::New => &source.head,
            ReviewLineSide::Old => &source.base,
        };
        if *now == citation.revision && !now.is_live() {
            citations.insert(id.clone(), Freshness::Fresh);
            continue;
        }
        let key = cache_key(version, citation, source, now);
        if let Some(key) = &key
            && let Some(entry) = disk
                .get_many(CacheKind::Freshness, std::slice::from_ref(key))
                .get(key)
            && let Ok(known) = serde_json::from_str::<Freshness>(&entry.value)
        {
            citations.insert(id.clone(), known);
            continue;
        }
        let changed = if citation.side == ReviewLineSide::New {
            let delta = citation
                .source_id
                .as_deref()
                .or_else(|| primary.map(|source| source.source_id.as_str()))
                .and_then(|source_id| deltas.get(source_id));
            delta.and_then(|delta| {
                marks
                    .entry((source.source_id.clone(), citation.path.clone()))
                    .or_insert_with(|| {
                        changed_lines(data, repo_path, delta, source, &citation.path)
                    })
                    .clone()
            })
        } else {
            None
        };
        let lines = contents
            .entry((now.clone(), citation.path.clone()))
            .or_insert_with(|| {
                read_file(data, repo_path, now, &citation.path)
                    .ok()
                    .flatten()
            });
        let freshness = citation_freshness(
            citation,
            FreshnessInput {
                same_content: false,
                lines: lines.as_deref(),
                changed: changed.as_deref(),
            },
        );
        if let Some(key) = key
            && let Ok(json) = serde_json::to_string(&freshness)
        {
            disk.put_many(CacheKind::Freshness, &[(key, json)]);
        }
        citations.insert(id.clone(), freshness);
    }
    let stale_sections = stale_sections(&version.outline, &citations);
    Ok(GuideFreshness {
        citations,
        stale_sections,
    })
}

/// The cache key of a citation's freshness, when every revision involved is a commit (so the
/// answer never changes).
fn cache_key(
    version: &GuideVersion,
    citation: &Citation,
    source: &ResolvedSource,
    now: &RevisionRef,
) -> Option<String> {
    let pinned = citation.revision.commit_oid()?;
    let now = now.commit_oid()?;
    let head = version.heads.get(&source.source_id);
    let old = head.map_or(String::new(), |head| {
        format!(
            "{}..{}",
            head.base.commit_oid().unwrap_or_default(),
            head.head.commit_oid().unwrap_or_default()
        )
    });
    let new = format!(
        "{}..{}",
        source.base.commit_oid().unwrap_or_default(),
        source.head.commit_oid().unwrap_or_default()
    );
    Some(format!(
        "{pinned} {now} {old} {new} {:?} {}-{} {}\u{1}{}",
        citation.side, citation.lines.start, citation.lines.end, citation.quote_hash, citation.path
    ))
}

/// Each source's delta from its pinned version to now, for sources whose pinned and current
/// sides are commits.
fn layer_deltas(
    data: &ReviewData,
    repo_path: &Path,
    version: &GuideVersion,
    current: &BTreeMap<&str, &ResolvedSource>,
) -> Result<BTreeMap<String, LayerDelta>, DataError> {
    let mut ids = Vec::new();
    let mut pairs = Vec::new();
    for (source_id, pinned) in &version.heads {
        let Some(now) = current.get(source_id.as_str()) else {
            continue;
        };
        let (Some(old_head), Some(new_head)) = (pinned.head.commit_oid(), now.head.commit_oid())
        else {
            continue;
        };
        let (Some(old_fork), Some(new_fork)) = (pinned.base.commit_oid(), now.base.commit_oid())
        else {
            continue;
        };
        ids.push(source_id.clone());
        pairs.push(LayerPair {
            number: u64::try_from(pairs.len()).unwrap_or(u64::MAX),
            from: Some(VersionPoint {
                head_oid: old_head.to_owned(),
                fork_oid: Some(old_fork.to_owned()),
            }),
            to: VersionPoint {
                head_oid: new_head.to_owned(),
                fork_oid: Some(new_fork.to_owned()),
            },
        });
    }
    if pairs.is_empty() {
        return Ok(BTreeMap::new());
    }
    let deltas = data
        .compare_layers(&ReviewCompareLayersRequest {
            repo_path: repo_path.to_path_buf(),
            pairs,
        })?
        .deltas;
    Ok(ids.into_iter().zip(deltas).collect())
}

/// Current lines of `path` new since the pinned version, by the layer's delta; `None` when the
/// delta could not take the rebase out or the file is too large to mark.
fn changed_lines(
    data: &ReviewData,
    repo_path: &Path,
    delta: &LayerDelta,
    source: &ResolvedSource,
    path: &str,
) -> Option<Vec<LineSpan>> {
    new_lines(
        data,
        repo_path,
        delta,
        (source.base.commit_oid()?, source.head.commit_oid()?),
        path,
    )
}

/// Lines of `path` at `head` (its change starting at `fork`) new since the baseline `delta`
/// compares with, with the rebase taken out; `None` when the delta could not take the rebase
/// out or the file is too large to mark.
pub(crate) fn new_lines(
    data: &ReviewData,
    repo_path: &Path,
    delta: &LayerDelta,
    (fork, head): (&str, &str),
    path: &str,
) -> Option<Vec<LineSpan>> {
    match delta.kind {
        DeltaKind::Unchanged | DeltaKind::RebasedOnly => return Some(Vec::new()),
        DeltaKind::Changed => {}
        DeltaKind::NewToYou | DeltaKind::WithRebase | DeltaKind::Unavailable => return None,
    }
    if delta.file(path) == FileSince::ReplayConflict {
        return None;
    }
    if delta.file(path) == FileSince::Unchanged {
        return Some(Vec::new());
    }
    let marks = data
        .compare_marks(&ReviewCompareMarksRequest {
            repo_path: repo_path.to_path_buf(),
            fork_oid: fork.to_owned(),
            replay_oid: delta.replay_oid.clone()?,
            head_oid: head.to_owned(),
            path: path.to_owned(),
            old_path: None,
        })
        .ok()?;
    (!marks.too_large).then_some(marks.new_lines)
}

#[cfg(test)]
mod tests;
