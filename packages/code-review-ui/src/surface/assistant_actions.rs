//! What the review assistant's app tools do in the review on screen.
//!
//! Each request runs the review's own actions (the ones its keys and palette run), so the
//! screen changes exactly as if the reviewer had pressed them, and checks first what the keys
//! would refuse. A request answers with what it did, or why it did nothing.
//!
//! * [`GoTo`]: a stack layer, a file and line, or a thread of the review on screen.
//! * [`ComparisonChange`]: the Compare menu's choices: baseline, view, the "only pull requests
//!   with real changes" filter, files the comparison hides, one pull request or all of them,
//!   versions of one pull request, and how new lines are marked.
//! * [`ViewChange`]: layout, wrapping, hidden resolved threads and viewed files, the file list,
//!   panels, and folding.
//! * [`ReviewCommand`]: refresh, complete the pass, and open the publish and pull request
//!   dialogs (the reviewer confirms those on screen).

use super::ReviewSurface;
use super::stack::StackView;
use crate::config::compare::{MarkStyle, SeenHunks};
use crate::context::UiContext;
use bcode_code_review::document::ItemKey;
use bcode_code_review_models::ReviewCompareBaseline;
use serde::Deserialize;

/// A place in the review on screen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GoTo {
    /// The review the place is in; must be the one on screen when given.
    pub review: Option<String>,
    /// A stack layer: its pull request number (`102`, `"#102"`) or source id.
    pub layer: Option<serde_json::Value>,
    /// A file path.
    pub path: Option<String>,
    /// A line of the file.
    pub line: Option<u32>,
    /// The side the line is on: `new` (default) or `old`.
    pub side: Option<String>,
    /// The source the file is in, when several have it.
    pub source: Option<String>,
    /// A thread id.
    pub thread: Option<String>,
}

impl GoTo {
    /// Whether the request names a place.
    #[must_use]
    pub const fn names_a_place(&self) -> bool {
        self.layer.is_some() || self.path.is_some() || self.thread.is_some()
    }
}

/// A change to the comparison, as the Compare menu makes it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ComparisonChange {
    /// `off` (the full change), `last_looked`, `previous_push`, `ago` (with `ago_minutes`) or
    /// `between` (with `from_ms` and optionally `to_ms`).
    pub baseline: Option<String>,
    /// Minutes before now, for `ago`.
    pub ago_minutes: Option<u64>,
    /// From, for `between`, in milliseconds since the Unix epoch.
    pub from_ms: Option<u64>,
    /// To, for `between` (now when absent).
    pub to_ms: Option<u64>,
    /// `full` (the full change with what is new marked) or `changes` (only what changed).
    pub view: Option<String>,
    /// Only pull requests with real changes since the baseline.
    pub only_changed: Option<bool>,
    /// Show the files the comparison hides (unchanged since the baseline).
    pub show_hidden_files: Option<bool>,
    /// Every pull request of the stack at once (`false`: one at a time).
    pub all_layers: Option<bool>,
    /// Compare one pull request between two of its versions (1-based: v1 is the first).
    pub versions: Option<VersionPick>,
    /// How new lines are marked: `both`, `bar`, `tint`, `strong`.
    pub mark_style: Option<String>,
    /// Whether changed words are marked.
    pub word_marks: Option<bool>,
    /// How hunks with nothing new show: `show`, `dim`, `fold`.
    pub seen_hunks: Option<String>,
    /// Whether removed code shows inline.
    pub removed_inline: Option<bool>,
}

/// Versions of one pull request to compare.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionPick {
    /// Pull request number.
    pub number: u64,
    /// From version (1-based).
    pub from: usize,
    /// To version (1-based); the latest when absent.
    #[serde(default)]
    pub to: Option<usize>,
}

/// A change to how the review is shown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ViewChange {
    /// `unified` or `side_by_side`.
    pub layout: Option<String>,
    /// Wrap long lines.
    pub wrap: Option<bool>,
    /// Hide resolved threads.
    pub hide_resolved_threads: Option<bool>,
    /// Hide viewed files in the file panel.
    pub hide_viewed_files: Option<bool>,
    /// `tree` or `flat`.
    pub file_list: Option<String>,
    /// Show the file panel.
    pub files_panel: Option<bool>,
    /// Show the threads panel.
    pub threads_panel: Option<bool>,
    /// Fold (`true`) or open (`false`) every file.
    pub fold_files: Option<bool>,
    /// Fold (`true`) or open (`false`) every thread.
    pub fold_threads: Option<bool>,
}

