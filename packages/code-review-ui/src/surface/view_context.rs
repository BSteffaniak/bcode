//! The review on screen as the assistant sees it ([`crate::assistant::context::ReviewView`]):
//! read from the review's live state when a message is sent or the context tool is called.

use super::ReviewSurface;
use crate::assistant::context::{
    ComparisonView, CursorView, FileDelta, FileView, FocusedThread, LayerView, Listed, PullView,
    PushView, ReviewView, SourceResult, SourceView, StackView, ThreadsView,
};
use bcode_code_review::document::{AnchorTarget, CompareView, ItemKey, ReviewItem};
use bcode_code_review_models::{
    CheckState, DeltaKind, FileSince, PullState, ReviewCompareBaseline, ReviewFileStatus,
    ReviewLineSide, ReviewSourceKind, ReviewState,
};
use std::collections::BTreeMap;

/// Sources described at most.
const MAX_SOURCES: usize = 200;
/// Selected lines described at most.
const MAX_SELECTION_LINES: usize = 400;
/// Threads on the file described at most.
const MAX_THREADS: usize = 200;
/// Comments of the focused thread described at most (the first and the latest).
const MAX_COMMENTS: usize = 12;
/// Bytes of one comment of the focused thread.
const MAX_COMMENT_BYTES: usize = 1_024;
/// Files of a layer's comparison described at most.
const MAX_DELTA_FILES: usize = 400;
/// The stack's newest pushes listed for the comparison.
const MAX_PUSHES: usize = 20;

impl ReviewSurface {
    /// The review on screen, for the assistant.
    #[must_use]
    pub fn view_context(&self) -> ReviewView {
        let workspace = self.workspace();
        let scope_id = workspace
            .map(bcode_code_review::scope::annotation_workspace_id)
            .unwrap_or_default();
        let mut view = ReviewView {
            id: workspace
                .filter(|workspace| !bcode_code_review::scope::is_transient(workspace))
                .map(|workspace| workspace.id.clone()),
            scope_id,
            title: self.title.clone(),
            kind: review_kind(&self.sources),
            sources: self.source_views(),
            loading: self.review.is_none(),
            ..ReviewView::default()
        };
        let Some(review) = self.review.as_ref() else {
            return view;
        };
        let doc = &review.doc;
        view.files = doc.groups().fold((0, 0), |(files, viewed), group| {
            (files + group.file_count, viewed + group.viewed)
        });
        if self.steps_through_stack() {
            view.stack = Some(StackView {
                layers: self.stack_layer_numbers(),
                current: self.current_layer_number(),
                view: match self.shown_stack_view().unwrap_or_default() {
                    super::stack::StackView::OneLayer => "one_pr",
                    super::stack::StackView::AllLayers => "all_prs",
                    super::stack::StackView::Whole => "whole",
                }
                .to_owned(),
            });
        }
        view.comparison = self.comparison_view();
        let cursor_item = review
            .cursor
            .as_ref()
            .and_then(|_| doc.item(review.cursor_index));
        if let Some(file) = cursor_item.and_then(ReviewItem::file) {
            self.describe_file(file, &mut view);
        }
        self.describe_cursor(&mut view);
        if let Some(ReviewItem::Thread(thread)) = cursor_item {
            view.threads.focused = self.focused_thread(&thread.thread_id);
        }
        view.pull_request = self.pull_view(view.stack.as_ref().and_then(|stack| stack.current));
        view.guide = self.guide_view();
        view
    }

    /// The file the cursor is in, and the threads on it.
    fn describe_file(&self, file: &bcode_code_review::document::FileRef, view: &mut ReviewView) {
        let Some(doc) = self.review.as_ref().map(|review| &review.doc) else {
            return;
        };
        view.file = Some(FileView {
            path: file.path.clone(),
            source_id: file.source_id.clone(),
            status: doc
                .entry(file)
                .map_or("unknown", |entry| status_name(entry.status))
                .to_owned(),
            viewed: match doc.viewed_state(file) {
                Some(state) if state.is_viewed() => "yes",
                _ => "no",
            }
            .to_owned(),
            since: view
                .comparison
                .as_ref()
                .and_then(|_| doc.file_since(file))
                .map(since_text),
        });
        let (threads, omitted) = self.other_threads(&file.path, None);
        let total = threads.len() + omitted;
        let (open, resolved) = self.thread_counts(&file.path);
        view.threads = ThreadsView {
            open,
            resolved,
            on_file: Listed {
                total,
                omitted,
                items: threads.into_iter().take(MAX_THREADS).collect(),
            },
            focused: None,
        };
    }

