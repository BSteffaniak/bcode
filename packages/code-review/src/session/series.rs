//! Choose the visible comparisons before asking git to list files.

use super::{OpenRequest, ReviewData};
use crate::data::DataError;
use bcode_code_review_models::{
    DraftAnchor, ResolvedSource, ReviewGroupKey, ReviewSeriesPresentation, ReviewSeriesView,
    ReviewSource, ReviewSourceKind, ReviewWorkspace,
};
use std::path::Path;

/// Description of a multi-commit source. Members are in selected order, even when their
/// comparisons aren't loaded. An absent `unavailable` need not mean a sparse replay was tried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitSeriesState {
    /// Canonical workspace source id.
    pub source_id: String,
    /// Each member's full OID and human-readable commit label.
    pub members: Vec<(String, String)>,
    /// Effective view, including fallback when an exact aggregate is unavailable.
    pub view: ReviewSeriesView,
    /// Whether the selected commits form a contiguous chain.
    pub contiguous: bool,
    /// Reason an exact combined comparison cannot be offered.
    pub unavailable: Option<String>,
}

/// A commits source resolved for presentation: every group it can show without further
/// work, and its series description when it has more than one distinct commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedCommitSeries {
    /// Series description; `None` when the selection pins to a single commit, which is an
    /// ordinary standalone source.
    pub state: Option<CommitSeriesState>,
    /// Members in selected order. A single-commit selection has one part-less entry.
    pub members: Vec<ResolvedSource>,
    /// Exact combined diff, when the chain is contiguous or the combined view asked for it.
    pub combined: Option<ResolvedSource>,
    /// Index into `members` shown by the one-member view.
    pub current: usize,
}

impl ResolvedCommitSeries {
    /// The groups the effective view shows, in display order.
    #[must_use]
    pub fn visible(&self) -> Vec<ResolvedSource> {
        let Some(state) = &self.state else {
            return self.members.clone();
        };
        match state.view {
            ReviewSeriesView::AllMembers => self.members.clone(),
            ReviewSeriesView::Combined => self.combined.iter().cloned().collect(),
            ReviewSeriesView::OneMember => self
                .members
                .get(self.current)
                .cloned()
                .into_iter()
                .collect(),
        }
    }

    /// Every resolved group (members first, then the combined diff), whether shown or not.
    #[must_use]
    pub fn all(&self) -> Vec<ResolvedSource> {
        self.members
            .iter()
            .chain(self.combined.as_ref())
            .cloned()
            .collect()
    }
}

/// Whether `source` may resolve as a series: a commits source naming several revisions. The
/// resolved [`ResolvedCommitSeries::state`] decides whether it actually is one.
pub const fn is_series(source: &ReviewSource) -> bool {
    matches!(&source.kind, ReviewSourceKind::Commits { revs } if revs.len() > 1)
}

pub(super) fn owns_selector(source: &ReviewSource, selector: &str) -> bool {
    source.id == selector || (is_series(source) && owns_group_selector(&source.id, selector))
}

/// Whether `selector` names a member or the combined group of series `source_id`.
fn owns_group_selector(source_id: &str, selector: &str) -> bool {
    let combined = ReviewGroupKey::Combined {
        source_id: source_id.to_owned(),
    };
    let member_prefix = ReviewGroupKey::Member {
        source_id: source_id.to_owned(),
        member: String::new(),
    }
    .selector();
    selector == combined.selector() || selector.starts_with(&member_prefix)
}

impl CommitSeriesState {
    /// Identity of a resolved group, applying [`ReviewGroupKey::for_resolved`] with this
    /// series as the only one.
    #[must_use]
    pub fn group_key(&self, source: &ResolvedSource) -> ReviewGroupKey {
        ReviewGroupKey::for_resolved(source, source.source_id == self.source_id)
    }

    /// Stable selector for a resolved group belonging to this series.
    #[must_use]
    pub fn selector(&self, source: &ResolvedSource) -> String {
        self.group_key(source).selector()
    }

    /// Groups that correspond to how this source resolved before it was a series: a
    /// contiguous chain was one diff (now the combined group), and a sparse selection was one
    /// part per commit (now the members).
    #[must_use]
    pub fn legacy_groups(&self) -> Vec<ReviewGroupKey> {
        if self.contiguous {
            vec![ReviewGroupKey::Combined {
                source_id: self.source_id.clone(),
            }]
        } else {
            self.members
                .iter()
                .map(|(oid, _)| ReviewGroupKey::Member {
                    source_id: self.source_id.clone(),
                    member: oid.clone(),
                })
                .collect()
        }
    }