/// A one-shot review command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewCommand {
    /// Check for changes and reload.
    Refresh,
    /// Mark the review pass complete.
    CompletePass,
    /// Open the publish dialog.
    Publish,
    /// Open the pull request approval dialog.
    Approve,
    /// Open the request-changes dialog.
    RequestChanges,
    /// Open the pull request comment dialog.
    Comment,
    /// Open the dialog approving every fully viewed stack layer.
    ApproveViewedLayers,
}

impl ReviewCommand {
    const fn action(self) -> &'static str {
        match self {
            Self::Refresh => "refresh",
            Self::CompletePass => "complete_pass",
            Self::Publish => "publish",
            Self::Approve => "approve_pull_request",
            Self::RequestChanges => "request_changes",
            Self::Comment => "pull_request_comment",
            Self::ApproveViewedLayers => "approve_reviewed_layers",
        }
    }
}

impl ReviewSurface {
    /// The id the review is saved under (its scope id while unsaved).
    #[must_use]
    pub fn screen_review_id(&self) -> Option<String> {
        self.workspace()
            .map(bcode_code_review::scope::annotation_workspace_id)
    }

    fn status_or(&self, fallback: impl Into<String>) -> String {
        self.status().map_or_else(|| fallback.into(), str::to_owned)
    }

    /// Move to `target` in this review.
    ///
    /// # Errors
    ///
    /// * The target names another review, a layer, file or thread this review does not have,
    ///   or nothing at all.
    pub fn assistant_go_to(&mut self, target: &GoTo) -> Result<String, String> {
        if let Some(review) = target.review.as_deref().filter(|id| !id.is_empty())
            && self.screen_review_id().as_deref() != Some(review)
        {
            return Err(format!(
                "review {review} is not on screen; use bcode.code-review.open_review to show it"
            ));
        }
        if !target.names_a_place() {
            return Err(
                "name a `layer`, a `path` (with an optional `line`) or a `thread`".to_owned(),
            );
        }
        if self.review.is_none() {
            return Err("the review is still loading; try again".to_owned());
        }
        let mut done = Vec::new();
        if let Some(layer) = &target.layer {
            done.push(self.go_to_layer(layer)?);
        }
        if let Some(thread) = target.thread.as_deref() {
            let review = self.review.as_mut().ok_or("the review is not loaded")?;
            let key = ItemKey::Thread {
                thread_id: thread.to_owned(),
            };
            if review.doc.index_of(&key).is_none() && review.doc.jump_to(&key).is_none() {
                return Err(format!(
                    "thread {thread} is not shown in this review (code_review.list_threads lists them)"
                ));
            }
            super::annotate::jump_to_annotation(review, &key);
            done.push(format!("thread {thread}"));
        }
        if let Some(path) = target.path.as_deref() {
            done.push(self.show_location(
                path,
                target.line,
                target.side.as_deref(),
                target.source.as_deref(),
            )?);
        }
        Ok(format!("now showing {}", done.join(", ")))
    }

    fn go_to_layer(&mut self, layer: &serde_json::Value) -> Result<String, String> {
        let wanted = match layer {
            serde_json::Value::Number(number) => number.to_string(),
            serde_json::Value::String(text) => text.trim().trim_start_matches('#').to_owned(),
            _ => return Err("`layer` is a pull request number or a source id".to_owned()),
        };
        let entries = self.rail_entries();
        let position = entries.iter().position(|entry| {
            entry.source_id == wanted
                || self.sources.iter().any(|source| {
                    source.id == entry.source_id
                        && bcode_code_review::assistant::manage::pull_request_numbers(
                            std::slice::from_ref(source),
                        )
                        .iter()
                        .any(|number| number.to_string() == wanted)
                })
        });
        let Some(next) = position else {
            return Err(format!(
                "{wanted} is not a stack layer of this review (bcode.code-review.current_view lists them)"
            ));
        };
        let current = self.current_layer(&entries);
        self.enter_layer(&entries, current, next);
        Ok(format!("layer {wanted}"))
    }