    /// The cursor's place, side and selected range, with the selected lines.
    fn describe_cursor(&self, view: &mut ReviewView) {
        let (Some(review), Some(place)) = (self.review.as_ref(), self.assistant_place()) else {
            return;
        };
        let (line, side) = match review.cursor.as_ref() {
            Some(ItemKey::Line { side, line, .. }) => (Some(*line), Some(*side)),
            Some(ItemKey::LinePair { new, old, .. }) => new.map_or_else(
                || (*old, old.map(|_| ReviewLineSide::Old)),
                |new| (Some(new), Some(ReviewLineSide::New)),
            ),
            _ => (None, None),
        };
        let anchor = review.selection.as_ref().and_then(|_| self.cursor_anchor());
        let range = anchor.as_ref().and_then(|anchor| match &anchor.target {
            AnchorTarget::Lines(lines) => lines.new.or(lines.old),
            _ => None,
        });
        view.cursor = Some(CursorView {
            place,
            line,
            side: side.map(|side| match side {
                ReviewLineSide::Old => "old".to_owned(),
                ReviewLineSide::New => "new".to_owned(),
            }),
            range,
        });
        if let Some(anchor) = anchor.filter(|_| range.is_some()) {
            let lines = self.anchor_lines(&anchor);
            view.selection = Some(Listed::of(lines, MAX_SELECTION_LINES));
        }
    }

    /// The thread `thread_id` the cursor is on: its first comment and its latest ones.
    fn focused_thread(&self, thread_id: &str) -> Option<FocusedThread> {
        let asked = self.ask_thread(thread_id)?;
        let comments: Vec<String> = asked
            .comments
            .iter()
            .map(|comment| {
                let body: String = comment.body.chars().take(MAX_COMMENT_BYTES).collect();
                format!("{}: {body}", comment.author)
            })
            .collect();
        let total = comments.len();
        let items: Vec<String> = if total > MAX_COMMENTS {
            comments
                .iter()
                .take(1)
                .chain(comments.iter().skip(total - (MAX_COMMENTS - 1)))
                .cloned()
                .collect()
        } else {
            comments
        };
        Some(FocusedThread {
            id: asked.id,
            origin: asked.origin,
            resolved: asked.resolved,
            comments: Listed {
                omitted: total - items.len(),
                total,
                items,
            },
        })
    }

