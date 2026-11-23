//! Assistant projection of the active review groups. Source ids always remain workspace ids.

use super::{AssistantError, OpenReview};
use crate::ReviewData;
use bcode_code_review_models::{
    ResolvedSource, ReviewGroupKey, ReviewSeriesView, ReviewSource, ReviewSourceKind,
    ReviewWorkspace,
};

pub(super) const fn is_commit_series(source: &ReviewSource) -> bool {
    matches!(&source.kind, ReviewSourceKind::Commits { revs } if revs.len() > 1)
}

impl OpenReview {
    /// Identity of an active resolved group in the canonical workspace.
    #[must_use]
    pub fn group_key(&self, resolved: &ResolvedSource) -> ReviewGroupKey {
        let source_id = resolved.source_id.clone();
        if self
            .workspace
            .sources
            .iter()
            .any(|source| source.id == source_id && is_commit_series(source))
        {
            if resolved.part.is_none() {
                return ReviewGroupKey::Combined { source_id };
            }
            if let Some(oid) = resolved.head.commit_oid() {
                return ReviewGroupKey::Member {
                    source_id,
                    member: oid.to_owned(),
                };
            }
        }
        ReviewGroupKey::Standalone { source_id }
    }

    pub(super) fn matches_selector(&self, resolved: &ResolvedSource, selector: &str) -> bool {
        resolved.source_id == selector || self.group_key(resolved).selector() == selector
    }
}

pub(super) fn active_commits(
    data: &ReviewData,
    workspace: &ReviewWorkspace,
    source: &ReviewSource,
) -> Result<Vec<ResolvedSource>, AssistantError> {
    let preference = workspace
        .presentation
        .as_ref()
        .and_then(|state| state.series_views.get(&source.id));
    let description = data.commit_series(&workspace.repo_root, source, false)?;
    let mode = preference.map_or_else(
        || {
            if description.contiguous {
                ReviewSeriesView::Combined
            } else {
                ReviewSeriesView::AllMembers
            }
        },
        |state| state.view,
    );
    match mode {
        ReviewSeriesView::AllMembers => Ok(description.members),
        ReviewSeriesView::OneMember => {
            let last = preference.and_then(|state| state.last_member.as_deref());
            let member = description
                .members
                .into_iter()
                .find(|member| {
                    last.is_none_or(|id| {
                        member.head.commit_oid().is_some_and(|oid| {
                            oid == id
                                || (ReviewGroupKey::Member {
                                    source_id: source.id.clone(),
                                    member: oid.to_owned(),
                                })
                                .selector()
                                    == id
                        })
                    })
                })
                .ok_or_else(|| {
                    AssistantError::Invalid(format!("selected member is not in {}", source.label))
                })?;
            Ok(vec![member])
        }
        ReviewSeriesView::Combined => {
            let plan = data.commit_series(&workspace.repo_root, source, true)?;
            plan.combined.map(|group| vec![group]).ok_or_else(|| {
                AssistantError::Invalid(
                    plan.unavailable
                        .unwrap_or_else(|| format!("{} has no exact combined diff", source.label)),
                )
            })
        }
    }
}

/// Project existing stack sources without creating forge identities. Without an explicit
/// grouping, a headless layer review exposes every layer; a whole-stack review shows its diff.
pub(super) fn stack_sources(
    workspace: &ReviewWorkspace,
) -> Result<std::collections::BTreeSet<String>, AssistantError> {
    use ReviewSourceKind::{Stack, StackLayer};
    let mut families = std::collections::BTreeMap::new();
    for source in workspace.sources.iter().filter(|source| source.included) {
        match &source.kind {
            Stack {
                provider,
                repository,
                ..
            }
            | StackLayer {
                provider,
                repository,
                ..
            } => {
                families
                    .entry((provider, repository))
                    .or_insert_with(Vec::new)
                    .push(source);
            }
            _ => {}
        }
    }
    let mut included = std::collections::BTreeSet::new();
    for sources in families.into_values() {
        let whole = sources
            .iter()
            .copied()
            .find(|s| matches!(s.kind, Stack { .. }));
        let layers: Vec<_> = sources
            .iter()
            .copied()
            .filter(|s| matches!(s.kind, StackLayer { .. }))
            .collect();
        let owner = whole.unwrap_or(sources[0]);
        let presentation = workspace.presentation.as_ref();
        let pref = presentation.and_then(|p| p.series_views.get(&owner.id));
        let mode = pref
            .map(|p| p.view)
            .or_else(|| {
                presentation
                    .and_then(|p| p.stack_view)
                    .map(|view| match view {
                        bcode_code_review_models::ReviewStackView::Whole => {
                            ReviewSeriesView::Combined
                        }
                        bcode_code_review_models::ReviewStackView::OneLayer => {
                            ReviewSeriesView::OneMember
                        }
                        bcode_code_review_models::ReviewStackView::AllLayers => {
                            ReviewSeriesView::AllMembers
                        }
                    })
            })
            .unwrap_or_else(|| {
                if matches!(sources[0].kind, Stack { .. }) {
                    ReviewSeriesView::Combined
                } else {
                    ReviewSeriesView::AllMembers
                }
            });
        if pref.is_some()
            && ((mode == ReviewSeriesView::Combined && whole.is_none())
                || (mode != ReviewSeriesView::Combined && layers.is_empty()))
        {
            return Err(AssistantError::Invalid(format!(
                "{} has no locally recorded sources for {mode:?}",
                owner.label
            )));
        }
        match mode {
            ReviewSeriesView::Combined if whole.is_some() => {
                included.insert(owner.id.clone());
            }
            _ if layers.is_empty() => {
                included.insert(owner.id.clone());
            }
            ReviewSeriesView::OneMember => {
                let last = pref.and_then(|p| p.last_member.as_deref());
                let layer = layers
                    .iter()
                    .find(|s| last.is_none_or(|id| id == s.id))
                    .ok_or_else(|| {
                        AssistantError::Invalid(
                            "selected stack member is no longer in the review".into(),
                        )
                    })?;
                included.insert(layer.id.clone());
            }
            _ => included.extend(layers.iter().map(|s| s.id.clone())),
        }
    }
    Ok(included)
}

pub(super) fn viewed_state(
    workspace: &ReviewWorkspace,
    group: &ReviewGroupKey,
    file: &crate::viewed::ViewedIdentity<'_>,
    reset: crate::viewed::ViewedReset,
) -> crate::viewed::ViewedState {
    if matches!(group, ReviewGroupKey::Standalone { .. }) {
        crate::viewed::viewed_state(workspace, file, reset)
    } else {
        crate::viewed::group_viewed_state(workspace, group, file, reset)
    }
}

pub(super) fn set_viewed(
    workspace: &mut ReviewWorkspace,
    group: &ReviewGroupKey,
    file: &crate::viewed::ViewedIdentity<'_>,
    viewed: bool,
) {
    match (group, viewed) {
        (ReviewGroupKey::Standalone { .. }, true) => crate::viewed::mark_viewed(workspace, file),
        (ReviewGroupKey::Standalone { .. }, false) => crate::viewed::unmark_viewed(workspace, file),
        (_, true) => crate::viewed::mark_group_viewed(workspace, group, std::slice::from_ref(file)),
        (_, false) => crate::viewed::unmark_group_viewed(workspace, group, file),
    }
}
