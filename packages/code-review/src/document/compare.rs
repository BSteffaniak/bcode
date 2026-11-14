//! Comparison overlays: how a source's files changed since a baseline, shown on the same
//! document (see `docs/review-stack-history.md`).
//!
//! A comparison never changes the document's sources, files, or keys. It only changes how files
//! show: which are folded or hidden, which lines are marked new since the baseline, and, in the
//! "only what changed" view, which commit a changed file's hunks are diffed from (the baseline's
//! change replayed on today's base). Viewed state always uses each file's own identity, so it
//! reads the same in both views.

use super::items::CompareView;
use super::keys::FileKey;
use super::review::{Pending, ReviewDocument, Ticket};
use bcode_code_review_models::{
    DeltaKind, FileSince, LayerDelta, LineSpan, RemovedSince, ReviewCompareMarksRequest,
    ReviewCompareMarksResponse,
};
use std::collections::BTreeMap;
use std::sync::Arc;

/// One source's comparison.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupComparison {
    /// What the layer's comparison found; `None` while it computes.
    pub delta: Option<LayerDelta>,
    /// How the source's files show.
    pub view: CompareView,
    /// Whether the comparison ends at an older version than the one the source shows: its
    /// changed files show only what changed (diffed to that version) and take no comments.
    pub historical: bool,
    /// Comparisons of single files that replace the source's for those files (a file stepped
    /// to another pair of versions), by path. One whose version is not the source's head is
    /// historical for that file.
    pub files: BTreeMap<String, LayerDelta>,
}

/// One file's lines new since the baseline.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Marks {
    pub new_lines: Vec<LineSpan>,
    pub newly_removed: Vec<LineSpan>,
    pub removed_since: Vec<RemovedSince>,
}

fn in_spans(spans: &[LineSpan], line: u32) -> bool {
    let index = spans.partition_point(|(_, end)| *end < line);
    spans.get(index).is_some_and(|(start, _)| *start <= line)
}

impl Marks {
    /// Whether new-side line `line` is new since the baseline.
    pub fn new_line(&self, line: u32) -> bool {
        in_spans(&self.new_lines, line)
    }

    /// Whether old-side line `line` is removed since the baseline.
    pub fn removed(&self, line: u32) -> bool {
        in_spans(&self.newly_removed, line)
    }
}

/// Whether a file's marks are loaded.
#[derive(Debug, Clone)]
pub(super) enum MarksState {
    Missing,
    Pending(Ticket),
    Loaded(Arc<Marks>),
    Failed,
}

impl ReviewDocument {
    /// Compare source `source_id` (`None`: stop comparing it): its files fold, hide, and mark
    /// their lines by how they changed since the baseline, and in [`CompareView::Changes`] its
    /// changed files show only what changed. Viewed state, keys, and annotations are unchanged.
    /// Returns whether anything changed.
    pub fn set_comparison(&mut self, source_id: &str, comparison: Option<GroupComparison>) -> bool {
        if self.comparisons.get(source_id) == comparison.as_ref() {
            return false;
        }
        let replay_of = |comparison: Option<&GroupComparison>| {
            comparison.map(|comparison| {
                let of =
                    |delta: &LayerDelta| (delta.replay_oid.clone(), delta.pair.to.head_oid.clone());
                (
                    comparison.delta.as_ref().map(of),
                    comparison
                        .files
                        .iter()
                        .map(|(path, delta)| (path.clone(), of(delta)))
                        .collect::<Vec<_>>(),
                )
            })
        };
        let marks_stale =
            replay_of(self.comparisons.get(source_id)) != replay_of(comparison.as_ref());
        match comparison {
            Some(comparison) => self.comparisons.insert(source_id.to_owned(), comparison),
            None => self.comparisons.remove(source_id),
        };
        self.reapply(source_id, marks_stale);
        true
    }

    /// Show every compared source in `view`. Returns whether anything changed.
    pub fn set_compare_view(&mut self, view: CompareView) -> bool {
        let changed: Vec<String> = self
            .comparisons
            .iter()
            .filter(|(_, comparison)| comparison.view != view)
            .map(|(source, _)| source.clone())
            .collect();
        for source in &changed {
            if let Some(comparison) = self.comparisons.get_mut(source) {
                comparison.view = view;
            }
            self.reapply(source, false);
        }
        !changed.is_empty()
    }