    fn source_views(&self) -> Listed<SourceView> {
        let heads: BTreeMap<&str, String> = self
            .review
            .as_ref()
            .map(|review| {
                review
                    .doc
                    .groups()
                    .filter_map(|group| {
                        Some((
                            group.source.source_id.as_str(),
                            group.source.head.commit_oid()?.to_owned(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Listed::of(
            self.sources.iter().map(|source| {
                let (number, branch) = match &source.kind {
                    ReviewSourceKind::PullRequest { number, .. }
                    | ReviewSourceKind::StackLayer { number, .. } => (Some(*number), None),
                    ReviewSourceKind::Stack { top, .. } => (Some(*top), None),
                    ReviewSourceKind::BranchCompare { head_branch, .. } => {
                        (None, Some(head_branch.clone()))
                    }
                    _ => (None, None),
                };
                SourceView {
                    id: source.id.clone(),
                    label: source.label.clone(),
                    kind: source_kind(&source.kind).to_owned(),
                    number,
                    branch,
                    head: heads.get(source.id.as_str()).cloned(),
                    included: source.included,
                }
            }),
            MAX_SOURCES,
        )
    }

    /// Open and resolved threads (local and the pull requests') on `path`.
    fn thread_counts(&self, path: &str) -> (usize, usize) {
        let mut seen = std::collections::BTreeSet::new();
        let (mut open, mut resolved) = (0, 0);
        for draft in self.notes.drafts() {
            if draft.anchor.file_path != path || !seen.insert(draft.thread_id.clone()) {
                continue;
            }
            let comments = self.notes.thread(&draft.thread_id);
            if comments
                .iter()
                .all(|comment| comment.resolved_at_ms.is_some())
            {
                resolved += 1;
            } else {
                open += 1;
            }
        }
        (open, resolved)
    }

    /// The comparison as the user chose it, with each layer's result.
    fn comparison_view(&self) -> Option<ComparisonView> {
        let review = self.review.as_ref()?;
        let doc = &review.doc;
        let comparing = &self.comparing;
        let layers = self.layer_sources();
        if layers.is_empty() && !comparing.active() {
            return None;
        }
        let name = baseline_name(comparing);
        let mut hidden_files = 0;
        for file in doc.file_keys() {
            if doc.file_compare_hidden(file) {
                hidden_files += 1;
            }
        }
        let layers = layers
            .iter()
            .map(|layer| {
                let comparison = doc.comparison(&layer.source_id);
                let delta = comparison.and_then(|comparison| comparison.delta.as_ref());
                LayerView {
                    number: layer.number,
                    state: match (comparison, delta) {
                        (None, _) => "not_compared".to_owned(),
                        (Some(_), None) => "computing".to_owned(),
                        (Some(_), Some(delta)) => delta_name(delta.kind).to_owned(),
                    },
                    from: delta.and_then(|delta| {
                        delta.pair.from.as_ref().map(|from| from.head_oid.clone())
                    }),
                    to: delta.map_or_else(
                        || layer.head.clone(),
                        |delta| delta.pair.to.head_oid.clone(),
                    ),
                    files: delta
                        .map(|delta| {
                            delta
                                .files
                                .iter()
                                .take(MAX_DELTA_FILES)
                                .map(|file| file_delta(&file.path, file.since))
                                .collect()
                        })
                        .unwrap_or_default(),
                    files_omitted: delta
                        .map_or(0, |delta| delta.files.len().saturating_sub(MAX_DELTA_FILES)),
                    hidden: doc.source_filtered(&layer.source_id),
                    version: comparison
                        .filter(|comparison| !comparison.steps.is_empty())
                        .map(|comparison| {
                            let total = comparison.steps.len() + 1;
                            let current = if comparison.current_version == 0 {
                                total
                            } else {
                                comparison.current_version
                            };
                            (current, total)
                        }),
                }
            })
            .collect();
        let sources = self.source_results();
        let pushes = self.recent_pushes();
        Some(ComparisonView {
            baseline: name,
            view: match comparing.view() {
                CompareView::Full => "full",
                CompareView::Changes => "changes",
            }
            .to_owned(),
            only_changed: comparing.only_changed(),
            marks: comparing.marks().map(|marks| format!("{marks:?}")),
            layers,
            sources,
            pushes,
            hidden_files,
        })
    }

    /// Results of the compared sources that are not pull request layers.
    fn source_results(&self) -> Vec<SourceResult> {
        let Some(review) = self.review.as_ref() else {
            return Vec::new();
        };
        let doc = &review.doc;
        let layer_ids: Vec<String> = self
            .layer_sources()
            .into_iter()
            .map(|layer| layer.source_id)
            .collect();
        self.sources
            .iter()
            .filter(|source| source.included && !layer_ids.contains(&source.id))
            .filter_map(|source| {
                let comparison = doc.comparison(&source.id)?;
                let delta = comparison.delta.as_ref();
                Some(SourceResult {
                    id: source.id.clone(),
                    label: source.label.clone(),
                    state: delta.map_or_else(
                        || "computing".to_owned(),
                        |delta| delta_name(delta.kind).to_owned(),
                    ),
                    from: delta.and_then(|delta| {
                        delta.pair.from.as_ref().map(|from| from.head_oid.clone())
                    }),
                    to: delta.map_or_else(String::new, |delta| delta.pair.to.head_oid.clone()),
                    files: delta
                        .map(|delta| {
                            delta
                                .files
                                .iter()
                                .take(MAX_DELTA_FILES)
                                .map(|file| file_delta(&file.path, file.since))
                                .collect()
                        })
                        .unwrap_or_default(),
                    files_omitted: delta
                        .map_or(0, |delta| delta.files.len().saturating_sub(MAX_DELTA_FILES)),
                })
            })
            .collect()
    }

    /// The stack's newest pushes, once its history is loaded.
    fn recent_pushes(&self) -> Option<Listed<PushView>> {
        self.stack_history.as_ref().map(|history| {
            let total = history.pushes.len();
            let first = total.saturating_sub(MAX_PUSHES);
            Listed {
                items: history
                    .pushes
                    .iter()
                    .enumerate()
                    .skip(first)
                    .map(|(index, push)| PushView {
                        number: index + 1,
                        at_ms: push.at_ms,
                        summary: bcode_code_review::history::push_summary(push),
                    })
                    .collect(),
                total,
                omitted: first,
            }
        })
    }

    /// Loaded pull request state of layer (or pull request) `number`, else of the review's
    /// only pull request.
    fn pull_view(&self, number: Option<u64>) -> Option<PullView> {
        let layers = self.layer_sources();
        let layer = number
            .and_then(|number| layers.iter().find(|layer| layer.number == number))
            .or_else(|| (layers.len() == 1).then(|| &layers[0]))?;
        let activity = self.talk.data.activity(&crate::conversation::PullKey {
            provider: layer.provider.clone(),
            repository: layer.repository.clone(),
            number: layer.number,
        })?;
        // The latest review of each reviewer counts.
        let mut latest: BTreeMap<&str, ReviewState> = BTreeMap::new();
        for review in &activity.reviews {
            if matches!(
                review.state,
                ReviewState::Approved | ReviewState::ChangesRequested | ReviewState::Dismissed
            ) {
                latest.insert(review.author.login.as_str(), review.state);
            }
        }
        let checks = &activity.checks;
        let failing: Vec<String> = checks
            .checks
            .iter()
            .filter(|check| check.state == CheckState::Failing)
            .map(|check| check.name.clone())
            .collect();
        let checks_text = if checks.checks.is_empty() {
            "none".to_owned()
        } else if !failing.is_empty() {
            format!("failing {}/{}", failing.len(), checks.checks.len())
        } else if checks.count(CheckState::Pending) > 0 {
            "pending".to_owned()
        } else {
            "passing".to_owned()
        };
        Some(PullView {
            number: activity.number,
            state: match activity.state {
                PullState::Open => "open",
                PullState::Draft => "draft",
                PullState::Merged => "merged",
                PullState::Closed => "closed",
            }
            .to_owned(),
            approvals: latest
                .values()
                .filter(|state| **state == ReviewState::Approved)
                .count(),
            changes_requested: latest
                .values()
                .filter(|state| **state == ReviewState::ChangesRequested)
                .count(),
            checks: checks_text,
            failing,
        })
    }
}

/// The baseline as the comparison names it.
fn baseline_name(comparing: &super::compare::Comparing) -> String {
    use std::fmt::Write as _;
    let chosen = || match comparing.baseline() {
        ReviewCompareBaseline::Off => "off".to_owned(),
        ReviewCompareBaseline::LastLooked => "last_looked".to_owned(),
        ReviewCompareBaseline::PreviousPush => "previous_push".to_owned(),
        ReviewCompareBaseline::Between { from_ms, to_ms } => format!(
            "between={}..{}",
            from_ms,
            to_ms.map_or_else(|| "now".to_owned(), |to| to.to_string())
        ),
    };
    let mut name = comparing.stack_at().map_or_else(
        || {
            comparing
                .ago()
                .map_or_else(chosen, |ago| format!("ago={}m", ago / 60_000))
        },
        |at| format!("stack_at={at}"),
    );
    for (number, pair) in comparing.picks() {
        let _ = write!(
            name,
            " versions=#{number}:{}..{}",
            pair.from
                .as_ref()
                .map_or("none", |from| short(&from.head_oid)),
            short(&pair.to.head_oid)
        );
    }
    name
}

use bcode_code_review::assistant::manage::{review_kind, source_kind_name as source_kind};

const fn status_name(status: ReviewFileStatus) -> &'static str {
    match status {
        ReviewFileStatus::Modified => "modified",
        ReviewFileStatus::Added => "added",
        ReviewFileStatus::Deleted => "deleted",
        ReviewFileStatus::Renamed => "renamed",
        ReviewFileStatus::Copied => "copied",
        ReviewFileStatus::Unknown => "unknown",
    }
}

const fn delta_name(kind: DeltaKind) -> &'static str {
    match kind {
        DeltaKind::Unchanged => "unchanged",
        DeltaKind::RebasedOnly => "rebased_only",
        DeltaKind::Changed => "changed",
        DeltaKind::NewToYou => "new_to_you",
        DeltaKind::WithRebase => "with_rebase",
        DeltaKind::Unavailable => "unavailable",
    }
}

fn file_delta(path: &str, since: FileSince) -> FileDelta {
    let (name, added, removed) = match since {
        FileSince::Unchanged => ("unchanged", 0, 0),
        FileSince::Changed { added, removed } => ("changed", added, removed),
        FileSince::New { added, removed } => ("new", added, removed),
        FileSince::Dropped => ("dropped", 0, 0),
        FileSince::Unknown => ("unknown", 0, 0),
    };
    FileDelta {
        path: path.to_owned(),
        since: name.to_owned(),
        added,
        removed,
    }
}

fn since_text(since: FileSince) -> String {
    match since {
        FileSince::Changed { added, removed } => format!("changed +{added} −{removed}"),
        FileSince::New { added, removed } => format!("new +{added} −{removed}"),
        FileSince::Unchanged => "unchanged".to_owned(),
        FileSince::Dropped => "dropped".to_owned(),
        FileSince::Unknown => "computing".to_owned(),
    }
}

fn short(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::ReviewSource;

    fn source(kind: ReviewSourceKind) -> ReviewSource {
        ReviewSource {
            id: "s".to_owned(),
            label: "s".to_owned(),
            kind,
            included: true,
        }
    }

    #[test]
    fn kinds_follow_the_sources() {
        let layer = |number| ReviewSourceKind::StackLayer {
            provider: "p".to_owned(),
            repository: "r".to_owned(),
            number,
        };
        assert_eq!(
            review_kind(&[source(layer(1)), source(layer(2))]),
            "stack_step_through"
        );
        assert_eq!(review_kind(&[source(layer(1))]), "pull_request");
        assert_eq!(
            review_kind(&[source(ReviewSourceKind::IndexStaged)]),
            "staged"
        );
        assert_eq!(
            review_kind(&[
                source(ReviewSourceKind::IndexStaged),
                source(ReviewSourceKind::LastCommit)
            ]),
            "mixed"
        );
    }
}