    /// Change the comparison as `change` says.
    ///
    /// # Errors
    ///
    /// * An unknown baseline, view or mark option; a baseline missing its time; versions of a
    ///   pull request the review does not have or whose history is not loaded yet.
    pub fn assistant_set_comparison(
        &mut self,
        change: &ComparisonChange,
        host: &UiContext,
    ) -> Result<String, String> {
        if change == &ComparisonChange::default() {
            return Err("change at least one comparison setting".to_owned());
        }
        let mut done = Vec::new();
        if let Some(baseline) = change.baseline.as_deref() {
            match baseline {
                "off" => self.set_compare_baseline(ReviewCompareBaseline::Off, host),
                "last_looked" => self.set_compare_baseline(ReviewCompareBaseline::LastLooked, host),
                "previous_push" => {
                    self.set_compare_baseline(ReviewCompareBaseline::PreviousPush, host);
                }
                "ago" => {
                    let minutes = change
                        .ago_minutes
                        .filter(|minutes| *minutes > 0)
                        .ok_or("`ago` needs `ago_minutes`")?;
                    self.compare_ago(minutes.saturating_mul(60_000), host);
                }
                "between" => {
                    let from_ms = change.from_ms.ok_or("`between` needs `from_ms`")?;
                    self.set_compare_baseline(
                        ReviewCompareBaseline::Between {
                            from_ms,
                            to_ms: change.to_ms,
                        },
                        host,
                    );
                }
                other => return Err(format!("unknown baseline {other:?}")),
            }
            done.push(self.status_or(format!("baseline {baseline}")));
        }
        if let Some(view) = change.view.as_deref() {
            let wanted = match view {
                "full" => bcode_code_review::document::CompareView::Full,
                "changes" => bcode_code_review::document::CompareView::Changes,
                other => return Err(format!("unknown view {other:?} (full or changes)")),
            };
            if self.comparing.view() != wanted {
                self.toggle_compare_view();
            }
            done.push(format!("view {view}"));
        }
        if let Some(only) = change.only_changed {
            if self.comparing.only_changed() != only {
                self.toggle_only_changed();
            }
            done.push(format!("only pull requests with real changes: {only}"));
        }
        if let Some(shown) = change.show_hidden_files {
            self.show_compare_hidden(shown);
            done.push(format!("hidden files shown: {shown}"));
        }
        if let Some(all) = change.all_layers {
            if !self.steps_through_stack() {
                return Err("only a review that steps through a stack has layers".to_owned());
            }
            if (self.stack_view == StackView::AllLayers) != all {
                self.toggle_stack_view();
            }
            done.push(
                if all {
                    "every pull request"
                } else {
                    "one pull request at a time"
                }
                .to_owned(),
            );
        }
        if let Some(pick) = change.versions {
            done.push(self.pick_versions_for(pick, host)?);
        }
        done.extend(self.change_mark_options(change)?);
        Ok(done.join("; "))
    }

    fn pick_versions_for(&mut self, pick: VersionPick, host: &UiContext) -> Result<String, String> {
        if !self
            .layer_sources()
            .iter()
            .any(|layer| layer.number == pick.number)
        {
            return Err(format!(
                "#{} is not a pull request of this review",
                pick.number
            ));
        }
        let Some(count) = self.stack_history.as_ref().and_then(|history| {
            history
                .layers
                .iter()
                .find(|layer| layer.number == pick.number)
                .map(|layer| layer.versions.len())
        }) else {
            self.load_history_quietly(host);
            return Err(format!(
                "the versions of #{} are loading; call again in a moment",
                pick.number
            ));
        };
        let to = pick.to.unwrap_or(count);
        if pick.from == 0 || to == 0 || pick.from > count || to > count || pick.from > to {
            return Err(format!(
                "#{} has versions v1 to v{count}; pick from <= to within them",
                pick.number
            ));
        }
        self.pick_versions(pick.number, pick.from - 1, to - 1, host);
        Ok(format!("#{} v{} → v{to}", pick.number, pick.from))
    }

    fn change_mark_options(&mut self, change: &ComparisonChange) -> Result<Vec<String>, String> {
        let mut done = Vec::new();
        if let Some(style) = change.mark_style.as_deref() {
            let style = match style {
                "both" => MarkStyle::Both,
                "bar" => MarkStyle::Bar,
                "tint" => MarkStyle::Tint,
                "strong" => MarkStyle::Strong,
                other => return Err(format!("unknown mark style {other:?}")),
            };
            self.change_marks(|marks| marks.style = style);
            done.push("mark style changed".to_owned());
        }
        if let Some(seen) = change.seen_hunks.as_deref() {
            let seen = match seen {
                "show" => SeenHunks::Show,
                "dim" => SeenHunks::Dim,
                "fold" => SeenHunks::Fold,
                other => return Err(format!("unknown seen_hunks {other:?}")),
            };
            self.change_marks(|marks| marks.seen = seen);
            done.push("seen hunks changed".to_owned());
        }
        if let Some(words) = change.word_marks {
            self.change_marks(|marks| marks.words = words);
            done.push(format!("word marks: {words}"));
        }
        if let Some(inline) = change.removed_inline {
            self.change_marks(|marks| marks.removed_inline = inline);
            done.push(format!("removed code inline: {inline}"));
        }
        Ok(done)
    }

