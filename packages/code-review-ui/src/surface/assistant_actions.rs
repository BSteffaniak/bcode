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
    /// A thread id, as `code_review.list_threads`, `current_view` and the question context
    /// give it (`thread_id` is taken too, as the review tools name it).
    #[serde(alias = "thread_id")]
    pub thread: Option<String>,
    /// A version (1-based) of the layer (`layer`, else the current one) to show: its whole
    /// change as it was then, read-only where it differs from now.
    pub version: Option<usize>,
}

impl GoTo {
    /// The request with blank strings taken as not given.
    #[must_use]
    pub fn without_blanks(&self) -> Self {
        fn given(text: Option<&String>) -> Option<String> {
            text.map(|text| text.trim())
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        }
        Self {
            review: given(self.review.as_ref()),
            layer: self.layer.clone().filter(|layer| match layer {
                serde_json::Value::String(text) => !text.trim().is_empty(),
                serde_json::Value::Null => false,
                _ => true,
            }),
            path: given(self.path.as_ref()),
            line: self.line.filter(|line| *line > 0),
            side: given(self.side.as_ref()),
            source: given(self.source.as_ref()),
            thread: given(self.thread.as_ref()),
            version: self.version.filter(|version| *version > 0),
        }
    }

    /// Whether the request names a place.
    #[must_use]
    pub const fn names_a_place(&self) -> bool {
        self.layer.is_some()
            || self.path.is_some()
            || self.thread.is_some()
            || self.version.is_some()
    }
}

/// A thread a go-to reaches, checked.
#[derive(Debug)]
struct ThreadTarget {
    /// The id the tools use for it.
    tool_id: String,
    /// Where it shows, for the answer.
    shown: String,
    /// Its document key.
    key: ItemKey,
    /// The source (layer) holding it; `None` for the review as a whole.
    source: Option<String>,
    /// The files it is on: where it shows, and where it was written.
    paths: Vec<String>,
    /// Where it shows.
    anchor: Option<bcode_code_review_models::DraftAnchor>,
}

/// What a go-to to a thread does not use, when it names another place than the thread's.
fn thread_mismatch(target: &GoTo, thread: &ThreadTarget) -> Vec<String> {
    let mut notes = Vec::new();
    let path = target
        .path
        .as_deref()
        .map(|path| path.trim_start_matches("./"));
    let other_file = path.is_some_and(|path| !thread.paths.iter().any(|shown| shown == path));
    let other_line = target.line.is_some_and(|line| {
        thread.anchor.as_ref().is_none_or(|anchor| {
            let (start, end) = (
                anchor
                    .new_start
                    .or(anchor.new_line)
                    .or(anchor.old_start)
                    .or(anchor.old_line),
                anchor
                    .new_end
                    .or(anchor.new_line)
                    .or(anchor.old_end)
                    .or(anchor.old_line),
            );
            !matches!((start, end), (Some(start), Some(end)) if (start..=end).contains(&line))
        })
    });
    if other_file || other_line {
        let named = match (path, target.line) {
            (Some(path), Some(line)) => format!("{path}:{line}"),
            (Some(path), None) => path.to_owned(),
            (None, Some(line)) => format!("line {line}"),
            (None, None) => String::new(),
        };
        notes.push(format!(
            "{named} is not where the thread is shown now; went to the thread"
        ));
    }
    if target.version.is_some() {
        notes.push("`version` not used: threads show on the review as it is now".to_owned());
    }
    notes
}

/// A comparison baseline, checked.
#[derive(Debug, Clone, Copy)]
enum BaselinePlan {
    Set(ReviewCompareBaseline),
    Ago(u64),
    StackAt(u64),
}

/// A comparison change, checked before anything is applied.
#[derive(Debug, Default)]
struct ComparisonPlan {
    baseline: Option<(BaselinePlan, String)>,
    view: Option<bcode_code_review::document::CompareView>,
    versions: Option<(u64, usize, usize)>,
    mark_style: Option<MarkStyle>,
    seen: Option<SeenHunks>,
}

/// A view change, checked before anything is applied.
#[derive(Debug, Default)]
struct ViewPlan {
    side_by_side: Option<bool>,
    flat: Option<bool>,
}

