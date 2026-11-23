//! Choose the visible comparisons before asking git to list files.

use super::{OpenRequest, ReviewData};
use crate::data::DataError;
use bcode_code_review_models::{
    ResolvedSource, ReviewGroupKey, ReviewSeriesView, ReviewSource, ReviewSourceKind,
};

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

pub(super) const fn is_series(source: &ReviewSource) -> bool {
    matches!(&source.kind, ReviewSourceKind::Commits { revs } if revs.len() > 1)
}

pub(super) fn owns_selector(source: &ReviewSource, selector: &str) -> bool {
    if source.id == selector {
        return true;
    }
    if !is_series(source) {
        return false;
    }
    let source_id = source.id.clone();
    selector
        == (ReviewGroupKey::Combined {
            source_id: source_id.clone(),
        })
        .selector()
        || selector.starts_with(
            &(ReviewGroupKey::Member {
                source_id,
                member: String::new(),
            })
            .selector(),
        )
}

pub(super) fn group_selector(source: &ResolvedSource, series: bool) -> String {
    if !series {
        return source.source_id.clone();
    }
    let source_id = source.source_id.clone();
    match (source.part.as_ref(), source.head.commit_oid()) {
        (Some(_), Some(oid)) => ReviewGroupKey::Member {
            source_id,
            member: oid.to_owned(),
        },
        _ => ReviewGroupKey::Combined { source_id },
    }
    .selector()
}

impl CommitSeriesState {
    /// Stable selector for a resolved group belonging to this series.
    #[must_use]
    pub fn selector(&self, source: &ResolvedSource) -> String {
        group_selector(source, source.source_id == self.source_id)
    }
}

pub(super) fn resolve(
    data: &ReviewData,
    request: &OpenRequest,
    source: &ReviewSource,
) -> Result<(Option<CommitSeriesState>, Vec<ResolvedSource>), DataError> {
    let saved = request.series_views.get(&source.id);
    let wants_combined = saved.is_some_and(|p| p.view == ReviewSeriesView::Combined);
    let mut plan = data.commit_series(&request.repo_path, source, wants_combined)?;
    if plan.members.len() < 2 {
        // Aliases may pin to the same commit. It is an ordinary, untoggled source,
        // including for file identity and saved fingerprints.
        for member in &mut plan.members {
            member.part = None;
        }
        return Ok((None, plan.members));
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
    if view == ReviewSeriesView::Combined
        && !wants_combined
        && plan.combined.is_none()
        && plan.unavailable.is_none()
    {
        // Pin the description we just chose, even if one of the requested refs moves.
        let mut pinned = source.clone();
        pinned.kind = ReviewSourceKind::Commits {
            revs: plan
                .members
                .iter()
                .filter_map(|member| member.head.commit_oid().map(str::to_owned))
                .collect(),
        };
        plan = data.commit_series(&request.repo_path, &pinned, true)?;
    }
    if view == ReviewSeriesView::Combined && plan.combined.is_none() {
        view = ReviewSeriesView::AllMembers;
    }
    let state = CommitSeriesState {
        source_id: source.id.clone(),
        members: plan
            .members
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
    let visible = match view {
        ReviewSeriesView::AllMembers => plan.members,
        ReviewSeriesView::Combined => plan.combined.into_iter().collect(),
        ReviewSeriesView::OneMember => {
            let selected = saved.and_then(|p| p.last_member.as_deref());
            let index = plan
                .members
                .iter()
                .position(|m| m.head.commit_oid() == selected)
                .unwrap_or(0);
            vec![plan.members.swap_remove(index)]
        }
    };
    Ok((Some(state), visible))
}