    /// The group an anchor written without one belongs to. A contiguous chain was one diff
    /// whose head was the newest commit, so its anchors belong to the combined group; a sparse
    /// selection's anchors name their member by head commit. `None` when the anchor is for
    /// another source, already names a group, or names no member of this series.
    #[must_use]
    pub fn legacy_group(&self, anchor: &DraftAnchor) -> Option<ReviewGroupKey> {
        if anchor.group.is_some() || anchor.source_id.as_deref() != Some(&self.source_id) {
            return None;
        }
        if self.contiguous {
            return Some(ReviewGroupKey::Combined {
                source_id: self.source_id.clone(),
            });
        }
        let head = anchor.head_oid.as_deref()?;
        self.members
            .iter()
            .find(|(oid, _)| oid == head)
            .map(|(oid, _)| ReviewGroupKey::Member {
                source_id: self.source_id.clone(),
                member: oid.clone(),
            })
    }

    /// Carry the workspace-wide viewed marks of a review saved before this source was a
    /// series into its [`Self::legacy_groups`], once. Each group receives the marks as they
    /// were, so a file shows viewed exactly when its identity matched before. A series that
    /// already has group-scoped state (even an empty list) is left alone, which makes this
    /// idempotent and keeps later standalone marks out of the series. Returns whether the
    /// workspace changed.
    pub fn adopt_legacy_marks(&self, workspace: &mut ReviewWorkspace) -> bool {
        let groups = self.legacy_groups();
        let owned = workspace
            .group_viewed_marks
            .keys()
            .any(|selector| owns_group_selector(&self.source_id, selector));
        if owned || groups.is_empty() {
            return false;
        }
        let marks = crate::viewed::effective_marks(workspace);
        for group in groups {
            workspace
                .group_viewed_marks
                .insert(group.selector(), marks.clone());
        }
        true
    }
}

/// Resolve a commits source for presentation with its saved view.
///
/// This is the one rule for series views: no saved view opens a contiguous chain combined and a sparse selection with every
/// member; a combined view without an exact aggregate falls back to every member; a
/// one-member view restores the saved member (by full OID or member selector), else the first.
///
/// # Errors
///
/// * The source is not a commits source, selects nothing, or names a revision that cannot be
///   pinned.
/// * The repository cannot be read.
pub fn resolve_commit_series(
    data: &ReviewData,
    repo_path: &Path,
    source: &ReviewSource,
    saved: Option<&ReviewSeriesPresentation>,
) -> Result<ResolvedCommitSeries, DataError> {
    let wants_combined = saved.is_some_and(|p| p.view == ReviewSeriesView::Combined);
    let plan = data.commit_series(repo_path, source, wants_combined)?;
    let mut members = plan.members;
    if members.len() < 2 {
        // Aliases may pin to the same commit. It is an ordinary, untoggled source,
        // including for file identity and saved fingerprints.
        for member in &mut members {
            member.part = None;
        }
        return Ok(ResolvedCommitSeries {
            state: None,
            members,
            combined: None,
            current: 0,
        });
    }
    let mut view = saved.map_or_else(
        || {
            if plan.contiguous {
                ReviewSeriesView::Combined
            } else {
                ReviewSeriesView::AllMembers
            }
        },
        |p| p.view,
    );
    if view == ReviewSeriesView::Combined && plan.combined.is_none() {
        view = ReviewSeriesView::AllMembers;
    }
    let selected = saved.and_then(|p| p.last_member.as_deref());
    let current = selected
        .and_then(|selected| {
            members.iter().position(|member| {
                member.head.commit_oid().is_some_and(|oid| {
                    oid == selected
                        || ReviewGroupKey::Member {
                            source_id: source.id.clone(),
                            member: oid.to_owned(),
                        }
                        .selector()
                            == selected
                })
            })
        })
        .unwrap_or(0);
    let state = CommitSeriesState {
        source_id: source.id.clone(),
        members: members
            .iter()
            .filter_map(|m| {
                Some((
                    m.head.commit_oid()?.to_owned(),
                    m.part.as_ref()?.label.clone(),
                ))
            })
            .collect(),
        contiguous: plan.contiguous,
        unavailable: plan.unavailable,
        view,
    };
    Ok(ResolvedCommitSeries {
        state: Some(state),
        members,
        combined: plan.combined,
        current,
    })
}

/// Resolve a commits source for opening: its series description and every group it can show
/// without further work ([`ResolvedCommitSeries::all`]), so switching between members, every
/// member, and an already computed combined diff needs no further resolution. A sparse
/// selection's combined diff is only computed when the saved view asks for it.
pub(super) fn resolve(
    data: &ReviewData,
    request: &OpenRequest,
    source: &ReviewSource,
) -> Result<(Option<CommitSeriesState>, Vec<ResolvedSource>), DataError> {
    let resolved = resolve_commit_series(
        data,
        &request.repo_path,
        source,
        request.series_views.get(&source.id),
    )?;
    let groups = resolved.all();
    Ok((resolved.state, groups))
}