    /// The comparison of source `source_id`, when it is compared.
    #[must_use]
    pub fn comparison(&self, source_id: &str) -> Option<&GroupComparison> {
        self.comparisons.get(source_id)
    }

    /// How `file` changed since its source's baseline, when the source is compared (and the
    /// comparison says something about its files).
    #[must_use]
    pub fn file_since(&self, file: &FileKey) -> Option<FileSince> {
        self.since_of(*self.file_index.get(file)?)
    }

    /// Compare file `file` alone with `delta` (`None`: as its source is compared again), as
    /// when stepping through the file's versions. Returns whether anything changed.
    pub fn set_file_comparison(&mut self, file: &FileKey, delta: Option<LayerDelta>) -> bool {
        let Some(&index) = self.file_index.get(file) else {
            return false;
        };
        let source_id = self.groups[self.files[index].group]
            .source
            .source_id
            .clone();
        let path = self.files[index].entry.path.clone();
        let Some(comparison) = self.comparisons.get_mut(&source_id) else {
            return false;
        };
        let changed = match delta {
            Some(delta) => comparison.files.insert(path, delta.clone()) != Some(delta),
            None => comparison.files.remove(&path).is_some(),
        };
        if changed {
            if let MarksState::Pending(ticket) = self.files[index].marks {
                self.pending.remove(&ticket);
            }
            self.files[index].marks = MarksState::Missing;
            let resident = !matches!(self.files[index].content, super::review::Content::Unloaded);
            if resident
                && (self.files[index].content_base != self.changes_base(index)
                    || self.files[index].content_head != self.changes_head(index))
            {
                self.unload(index);
            } else {
                self.rebuild_file(index);
            }
            let block = self.groups[self.files[index].group].header_block;
            self.rebuild_block(block);
        }
        changed
    }

    /// Show (or hide again) the files a comparison hides in source `source_id`: its unchanged
    /// files in the changes view, or every file of a layer that was only rebased. Returns
    /// whether anything changed.
    pub fn set_compare_files_shown(&mut self, source_id: &str, shown: bool) -> bool {
        let changed = if shown {
            self.compare_files_shown.insert(source_id.to_owned())
        } else {
            self.compare_files_shown.remove(source_id)
        };
        if changed {
            self.reapply(source_id, false);
        }
        changed
    }

    /// Whether the files a comparison hides in source `source_id` are shown.
    #[must_use]
    pub fn compare_files_shown(&self, source_id: &str) -> bool {
        self.compare_files_shown.contains(source_id)
    }

    /// Show or hide the text of the removed-since marker of `file` before new-side line
    /// `before`. Returns whether the file has such a marker.
    pub fn toggle_removal(&mut self, file: &FileKey, before: u32) -> bool {
        let Some(&index) = self.file_index.get(file) else {
            return false;
        };
        let has = self.file_marks(index).is_some_and(|marks| {
            marks
                .removed_since
                .iter()
                .any(|r| r.before_new_line == before)
        });
        if !has {
            return false;
        }
        let key = (file.clone(), before);
        if !self.expanded_removals.remove(&key) {
            self.expanded_removals.insert(key);
        }
        self.rebuild_file(index);
        true
    }

    /// Accept a file's marks. Returns whether the result was applied.
    pub fn accept_marks(&mut self, ticket: Ticket, response: ReviewCompareMarksResponse) -> bool {
        let Some(Pending::Marks(file)) = self.pending.get(&ticket).copied() else {
            return false;
        };
        self.pending.remove(&ticket);
        let marks = if response.too_large {
            Marks::default()
        } else {
            Marks {
                new_lines: response.new_lines,
                newly_removed: response.newly_removed,
                removed_since: response.removed_since,
            }
        };
        self.files[file].marks = MarksState::Loaded(Arc::new(marks));
        self.rebuild_file(file);
        true
    }

