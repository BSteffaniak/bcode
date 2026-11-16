//! Comparison overlays: how a source's files changed since a baseline, shown on the same
//! document (see `docs/review-stack-history.md`).
//!
//! A comparison never changes the document's sources, files, or keys. It only changes how files
//! show: which are folded or hidden, which lines are marked new since the baseline, and, in the
//! "only what changed" view, which commit a changed file's hunks are diffed from (the baseline's
//! change replayed on today's base). Viewed state always uses each file's own identity, so it
//! reads the same in both views.

use super::items::{CompareView, FileVersionsItem};
use super::keys::FileKey;
use super::review::{Pending, ReviewDocument, Ticket};
use bcode_code_review_models::{
    DeltaKind, FileSince, LayerDelta, LineSpan, RemovedSince, ReviewCompareMarksRequest,
    ReviewCompareMarksResponse,
};
use std::collections::{BTreeMap, BTreeSet};
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
    /// The layer's version steps, oldest first: each pair of consecutive available versions
    /// compared. File headers say in which steps their file changed, and a file can be stepped
    /// to one.
    pub steps: Vec<VersionStep>,
    /// The number (1-based, as the layer's version list counts them) of the version the source
    /// shows; `0` when not known (the newest step's version is assumed).
    pub current_version: usize,
}

/// Two consecutive available versions of a layer compared.
///
/// Versions are numbered as the layer's version list counts them (1-based, unavailable versions
/// included), so numbers stay truthful when a version between others cannot be had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionStep {
    /// The older version's number.
    pub from: usize,
    /// The newer version's number.
    pub to: usize,
    /// The comparison from the older version to the newer.
    pub delta: LayerDelta,
}

/// What a source's comparison found, summed once when it is set: lines added and removed
/// since the baseline, and files that changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ComparisonSummary {
    /// Lines added since the baseline.
    pub added: u64,
    /// Lines removed since the baseline.
    pub removed: u64,
    /// Files changed since the baseline.
    pub files: usize,
}

impl ComparisonSummary {
    /// The summary of `delta`.
    #[must_use]
    pub fn of(delta: &LayerDelta) -> Self {
        let (added, removed) = delta.totals();
        Self {
            added,
            removed,
            files: delta
                .files
                .iter()
                .filter(|file| file.since.changed())
                .count(),
        }
    }
}

