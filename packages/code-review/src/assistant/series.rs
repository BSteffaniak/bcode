//! Assistant projection of review groups. Every resolved group stays addressable by its
//! selector; the review's view only decides which groups are active (counted in totals and
//! listed by default). Source ids always remain workspace ids.

use super::OpenReview;
use crate::session::ResolvedCommitSeries;
use bcode_code_review_models::{
    ResolvedSource, ReviewGroupKey, ReviewSeriesView, ReviewSource, ReviewSourceKind,
    ReviewStackView, ReviewWorkspace,
};
use std::collections::{BTreeMap, BTreeSet};

impl OpenReview {
    /// Identity of a resolved group: [`ReviewGroupKey::for_resolved`], with series-ness taken
    /// from how the review resolved the group's source.
    #[must_use]
    pub fn group_key(&self, resolved: &ResolvedSource) -> ReviewGroupKey {
        ReviewGroupKey::for_resolved(resolved, self.series.contains_key(&resolved.source_id))
    }

    /// Whether resolved group `index` is shown by the review's current view.
    #[must_use]
    pub fn is_active(&self, index: usize) -> bool {
        self.resolved
            .get(index)
            .is_some_and(|resolved| !self.hidden.contains(&self.group_key(resolved)))
    }

    pub(super) fn matches_selector(&self, resolved: &ResolvedSource, selector: &str) -> bool {
        resolved.source_id == selector || self.group_key(resolved).selector() == selector
    }

    /// Narrow several groups a name matched: an exact group selector wins, then the active
    /// ones. One group is returned unchanged.
    pub(super) fn narrow(&self, mut found: Vec<usize>, name: &str) -> Vec<usize> {
        if found.len() < 2 {
            return found;
        }
        if let Some(&exact) = found
            .iter()
            .find(|&&index| self.group_key(&self.resolved[index]).selector() == name)
        {
            return vec![exact];
        }
        if found.iter().any(|&index| self.is_active(index)) {
            found.retain(|&index| self.is_active(index));
        }
        found
    }

    /// Selectors of resolved groups, for messages that ask for one.
    pub(super) fn selectors_of(&self, indices: &[usize]) -> Vec<String> {
        let mut out: Vec<String> = indices
            .iter()
            .map(|&index| self.group_key(&self.resolved[index]).selector())
            .collect();
        out.dedup();
        out
    }
}

/// Groups of one commit series resolution, and the ones its view hides.
pub(super) fn commit_groups(
    resolved: &ResolvedCommitSeries,
) -> (Vec<ResolvedSource>, BTreeSet<ReviewGroupKey>) {
    let Some(state) = &resolved.state else {
        return (resolved.members.clone(), BTreeSet::new());
    };
    let shown: BTreeSet<ReviewGroupKey> = resolved
        .visible()
        .iter()
        .map(|group| state.group_key(group))
        .collect();
    let all = resolved.all();
    let hidden = all
        .iter()
        .map(|group| state.group_key(group))
        .filter(|key| !shown.contains(key))
        .collect();
    (all, hidden)
}

pub(super) const fn is_stack(source: &ReviewSource) -> bool {
    matches!(
        source.kind,
        ReviewSourceKind::Stack { .. } | ReviewSourceKind::StackLayer { .. }
    )
}

/// The stack groups the review's view leaves out of totals, with the review app's policy:
/// the saved series view of the stack (keyed by its whole-stack source, else its first
/// layer), else [`bcode_code_review_models::ReviewPresentationState::stack_view`], which
/// defaults to one layer at a time. The whole-stack view leaves out the layers; the layer
/// views leave out the whole-stack diff (one-layer view only moves between layers, which all
/// still count). A stack missing one representation keeps the other.
pub(super) fn stack_hidden(
    workspace: &ReviewWorkspace,
    resolved: &[ResolvedSource],
) -> BTreeSet<ReviewGroupKey> {
    use ReviewSourceKind::{Stack, StackLayer};
    let present = |source: &&ReviewSource| resolved.iter().any(|r| r.source_id == source.id);
    let mut families = BTreeMap::<_, Vec<&ReviewSource>>::new();
    for source in workspace
        .sources
        .iter()
        .filter(|s| s.included)
        .filter(present)
    {
        if let Stack {
            provider,
            repository,
            ..
        }
        | StackLayer {
            provider,
            repository,
            ..
        } = &source.kind
        {
            families
                .entry((provider, repository))
                .or_default()
                .push(source);
        }
    }
    let presentation = workspace.presentation.clone().unwrap_or_default();
    let standalone = |source: &ReviewSource| ReviewGroupKey::Standalone {
        source_id: source.id.clone(),
    };
    let mut hidden = BTreeSet::new();
    for sources in families.into_values() {
        let whole = sources.iter().find(|s| matches!(s.kind, Stack { .. }));
        let layers: Vec<&ReviewSource> = sources
            .iter()
            .copied()
            .filter(|s| matches!(s.kind, StackLayer { .. }))
            .collect();
        let (Some(whole), Some(first)) = (whole, layers.first()) else {
            continue;
        };
        let view = presentation
            .series_views
            .get(&whole.id)
            .or_else(|| presentation.series_views.get(&first.id))
            .map_or_else(
                || match presentation.stack_view() {
                    ReviewStackView::Whole => ReviewSeriesView::Combined,
                    ReviewStackView::AllLayers => ReviewSeriesView::AllMembers,
                    ReviewStackView::OneLayer => ReviewSeriesView::OneMember,
                },
                |saved| saved.view,
            );
        if view == ReviewSeriesView::Combined {
            hidden.extend(layers.iter().map(|s| standalone(s)));
        } else {
            hidden.insert(standalone(whole));
        }
    }
    hidden
}