/// A change to the comparison, as the Compare menu makes it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ComparisonChange {
    /// `off` (the full change), `last_looked`, `previous_push`, `ago` (with `ago_minutes`),
    /// `between` (with `from_ms` and optionally `to_ms`), `pushes` (with `from_push` and
    /// optionally `to_push`) or `stack_at` (with `at_ms`).
    pub baseline: Option<String>,
    /// Minutes before now, for `ago`.
    pub ago_minutes: Option<u64>,
    /// From, for `between`, in milliseconds since the Unix epoch.
    pub from_ms: Option<u64>,
    /// To, for `between` (now when absent).
    pub to_ms: Option<u64>,
    /// For `pushes`: the first push compared (1-based, oldest first, as `current_view` lists
    /// the stack's pushes): the stack just before it is the baseline.
    pub from_push: Option<usize>,
    /// For `pushes`: the last push compared (1-based); the stack after it is compared (now
    /// when absent).
    pub to_push: Option<usize>,
    /// For `stack_at`: show every layer as it was then (milliseconds since the Unix epoch).
    pub at_ms: Option<u64>,
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
    /// A thread is the place when one is named: it is reached wherever the review shows it
    /// (an outdated pull request thread near its code, on its file or on its layer), entering
    /// its layer when a layer is shown alone; a `path`, `line`, `side`, `source` or `version`
    /// given with it is not used, and the answer says so when it names another place.
    ///
    /// # Errors
    ///
    /// * The target names another review, a layer, file or thread this review does not have,
    ///   or nothing at all.
    /// * The thread or file is hidden by the comparison or by hidden resolved threads (the
    ///   refusal says which call shows it).
    pub fn assistant_go_to(&mut self, target: &GoTo, host: &UiContext) -> Result<String, String> {
        let target = &target.without_blanks();
        if let Some(review) = target.review.as_deref()
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
        let mut layer = target
            .layer
            .as_ref()
            .map(|layer| self.layer_position(layer))
            .transpose()?;
        let thread = target
            .thread
            .as_deref()
            .map(|named| self.thread_target(named))
            .transpose()?;
        let mut notes = Vec::new();
        // Everything is checked before anything moves; a layer shown alone only shows its own
        // files and threads.
        let entries = self.rail_entries();
        let one_layer = self.stack_view == super::stack::StackView::OneLayer;
        if let Some(source) = thread.as_ref().and_then(|thread| thread.source.as_deref())
            && one_layer
        {
            let (entered, note) = self.thread_layer(&entries, layer, source);
            layer = entered;
            notes.extend(note);
        }
        let alone = if one_layer {
            match &layer {
                Some((position, _)) => entries.get(*position).map(|entry| entry.source_id.clone()),
                None => self
                    .review
                    .as_ref()
                    .and_then(|review| review.doc.focus().map(str::to_owned)),
            }
        } else {
            None
        };
        let (version, file) = if let Some(thread) = &thread {
            notes.extend(thread_mismatch(target, thread));
            (None, None)
        } else {
            let version = target
                .version
                .map(|version| {
                    self.check_version(layer.as_ref().map(|(_, number)| number.clone()), version)
                })
                .transpose()?;
            let file = target
                .path
                .as_deref()
                .map(|path| {
                    let within = alone.as_deref().filter(|_| target.source.is_none());
                    let file = self.find_file(path, target.source.as_deref(), within)?;
                    let refusal =
                        self.hidden_refusal(&file.path, &ItemKey::FileHeader(file.clone()));
                    refusal.map_or(Ok(file), Err)
                })
                .transpose()?;
            (version, file)
        };
        let mut done = Vec::new();
        if let Some((position, wanted)) = layer {
            let current = self.current_layer(&entries);
            self.enter_layer(&entries, current, position);
            done.push(format!("layer {wanted}"));
        }
        if let Some((number, index)) = version {
            self.show_layer_version(number, index, host);
            done.push(format!("#{number} v{}", index + 1));
        }
        if let Some(thread) = thread
            && let Some(review) = self.review.as_mut()
        {
            super::annotate::jump_to_annotation(review, &thread.key);
            done.push(format!("thread {} {}", thread.tool_id, thread.shown));
        }
        if let Some(file) = file {
            done.push(self.show_file_location(&file, target.line, target.side.as_deref()));
        }
        let mut answer = format!("now showing {}", done.join(", "));
        if !notes.is_empty() {
            answer.push_str(" (");
            answer.push_str(&notes.join("; "));
            answer.push(')');
        }
        Ok(answer)
    }

    /// The layer to enter, shown one layer at a time, to reach a thread in layer `source`
    /// when `named` was named: the named one when it holds the thread, none when the layer
    /// shown alone does, else the thread's own (with a note when another was named).
    fn thread_layer(
        &self,
        entries: &[super::stack::RailEntry],
        named: Option<(usize, String)>,
        source: &str,
    ) -> (Option<(usize, String)>, Option<String>) {
        let named_source = named
            .as_ref()
            .and_then(|(position, _)| entries.get(*position))
            .map(|entry| entry.source_id.as_str());
        if named_source == Some(source) {
            return (named, None);
        }
        let shown = self.review.as_ref().and_then(|review| review.doc.focus());
        if named_source.is_none() && shown == Some(source) {
            return (None, None);
        }
        let number = self
            .layer_sources()
            .iter()
            .find(|layer| layer.source_id == source)
            .map_or_else(|| source.to_owned(), |layer| layer.number.to_string());
        let entered = entries
            .iter()
            .position(|entry| entry.source_id == source)
            .map(|position| (position, number));
        let note = named_source.map(|_| {
            "the thread is in another layer than the one named; went to its layer".to_owned()
        });
        (entered, note)
    }

    /// The thread a tool names `named` (as `code_review.list_threads`, `current_view` and the
    /// question context give it), checked to be reachable on screen.
    fn thread_target(&self, named: &str) -> Result<ThreadTarget, String> {
        let review = self
            .review
            .as_ref()
            .ok_or("the review is still loading; try again")?;
        let Some(thread_id) = self.notes.thread_for_tool_id(named) else {
            return Err(self.unknown_thread(named));
        };
        let key = ItemKey::Thread {
            thread_id: thread_id.clone(),
        };
        let resolved_hidden = self.hide_resolved && self.notes.thread_resolved(&thread_id);
        let Some(source) = review.doc.source_of_key(&key) else {
            return Err(if resolved_hidden {
                format!(
                    "thread {named} is resolved and resolved threads are hidden; call \
                     bcode.code-review.set_view with `hide_resolved_threads: false` first"
                )
            } else {
                format!(
                    "thread {named} is not placed in the review on screen yet; try again in a \
                     moment, or go to its file with `path`"
                )
            });
        };
        let source = source.map(str::to_owned);
        if let Some(refusal) = self.hidden_refusal(&format!("thread {named}"), &key) {
            return Err(refusal);
        }
        let first = self
            .notes
            .thread(&thread_id)
            .first()
            .map(|first| first.anchor.clone());
        let remote = self.notes.remote(&thread_id);
        let placement = remote.map(|info| match info.placement {
            bcode_code_review::conversation::Placement::Exact => "",
            bcode_code_review::conversation::Placement::Moved => "moved with its code, ",
            bcode_code_review::conversation::Placement::Near => {
                "near its code, which changed since, "
            }
            bcode_code_review::conversation::Placement::OnFile => {
                "on its file: its lines are gone, "
            }
            bcode_code_review::conversation::Placement::OnLayer => {
                "on its pull request: the review no longer changes its file, "
            }
        });
        let shown = first.as_ref().map_or_else(
            || "the review".to_owned(),
            bcode_code_review::assistant::location_label,
        );
        let mut paths = Vec::new();
        if let Some(first) = &first {
            paths.push(first.file_path.clone());
        }
        if let Some(info) = remote {
            paths.push(info.thread.anchor.path.clone());
        }
        Ok(ThreadTarget {
            tool_id: self.notes.tool_thread_id(&thread_id),
            shown: format!("({}shown at {shown})", placement.unwrap_or_default()),
            key,
            source,
            paths,
            anchor: first,
        })
    }

    /// Why no thread the review holds is named `named`, and what to do.
    fn unknown_thread(&self, named: &str) -> String {
        if bcode_code_review_models::is_pull_thread_ref(named) {
            if self.notes.remote_threads().is_empty() {
                return format!(
                    "pull request thread {named} is not on screen: the review has not read its \
                     pull request threads yet (try again in a moment), or they are off"
                );
            }
            return format!(
                "pull request thread {named} is not placed on the review on screen (its code \
                 could not be followed and such threads show only in the threads panel); go to \
                 its file with `path` instead"
            );
        }
        format!("thread {named} is not shown in this review (code_review.list_threads lists them)")
    }

    /// Why `what` (held at `key`) cannot be shown as the review is shown now, and how to show
    /// it: the comparison hides its file or leaves its layer out. A layer shown alone is not a
    /// refusal; naming the layer shows it.
    fn hidden_refusal(&self, what: &str, key: &ItemKey) -> Option<String> {
        use bcode_code_review::document::KeyHidden;
        match self.review.as_ref()?.doc.key_hidden(key)? {
            KeyHidden::Comparison { source } => Some(format!(
                "{what} is not shown: the comparison shows only what changed and its file in \
                 {source} did not; call bcode.code-review.set_comparison with \
                 `show_hidden_files: true` (or `view: full`) first"
            )),
            KeyHidden::Filtered { source } => Some(format!(
                "{what} is not shown: its layer {source} did not change and the comparison shows \
                 only changed layers; call bcode.code-review.set_comparison with \
                 `only_changed: false` first"
            )),
            KeyHidden::Focused { .. } => None,
        }
    }

    /// The rail position of the stack layer `layer` names, and its number as named.
    fn layer_position(&self, layer: &serde_json::Value) -> Result<(usize, String), String> {
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
        position.map(|position| (position, wanted.clone())).ok_or_else(|| {
            format!(
                "{wanted} is not a stack layer of this review (bcode.code-review.current_view lists them)"
            )
        })
    }

    /// The pull request (`named`, else the current layer's) and version index `version`
    /// (1-based) names, once its versions are known.
    fn check_version(&self, named: Option<String>, version: usize) -> Result<(u64, usize), String> {
        let layers = self.layer_sources();
        let number = named
            .map_or_else(
                || self.current_layer_number(),
                |named| {
                    layers
                        .iter()
                        .find(|layer| layer.number.to_string() == named || layer.source_id == named)
                        .map(|layer| layer.number)
                },
            )
            .ok_or("name the pull request whose version to show with `layer`")?;
        let count = self.version_count(number)?;
        if version == 0 || version > count {
            return Err(format!("#{number} has versions v1 to v{count}"));
        }
        Ok((number, version - 1))
    }

    /// How many versions pull request `number` has, once the stack history is loaded.
    fn version_count(&self, number: u64) -> Result<usize, String> {
        if !self
            .layer_sources()
            .iter()
            .any(|layer| layer.number == number)
        {
            return Err(format!("#{number} is not a pull request of this review"));
        }
        self.stack_history
            .as_ref()
            .ok_or_else(|| {
                format!("the versions of #{number} are loading; call again in a moment")
            })?
            .layers
            .iter()
            .find(|layer| layer.number == number)
            .map(|layer| layer.versions.len())
            .ok_or_else(|| format!("#{number} has no recorded versions yet"))
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
        let plan = match self.plan_comparison(change) {
            Ok(plan) => plan,
            Err(error) => {
                // Versions and pushes need the stack history: start loading it for the retry.
                if self.stack_history.is_none() {
                    self.load_history_quietly(host);
                }
                return Err(error);
            }
        };
        let mut done = Vec::new();
        if let Some((baseline, name)) = plan.baseline {
            match baseline {
                BaselinePlan::Set(baseline) => self.set_compare_baseline(baseline, host),
                BaselinePlan::Ago(ms) => self.compare_ago(ms, host),
                BaselinePlan::StackAt(at_ms) => self.compare_stack_at(at_ms, host),
            }
            done.push(self.status_or(format!("baseline {name}")));
        }
        if let Some(wanted) = plan.view {
            if self.comparing.view() != wanted {
                self.toggle_compare_view();
            }
            done.push(format!(
                "view {}",
                change.view.as_deref().unwrap_or_default()
            ));
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
        if let Some((number, from, to)) = plan.versions {
            self.pick_versions(number, from, to, host);
            done.push(format!("#{number} v{} → v{}", from + 1, to + 1));
        }
        if let Some(style) = plan.mark_style {
            self.change_marks(|marks| marks.style = style);
            done.push("mark style changed".to_owned());
        }
        if let Some(seen) = plan.seen {
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
        Ok(done.join("; "))
    }

    /// Check every argument of `change`; nothing is applied when one is wrong.
    fn plan_comparison(&self, change: &ComparisonChange) -> Result<ComparisonPlan, String> {
        if change == &ComparisonChange::default() {
            return Err("change at least one comparison setting".to_owned());
        }
        let mut plan = ComparisonPlan::default();
        if let Some(baseline) = change.baseline.as_deref() {
            let checked = match baseline {
                "off" => BaselinePlan::Set(ReviewCompareBaseline::Off),
                "last_looked" => BaselinePlan::Set(ReviewCompareBaseline::LastLooked),
                "previous_push" => BaselinePlan::Set(ReviewCompareBaseline::PreviousPush),
                "ago" => BaselinePlan::Ago(
                    change
                        .ago_minutes
                        .filter(|minutes| *minutes > 0)
                        .ok_or("`ago` needs `ago_minutes`")?
                        .saturating_mul(60_000),
                ),
                "between" => BaselinePlan::Set(ReviewCompareBaseline::Between {
                    from_ms: change.from_ms.ok_or("`between` needs `from_ms`")?,
                    to_ms: change.to_ms,
                }),
                "pushes" => BaselinePlan::Set(self.pushes_baseline(change)?),
                "stack_at" => {
                    let at_ms = change.at_ms.ok_or("`stack_at` needs `at_ms`")?;
                    if self.stack_history.is_none() {
                        return Err(
                            "the stack's history is loading; call again in a moment".to_owned()
                        );
                    }
                    BaselinePlan::StackAt(at_ms)
                }
                other => return Err(format!("unknown baseline {other:?}")),
            };
            plan.baseline = Some((checked, baseline.to_owned()));
        }
        if let Some(view) = change.view.as_deref() {
            plan.view = Some(match view {
                "full" => bcode_code_review::document::CompareView::Full,
                "changes" => bcode_code_review::document::CompareView::Changes,
                other => return Err(format!("unknown view {other:?} (full or changes)")),
            });
        }
        if change.all_layers.is_some() && !self.steps_through_stack() {
            return Err("only a review that steps through a stack has layers".to_owned());
        }
        if let Some(pick) = change.versions {
            let count = self.version_count(pick.number)?;
            let to = pick.to.unwrap_or(count);
            if pick.from == 0 || to == 0 || pick.from > count || to > count || pick.from > to {
                return Err(format!(
                    "#{} has versions v1 to v{count}; pick from <= to within them",
                    pick.number
                ));
            }
            plan.versions = Some((pick.number, pick.from - 1, to - 1));
        }
        if let Some(style) = change.mark_style.as_deref() {
            plan.mark_style = Some(match style {
                "both" => MarkStyle::Both,
                "bar" => MarkStyle::Bar,
                "tint" => MarkStyle::Tint,
                "strong" => MarkStyle::Strong,
                other => return Err(format!("unknown mark style {other:?}")),
            });
        }
        if let Some(seen) = change.seen_hunks.as_deref() {
            plan.seen = Some(match seen {
                "show" => SeenHunks::Show,
                "dim" => SeenHunks::Dim,
                "fold" => SeenHunks::Fold,
                other => return Err(format!("unknown seen_hunks {other:?}")),
            });
        }
        Ok(plan)
    }

    /// The baseline comparing the stack just before push `from_push` with the stack after push
    /// `to_push` (now when absent), as the push timeline picks it.
    fn pushes_baseline(&self, change: &ComparisonChange) -> Result<ReviewCompareBaseline, String> {
        let from = change.from_push.ok_or("`pushes` needs `from_push`")?;
        let history = self
            .stack_history
            .as_ref()
            .ok_or("the stack's pushes are loading; call again in a moment")?;
        let count = history.pushes.len();
        let to = change.to_push.unwrap_or(count);
        if count == 0 {
            return Err("the stack has no recorded pushes".to_owned());
        }
        if from == 0 || to == 0 || from > count || to > count || from > to {
            return Err(format!(
                "the stack has pushes 1 to {count}; pick from_push <= to_push within them"
            ));
        }
        let from_ms = super::compare::column_time(history, from - 1)
            .ok_or("that push is not on the timeline")?;
        Ok(ReviewCompareBaseline::Between {
            from_ms,
            to_ms: super::compare::column_time(history, to),
        })
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
        let plan = Self::plan_view(change)?;
        let mut done = Vec::new();
        if let (Some(side_by_side), Some(layout)) = (plan.side_by_side, change.layout.as_deref()) {
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
        if let (Some(flat), Some(list)) = (plan.flat, change.file_list.as_deref()) {
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

    /// Check every argument of `change`; nothing is applied when one is wrong.
    fn plan_view(change: &ViewChange) -> Result<ViewPlan, String> {
        if change == &ViewChange::default() {
            return Err("change at least one view setting".to_owned());
        }
        let mut plan = ViewPlan::default();
        if let Some(layout) = change.layout.as_deref() {
            plan.side_by_side = Some(match layout {
                "unified" => false,
                "side_by_side" => true,
                other => return Err(format!("unknown layout {other:?}")),
            });
        }
        if let Some(list) = change.file_list.as_deref() {
            plan.flat = Some(match list {
                "flat" => true,
                "tree" => false,
                other => return Err(format!("unknown file list {other:?} (tree or flat)")),
            });
        }
        Ok(plan)
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