    /// Rebuild every group of `source_id` after its comparison changed: files whose content
    /// is diffed from another commit now are loaded again, and with `marks_stale` their marks
    /// are dropped.
    fn reapply(&mut self, source_id: &str, marks_stale: bool) {
        let groups: Vec<usize> = (0..self.groups.len())
            .filter(|group| self.groups[*group].source.source_id == source_id)
            .collect();
        for group in groups {
            for file in self.groups[group].files.clone() {
                if marks_stale {
                    if let MarksState::Pending(ticket) = self.files[file].marks {
                        self.pending.remove(&ticket);
                    }
                    self.files[file].marks = MarksState::Missing;
                }
                let resident =
                    !matches!(self.files[file].content, super::review::Content::Unloaded);
                if resident
                    && (self.files[file].content_base != self.changes_base(file)
                        || self.files[file].content_head != self.changes_head(file))
                {
                    self.unload(file);
                } else {
                    self.rebuild_file(file);
                }
            }
            self.rebuild_block(self.groups[group].header_block);
        }
    }

    pub(super) fn group_comparison(&self, group: usize) -> Option<&GroupComparison> {
        self.comparisons.get(&self.groups[group].source.source_id)
    }

    /// The delta of `group`'s comparison when it describes the revisions the group shows (or,
    /// for a historical comparison, an older version on purpose).
    fn current_delta(&self, group: usize) -> Option<&LayerDelta> {
        let comparison = self.group_comparison(group)?;
        let delta = comparison.delta.as_ref()?;
        let head = self.groups[group].source.head.commit_oid();
        (comparison.historical || head.is_none_or(|head| head == delta.pair.to.head_oid))
            .then_some(delta)
    }

    /// The delta file `file` shows: its own override, else its source's.
    fn file_delta(&self, file: usize) -> Option<&LayerDelta> {
        let group = self.files[file].group;
        let comparison = self.group_comparison(group)?;
        comparison
            .files
            .get(&self.files[file].entry.path)
            .or_else(|| self.current_delta(group))
    }

    /// Whether file `file` is compared up to an older version than its source shows.
    pub(super) fn historical(&self, file: usize) -> bool {
        let group = self.files[file].group;
        let Some(comparison) = self.group_comparison(group) else {
            return false;
        };
        let whole = comparison.historical && comparison.delta.is_some();
        comparison
            .files
            .get(&self.files[file].entry.path)
            .map_or(whole, |delta| {
                self.groups[group]
                    .source
                    .head
                    .commit_oid()
                    .is_some_and(|head| head != delta.pair.to.head_oid)
            })
    }

    /// How file `file` shows: an older version always shows only what changed.
    fn file_view(&self, file: usize) -> Option<CompareView> {
        let comparison = self.group_comparison(self.files[file].group)?;
        Some(if self.historical(file) {
            CompareView::Changes
        } else {
            comparison.view
        })
    }

    /// Whether `file` is compared up to an older version than its source shows (its lines are
    /// that version's, read-only).
    #[must_use]
    pub fn file_is_historical(&self, file: &FileKey) -> bool {
        self.file_index
            .get(file)
            .is_some_and(|index| self.historical(*index))
    }

    /// The override comparison of `file`, when it was stepped to another pair of versions.
    #[must_use]
    pub fn file_override(&self, file: &FileKey) -> Option<&LayerDelta> {
        let index = *self.file_index.get(file)?;
        self.group_comparison(self.files[index].group)?
            .files
            .get(&self.files[index].entry.path)
    }

    /// How file `file` changed since its source's baseline.
    pub(super) fn since_of(&self, file: usize) -> Option<FileSince> {
        let group = self.files[file].group;
        let comparison = self.group_comparison(group)?;
        let path = &self.files[file].entry.path;
        if comparison.delta.is_none() && !comparison.files.contains_key(path) {
            return Some(FileSince::Unknown);
        }
        let delta = self.file_delta(file)?;
        match delta.kind {
            DeltaKind::Unchanged | DeltaKind::RebasedOnly => Some(FileSince::Unchanged),
            DeltaKind::Changed | DeltaKind::WithRebase => Some(delta.file(path)),
            DeltaKind::NewToYou | DeltaKind::Unavailable => None,
        }
    }

    /// Whether file `file` is unchanged since its source's baseline.
    pub(super) fn unchanged_since(&self, file: usize) -> bool {
        self.since_of(file) == Some(FileSince::Unchanged)
    }