/// One file's lines new since the baseline.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Marks {
    pub new_lines: Vec<LineSpan>,
    pub newly_removed: Vec<LineSpan>,
    pub removed_since: Vec<RemovedSince>,
    /// Baseline text of new lines that replaced a line there, sorted by new-side line.
    pub was: Vec<(u32, Arc<str>)>,
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

    /// The baseline line new-side line `line` replaced, when it replaced one.
    pub fn was(&self, line: u32) -> Option<&Arc<str>> {
        self.was
            .binary_search_by_key(&line, |(at, _)| *at)
            .ok()
            .map(|index| &self.was[index].1)
    }

    /// Lines removed since the baseline whose markers stand within new-side lines
    /// `start..=end`.
    pub fn removed_within(&self, start: u32, end: u32) -> usize {
        self.removed_since
            .iter()
            .filter(|removal| (start..=end).contains(&removal.before_new_line))
            .map(|removal| removal.lines.len())
            .sum()
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
        let summary = comparison
            .as_ref()
            .and_then(|comparison| comparison.delta.as_ref())
            .map(ComparisonSummary::of);
        match summary {
            Some(summary) => self
                .comparison_summaries
                .insert(source_id.to_owned(), summary),
            None => self.comparison_summaries.remove(source_id),
        };
        match comparison {
            Some(comparison) => self.comparisons.insert(source_id.to_owned(), comparison),
            None => self.comparisons.remove(source_id),
        };
        self.reapply(source_id, marks_stale);
        true
    }

    /// What source `source_id`'s comparison found, once its delta is in (summed when the
    /// comparison was set, so painting badges every frame costs nothing per file).
    #[must_use]
    pub fn comparison_summary(&self, source_id: &str) -> Option<ComparisonSummary> {
        self.comparison_summaries.get(source_id).copied()
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

    /// Whether the comparison hides `file` now (an unchanged file in the changes view, or any
    /// file of a layer only rebased, until its source's hidden files are shown).
    #[must_use]
    pub fn file_compare_hidden(&self, file: &FileKey) -> bool {
        self.file_index
            .get(file)
            .is_some_and(|index| self.compare_hidden(*index))
    }

    /// Whether the comparison would hide `file` were its source's hidden files not shown.
    #[must_use]
    pub fn file_compare_hideable(&self, file: &FileKey) -> bool {
        self.file_index
            .get(file)
            .is_some_and(|index| self.hidden_by_comparison(*index))
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

    /// Fold hunks with nothing new since their source's baseline to their header (or show them
    /// again). Returns whether anything changed.
    pub fn set_fold_seen_hunks(&mut self, fold: bool) -> bool {
        if self.fold_seen_hunks == fold {
            return false;
        }
        self.fold_seen_hunks = fold;
        let marked: Vec<usize> = (0..self.files.len())
            .filter(|file| self.file_marks(*file).is_some())
            .collect();
        for file in marked {
            self.rebuild_file(file);
        }
        true
    }

    /// Take over from `previous`, the document this one replaces, whether hunks seen before
    /// fold, the folded hunks shown anyway, and the removed-since markers opened, for the files
    /// both have.
    pub fn carry_compare_choices(&mut self, previous: &Self) {
        let kept = |entries: &BTreeSet<(FileKey, u32)>| -> BTreeSet<(FileKey, u32)> {
            entries
                .iter()
                .filter(|(file, _)| self.file_index.contains_key(file))
                .cloned()
                .collect()
        };
        let revealed = kept(&previous.revealed_hunks);
        let removals = kept(&previous.expanded_removals);
        if self.fold_seen_hunks == previous.fold_seen_hunks
            && revealed == self.revealed_hunks
            && removals == self.expanded_removals
        {
            return;
        }
        self.fold_seen_hunks = previous.fold_seen_hunks;
        self.revealed_hunks = revealed;
        self.expanded_removals = removals;
        let marked: Vec<usize> = (0..self.files.len())
            .filter(|file| self.file_marks(*file).is_some())
            .collect();
        for file in marked {
            self.rebuild_file(file);
        }
    }

    /// Whether hunks with nothing new since the baseline fold.
    #[must_use]
    pub const fn fold_seen_hunks(&self) -> bool {
        self.fold_seen_hunks
    }

    /// Show the folded hunk of `file` starting at new-side line `new_start` (or fold it again).
    /// Returns whether the file has such a hunk: one with nothing new since the baseline,
    /// while such hunks fold, that nothing anchored in it holds open.
    pub fn toggle_seen_hunk(&mut self, file: &FileKey, new_start: u32) -> bool {
        if !self.fold_seen_hunks {
            return false;
        }
        let Some(&index) = self.file_index.get(file) else {
            return false;
        };
        let foldable = self.blocks[self.files[index].block]
            .items
            .iter()
            .any(|item| match item {
                super::items::ReviewItem::HunkHeader(hunk) => {
                    hunk.new_start == new_start && hunk.seen() && !hunk.held_open
                }
                _ => false,
            });
        if !foldable {
            return false;
        }
        let key = (file.clone(), new_start);
        if !self.revealed_hunks.remove(&key) {
            self.revealed_hunks.insert(key);
        }
        self.rebuild_file(index);
        true
    }

    /// Whether the hunk of `file` starting at new-side line `new_start` is shown although it
    /// would fold.
    pub(super) fn hunk_revealed(&self, file: &FileKey, new_start: u32) -> bool {
        self.revealed_hunks.contains(&(file.clone(), new_start))
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
                was: response
                    .was
                    .into_iter()
                    .map(|(line, text)| (line, Arc::from(text)))
                    .collect(),
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

    /// File `file`'s versions in its layer, once the layer's steps are known.
    pub(super) fn file_versions(&self, file: usize) -> Option<FileVersionsItem> {
        let comparison = self.group_comparison(self.files[file].group)?;
        let steps = &comparison.steps;
        if steps.is_empty() {
            return None;
        }
        let path = &self.files[file].entry.path;
        let changed: Vec<usize> = (0..steps.len())
            .filter(|index| steps[*index].delta.file(path).changed())
            .collect();
        let shown = comparison
            .files
            .get(path)
            .and_then(|delta| steps.iter().position(|step| step.delta.pair == delta.pair));
        let before = shown.unwrap_or(steps.len());
        let newest = steps.last().map_or(0, |step| step.to);
        let version = shown.map_or(
            if comparison.current_version > 0 {
                comparison.current_version
            } else {
                newest
            },
            |index| steps[index].to,
        );
        Some(FileVersionsItem {
            changed_in: changed.len(),
            steps: steps.len(),
            version,
            shown,
            previous: changed.iter().any(|index| *index < before),
            next: shown.is_some(),
        })
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

/// Uncommitted-change marks: a browse source's working-tree files mark their lines changed since
/// `HEAD`, through the same per-file marks as a comparison.
impl ReviewDocument {
    /// The worktree marks request file `file` needs now: a resident working-tree file of a
    /// browse source, while uncommitted changes are marked.
    pub(super) fn worktree_marks_request(
        &self,
        file: usize,
    ) -> Option<bcode_code_review_models::ReviewWorktreeMarksRequest> {
        let state = &self.files[file];
        let source = &self.groups[state.group].source;
        (self.config.uncommitted_marks
            && matches!(state.marks, MarksState::Missing)
            && state.loaded().is_some()
            && state.expandable()
            && source.mode == bcode_code_review_models::ResolvedSourceMode::Browse
            && matches!(
                source.head,
                bcode_code_review_models::RevisionRef::WorkingTree
            ))
        .then(|| bcode_code_review_models::ReviewWorktreeMarksRequest {
            repo_path: self.repo_path.clone(),
            paths: vec![state.entry.path.clone()],
        })
    }

    /// Accept a working-tree file's marks. Returns whether the result was applied.
    pub fn accept_worktree_marks(
        &mut self,
        ticket: Ticket,
        response: bcode_code_review_models::ReviewWorktreeMarksResponse,
    ) -> bool {
        let Some(Pending::Marks(file)) = self.pending.get(&ticket).copied() else {
            return false;
        };
        self.pending.remove(&ticket);
        let path = self.files[file].entry.path.clone();
        let found = response.files.into_iter().find(|marks| marks.path == path);
        let (marks, counts) = match found {
            Some(found) if !found.too_large && !found.untracked && !found.missing => (
                Marks {
                    new_lines: found.added,
                    newly_removed: Vec::new(),
                    removed_since: found.removed,
                    was: found
                        .was
                        .into_iter()
                        .map(|(line, text)| (line, Arc::from(text)))
                        .collect(),
                },
                Some(super::items::Uncommitted {
                    additions: found.additions,
                    deletions: found.deletions,
                }),
            ),
            _ => (Marks::default(), None),
        };
        self.files[file].marks = MarksState::Loaded(Arc::new(marks));
        self.files[file].uncommitted = counts;
        self.rebuild_file(file);
        true
    }

    /// Mark (or stop marking) the lines working-tree files changed since `HEAD`. Returns
    /// whether anything changed.
    pub fn set_uncommitted_marks(&mut self, marks: bool) -> bool {
        if self.config.uncommitted_marks == marks {
            return false;
        }
        self.config.uncommitted_marks = marks;
        let browse: Vec<usize> = (0..self.files.len())
            .filter(|file| {
                self.groups[self.files[*file].group].source.mode
                    == bcode_code_review_models::ResolvedSourceMode::Browse
            })
            .collect();
        for file in browse {
            if let MarksState::Pending(ticket) = self.files[file].marks {
                self.pending.remove(&ticket);
            }
            self.files[file].marks = MarksState::Missing;
            self.files[file].uncommitted = None;
            self.rebuild_file(file);
        }
        true
    }

    /// Whether working-tree files mark their lines changed since `HEAD`.
    #[must_use]
    pub const fn uncommitted_marks(&self) -> bool {
        self.config.uncommitted_marks
    }
}