    /// Change how the review is shown, as `change` says.
    ///
    /// # Errors
    ///
    /// * An unknown layout or file list, or nothing to change.
    pub fn assistant_set_view(
        &mut self,
        change: &ViewChange,
        host: &UiContext,
    ) -> Result<String, String> {
        if change == &ViewChange::default() {
            return Err("change at least one view setting".to_owned());
        }
        let mut done = Vec::new();
        if let Some(layout) = change.layout.as_deref() {
            let side_by_side = match layout {
                "unified" => false,
                "side_by_side" => true,
                other => return Err(format!("unknown layout {other:?}")),
            };
            let now = self.review.as_ref().is_some_and(|review| {
                review.doc.layout() == bcode_code_review::document::LayoutMode::SideBySide
            });
            if now != side_by_side {
                self.run_action("layout_toggle", None, host);
            }
            done.push(format!("layout {layout}"));
        }
        if let Some(wrap) = change.wrap {
            if (self.overflow == bmux_tui_components::diff_viewer::DiffRowOverflow::Wrap) != wrap {
                self.run_action("wrap_toggle", None, host);
            }
            done.push(format!("wrap {wrap}"));
        }
        if let Some(hide) = change.hide_resolved_threads {
            if self.hide_resolved != hide {
                self.run_action("resolved_toggle", None, host);
            }
            done.push(format!("resolved threads hidden: {hide}"));
        }
        if let Some(hide) = change.hide_viewed_files {
            if self.file_view.hide_viewed != hide {
                self.run_action("viewed_files_toggle", None, host);
            }
            done.push(format!("viewed files hidden: {hide}"));
        }
        if let Some(list) = change.file_list.as_deref() {
            let flat = match list {
                "flat" => true,
                "tree" => false,
                other => return Err(format!("unknown file list {other:?} (tree or flat)")),
            };
            if self.file_view.layout.is_flat() != flat {
                self.run_action("files_layout_toggle", None, host);
            }
            done.push(format!("file list {list}"));
        }
        if let Some(shown) = change.files_panel {
            if self.files_shown() != shown {
                self.run_action("files_toggle", None, host);
            }
            done.push(format!("file panel shown: {shown}"));
        }
        if let Some(shown) = change.threads_panel {
            let now = self
                .panels
                .current()
                .is_some_and(|plan| plan.threads != super::super::panels::SidePlacement::Hidden);
            if now != shown {
                self.run_action("threads_toggle", None, host);
            }
            done.push(format!("threads panel shown: {shown}"));
        }
        if let Some(fold) = change.fold_files {
            self.run_action(if fold { "fold_files" } else { "open_files" }, None, host);
            done.push(if fold { "files folded" } else { "files opened" }.to_owned());
        }
        if let Some(fold) = change.fold_threads {
            self.run_action(
                if fold { "fold_threads" } else { "open_threads" },
                None,
                host,
            );
            done.push(
                if fold {
                    "threads folded"
                } else {
                    "threads opened"
                }
                .to_owned(),
            );
        }
        Ok(done.join("; "))
    }

    /// Run `command` as its key does.
    ///
    /// # Errors
    ///
    /// * The review is still loading, or the command needs pull requests the review does not
    ///   have.
    pub fn assistant_command(
        &mut self,
        command: ReviewCommand,
        host: &UiContext,
    ) -> Result<String, String> {
        if self.review.is_none() {
            return Err("the review is still loading; try again".to_owned());
        }
        let needs_pull = matches!(
            command,
            ReviewCommand::Approve
                | ReviewCommand::RequestChanges
                | ReviewCommand::Comment
                | ReviewCommand::ApproveViewedLayers
        );
        if needs_pull && self.layer_sources().is_empty() {
            return Err("this review has no pull requests".to_owned());
        }
        self.run_action(command.action(), None, host);
        let fallback = match command {
            ReviewCommand::Refresh => "checking for changes",
            ReviewCommand::CompletePass => "marked the review pass complete",
            ReviewCommand::Publish => "opened the publish dialog; the reviewer confirms it",
            _ => "opened the pull request dialog; the reviewer confirms it",
        };
        let status = self.status_or(fallback);
        Ok(if needs_pull || command == ReviewCommand::Publish {
            format!("{status} (the dialog is on screen for the reviewer to confirm)")
        } else {
            status
        })
    }
}