    /// Whether the comparison folds file `file` whatever its viewed mark: a file it would hide,
    /// shown anyway, shows as its header.
    pub(super) fn compare_folded(&self, file: usize) -> bool {
        self.hidden_by_comparison(file)
    }

    /// Whether the comparison hides file `file`.
    pub(super) fn compare_hidden(&self, file: usize) -> bool {
        let group = self.files[file].group;
        !self
            .compare_files_shown
            .contains(&self.groups[group].source.source_id)
            && self.hidden_by_comparison(file)
    }

    /// Whether the comparison would hide file `file` (before "show files").
    fn hidden_by_comparison(&self, file: usize) -> bool {
        let group = self.files[file].group;
        let Some(comparison) = self.group_comparison(group) else {
            return false;
        };
        if comparison.files.contains_key(&self.files[file].entry.path) {
            // A stepped file is shown: it is the one being looked at.
            return false;
        }
        let Some(delta) = self.current_delta(group) else {
            return false;
        };
        delta.kind == DeltaKind::RebasedOnly
            || (self.file_view(file) == Some(CompareView::Changes) && self.unchanged_since(file))
    }

    /// The commit file `file`'s hunks are diffed from in the changes view, when it is shown
    /// there.
    pub(super) fn changes_base(&self, file: usize) -> Option<String> {
        if self.file_view(file)? != CompareView::Changes {
            return None;
        }
        let delta = self.file_delta(file)?;
        if !matches!(delta.kind, DeltaKind::Changed | DeltaKind::WithRebase) {
            return None;
        }
        self.since_of(file)
            .filter(|since| since.changed())
            .and_then(|_| delta.replay_oid.clone())
    }

    /// The commit file `file`'s hunks are diffed to, when it shows an older version.
    pub(super) fn changes_head(&self, file: usize) -> Option<String> {
        if !self.historical(file) {
            return None;
        }
        self.changes_base(file)?;
        Some(self.file_delta(file)?.pair.to.head_oid.clone())
    }

    /// Whether file `file`'s resident content shows only what changed (diffed from a replay).
    #[must_use]
    pub(super) fn shows_changes(&self, file: usize) -> bool {
        self.files[file].content_base.is_some()
    }

    /// Whether `file` shows only what changed since its source's baseline.
    #[must_use]
    pub fn file_shows_changes(&self, file: &FileKey) -> bool {
        self.file_index
            .get(file)
            .is_some_and(|index| self.shows_changes(*index))
    }

    /// File `file`'s marks, once loaded.
    pub(super) fn file_marks(&self, file: usize) -> Option<&Marks> {
        match &self.files[file].marks {
            MarksState::Loaded(marks) if !self.shows_changes(file) => Some(marks),
            _ => None,
        }
    }

    /// The marks request file `file` needs now: in the full view, for a changed file of a layer
    /// whose rebase was taken out, once its content is resident.
    pub(super) fn marks_request(&self, file: usize) -> Option<ReviewCompareMarksRequest> {
        let state = &self.files[file];
        if !matches!(state.marks, MarksState::Missing)
            || state.loaded().is_none()
            || state.content_base.is_some()
            || !state.expandable()
        {
            return None;
        }
        let group = state.group;
        if self.file_view(file)? != CompareView::Full {
            return None;
        }
        let delta = self.file_delta(file)?;
        if delta.kind != DeltaKind::Changed || !self.since_of(file)?.changed() {
            return None;
        }
        let fork = delta.pair.to.fork_oid.clone()?;
        // Marks number old-side lines from the fork point: the source must diff from it.
        if self.groups[group]
            .source
            .base
            .commit_oid()
            .is_some_and(|base| base != fork)
        {
            return None;
        }
        Some(ReviewCompareMarksRequest {
            repo_path: self.repo_path.clone(),
            fork_oid: fork,
            replay_oid: delta.replay_oid.clone()?,
            head_oid: delta.pair.to.head_oid.clone(),
            path: state.entry.path.clone(),
            old_path: state.entry.old_path.clone(),
        })
    }

    /// Whether the removed-since marker of `file` before `before` shows its text.
    pub(super) fn removal_expanded(&self, file: &FileKey, before: u32) -> bool {
        self.expanded_removals.contains(&(file.clone(), before))
    }
}
