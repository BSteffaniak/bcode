//! Group identity and series-local visibility, independent of comparison baselines.

use super::anchors::DocAnchor;
use super::keys::ItemKey;
use super::review::{BlockKind, Group, ReviewDocument};
use crate::session::CommitSeriesState;
use bcode_code_review_models::{DraftAnchor, ResolvedSource, ReviewGroupKey};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn group_key(
    source: &ResolvedSource,
    series: &BTreeMap<String, CommitSeriesState>,
) -> ReviewGroupKey {
    ReviewGroupKey::for_resolved(source, series.contains_key(&source.source_id))
}

impl ReviewDocument {
    /// Record that a commits source resolved as a multi-member series (for example when its
    /// description arrives after the document was built), re-identifying its groups. Returns
    /// whether any group's identity changed.
    pub fn set_commit_series(&mut self, state: CommitSeriesState) -> bool {
        let source_id = state.source_id.clone();
        if self.series.get(&source_id) == Some(&state) {
            return false;
        }
        self.series.insert(source_id.clone(), state);
        let mut changed = false;
        for index in 0..self.groups.len() {
            if self.groups[index].source.source_id != source_id {
                continue;
            }
            let key = group_key(&self.groups[index].source, &self.series);
            if self.groups[index].key != key {
                self.groups[index].key = key;
                changed = true;
                for file in self.groups[index].files.clone() {
                    self.refresh_viewed(file);
                }
                self.rebuild_group(index);
            }
        }
        if changed && let Some(annotations) = self.annotations_input.clone() {
            self.set_annotations(&annotations);
        }
        changed
    }

    /// The group a persisted anchor belongs to: the group it names, or for an anchor of a
    /// series written before it had groups, the group that representation became (see
    /// [`CommitSeriesState::legacy_group`]). `None` for anchors outside any series group.
    #[must_use]
    pub fn anchor_group(&self, anchor: &DraftAnchor) -> Option<ReviewGroupKey> {
        if let Some(group) = &anchor.group {
            return Some(group.clone());
        }
        self.series
            .get(anchor.source_id.as_deref()?)?
            .legacy_group(anchor)
    }

    /// Persist a file or line anchor with its exact series group. Standalone and review-level
    /// anchors keep their legacy shape. Callers that persist a series anchor should use this
    /// instead of the context-free [`DocAnchor::to_draft_anchor`].
    #[must_use]
    pub fn draft_anchor(&self, anchor: &DocAnchor) -> bcode_code_review_models::DraftAnchor {
        let mut draft = anchor.to_draft_anchor();
        if let Some(file) = &anchor.file
            && let Some(index) = self.file_index.get(file.as_ref())
        {
            let key = &self.groups[self.files[*index].group].key;
            if !matches!(key, ReviewGroupKey::Standalone { .. }) {
                draft.group = Some(key.clone());
            }
        }
        draft
    }

    /// Resolve an external selector to a visible group. A bare source id selects its first
    /// active group in document order; an exact group selector never falls back to a sibling.
    #[must_use]
    pub fn active_group(&self, selector: &str) -> Option<&ReviewGroupKey> {
        self.groups.iter().enumerate().find_map(|(index, group)| {
            (!self.group_hidden(index)
                && (group.source.source_id == selector || group.key.selector() == selector))
                .then_some(&group.key)
        })
    }

    /// Identity of the group holding an item, even when the group is not shown.
    #[must_use]
    pub fn group_of_key(&self, key: &ItemKey) -> Option<&ReviewGroupKey> {
        match self.blocks.get(self.block_of_key(key)?)?.kind {
            BlockKind::ReviewNotes => None,
            BlockKind::SourceHeader(group) => Some(&self.groups[group].key),
            BlockKind::File(file) => Some(&self.groups[self.files[file].group].key),
        }
    }

    pub(super) fn focus_hidden(&self, group: &Group) -> bool {
        group.hidden
            || self.hidden_groups.contains(&group.key)
            || self
                .group_focus
                .get(&group.source.source_id)
                .is_some_and(|key| key != &group.key)
    }

    pub(super) fn is_alternate(&self, group: &Group) -> bool {
        self.alternates.contains(&group.source.source_id)
            || self.group_alternates.contains(&group.key)
    }

    /// Show only `focus` within one workspace source, without focusing away unrelated sources.
    /// `None` clears just that source's focus. Focus does not remove files from progress totals.
    pub fn set_group_focus(&mut self, source_id: &str, focus: Option<&ReviewGroupKey>) -> bool {
        if self.group_focus.get(source_id) == focus {
            return false;
        }
        if let Some(key) = focus {
            let (ReviewGroupKey::Member {
                source_id: owner, ..
            }
            | ReviewGroupKey::Combined { source_id: owner }
            | ReviewGroupKey::Standalone { source_id: owner }) = key;
            if owner != source_id {
                return false;
            }
            self.group_focus.insert(source_id.to_owned(), key.clone());
        } else {
            self.group_focus.remove(source_id);
        }
        for index in 0..self.groups.len() {
            if self.groups[index].source.source_id == source_id {
                self.rebuild_group(index);
            }
        }
        true
    }

    /// Replace the exact groups hidden by the current presentation. Unlike alternates,
    /// these still count towards totals and viewed progress. An absent group may be named
    /// before streaming its index; groups not in the set gain no focus restriction.
    pub fn set_hidden_groups(&mut self, groups: BTreeSet<ReviewGroupKey>) -> bool {
        if self.hidden_groups == groups {
            return false;
        }
        let previous = std::mem::replace(&mut self.hidden_groups, groups);
        for index in 0..self.groups.len() {
            let key = &self.groups[index].key;
            if previous.contains(key) != self.hidden_groups.contains(key) {
                self.rebuild_group(index);
            }
        }
        true
    }

    /// Replace the set of exact groups omitted from both the sequence and progress totals.
    /// Independent of legacy source-wide alternates; can include groups awaiting materialization.
    pub fn set_alternate_groups(&mut self, groups: BTreeSet<ReviewGroupKey>) -> bool {
        if self.group_alternates == groups {
            return false;
        }
        let previous = std::mem::replace(&mut self.group_alternates, groups);
        for index in 0..self.groups.len() {
            let key = &self.groups[index].key;
            if previous.contains(key) != self.group_alternates.contains(key) {
                self.rebuild_group(index);
            }
        }
        true
    }

    fn rebuild_group(&mut self, index: usize) {
        self.rebuild_block(self.groups[index].header_block);
        for file in self.groups[index].files.clone() {
            self.rebuild_file(file);
        }
    }
}
