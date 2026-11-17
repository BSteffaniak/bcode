//! What the user is looking at, for the assistant: one snapshot of the screen
//! ([`ViewContext`]) rendered as two bounded turn context blocks and returned whole by the
//! `bcode.code-review.current_view` tool.
//!
//! * **Snapshot.** Taken on the UI thread from the screen on screen only, at the moment a message
//!   is sent (or the tool is called); nothing is cached across screens, so no field can survive
//!   from a place the user left.
//! * **View block** ([`VIEW_LABEL`], stored with the message, at most [`VIEW_BUDGET`] bytes):
//!   where the user is: screen, review, layer, file, cursor, comparison summary, counts, and the
//!   tools that read more. Later turns read it to understand the history after the user moved.
//! * **Detail block** ([`DETAIL_LABEL`], this turn only, at most [`DETAIL_BUDGET`] bytes): the
//!   selection's lines, threads on the file, the focused thread, the per-layer comparison, the
//!   files changed since the baseline, hidden files and pull request state. It is large and
//!   stale by the next turn, and the tools read it again.
//! * **Bounds.** Each section is cut to its own budget, then each block to its total; every cut
//!   says so and names the tool with the rest. Lines are single-line `key: value` text; free text
//!   is quoted with control characters escaped. Both blocks and their labels stay within Bcode's
//!   16 KiB of turn context, which Bcode rejects rather than truncates.

use crate::host::ContextBlock;
use serde::Serialize;
use std::fmt::Write as _;

/// Label of the stored view block.
pub const VIEW_LABEL: &str = "review app view";
/// Label of the request-only detail block.
pub const DETAIL_LABEL: &str = "review app detail";
/// Bytes of the view block at most.
pub const VIEW_BUDGET: usize = 4_096;
/// Bytes of the detail block at most.
pub const DETAIL_BUDGET: usize = 11_264;
/// Name of the tool that returns the whole snapshot.
pub const CURRENT_VIEW_TOOL: &str = "bcode.code-review.current_view";
/// Bytes of one free-text value in a block at most.
const MAX_VALUE_BYTES: usize = 240;
/// Sources listed in the view block.
const VIEW_SOURCES: usize = 8;
/// Stack layer numbers listed in the view block.
const VIEW_STACK_LAYERS: usize = 32;
/// Layers summarized in the view block.
const VIEW_LAYERS: usize = 12;
/// Suggestions listed in the view block.
const VIEW_SUGGESTIONS: usize = 5;
/// Bytes of the selection in the detail block.
const DETAIL_SELECTION_BYTES: usize = 4_096;
/// Threads on the file in the detail block.
const DETAIL_THREADS: usize = 12;
/// Bytes of the threads on the file in the detail block.
const DETAIL_THREADS_BYTES: usize = 2_048;
/// Bytes of the focused thread in the detail block.
const DETAIL_FOCUSED_BYTES: usize = 2_048;
/// Bytes of the per-layer comparison in the detail block.
const DETAIL_DELTAS_BYTES: usize = 2_048;
/// Layers in the detail block's per-layer comparison.
const DETAIL_LAYERS: usize = 32;
/// Files changed since the baseline listed in the detail block.
const DETAIL_CHANGED_FILES: usize = 40;
/// Hidden files listed in the detail block.
const DETAIL_HIDDEN_FILES: usize = 20;

/// What the user is looking at.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ViewContext {
    /// `home`, `wizard`, `review` or `opening`.
    pub screen: String,
    /// Repository root.
    pub repo: String,
    /// When the snapshot was taken, in milliseconds since the Unix epoch.
    pub as_of_ms: u64,
    /// Changes on every screen, review, layer, file or comparison change.
    pub screen_generation: u64,
    /// Home, on home.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home: Option<HomeView>,
    /// The wizard, in the wizard.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wizard: Option<WizardView>,
    /// The review, on a review.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewView>,
    /// Whether the user detached the context (only the screen, repository and review are
    /// described).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
}

/// The reviews home.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HomeView {
    /// The current branch, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Reviews per group: needs attention, in progress, published, archived.
    pub needs_attention: usize,
    /// See [`Self::needs_attention`].
    pub in_progress: usize,
    /// See [`Self::needs_attention`].
    pub published: usize,
    /// See [`Self::needs_attention`].
    pub archived: usize,
    /// The selected review: id and title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<(String, String)>,
    /// The filter typed, when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Suggested new reviews, as listed.
    pub suggestions: Vec<String>,
    /// Whether the reviews are still loading.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub loading: bool,
}

/// The new-review wizard.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WizardView {
    /// The step: `type`, `details` or `confirm`.
    pub step: String,
    /// The source kind being chosen, when one is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The review whose sources are edited, when editing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editing: Option<String>,
    /// Sources added so far.
    pub sources: Vec<String>,
}

/// A bounded list: the items kept and how many were left out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Listed<T> {
    /// Items kept, in order.
    pub items: Vec<T>,
    /// All items.
    pub total: usize,
    /// Items left out.
    pub omitted: usize,
}

impl<T> Listed<T> {
    /// The first `max` of `items`.
    #[must_use]
    pub fn of(items: impl IntoIterator<Item = T>, max: usize) -> Self {
        let mut kept = Vec::new();
        let mut total = 0;
        for item in items {
            total += 1;
            if kept.len() < max {
                kept.push(item);
            }
        }
        let omitted = total - kept.len();
        Self {
            items: kept,
            total,
            omitted,
        }
    }
}

/// One source of a review.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SourceView {
    /// Workspace source id.
    pub id: String,
    /// Label.
    pub label: String,
    /// Kind (`stack_layer`, `pull_request`, `branch`, …).
    pub kind: String,
    /// Pull request number, for pull request kinds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
    /// Head branch, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Head commit it resolved to, when resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Whether it is included.
    pub included: bool,
}

/// The stack of a step-through review.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StackView {
    /// Layer numbers, bottom first.
    pub layers: Vec<u64>,
    /// The current layer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<u64>,
    /// `one_pr`, `all_prs`, or `whole` (the whole stack as one diff).
    pub view: String,
}

/// One layer's comparison.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LayerView {
    /// Pull request number.
    pub number: u64,
    /// `changed`, `rebased_only`, `unchanged`, `new_to_you`, `with_rebase`, `unavailable`, or
    /// `computing`.
    pub state: String,
    /// Head of the baseline version, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Head compared with it.
    pub to: String,
    /// Files differing from the baseline.
    pub files: Vec<FileDelta>,
    /// Files differing from the baseline left out of `files`.
    #[serde(skip_serializing_if = "is_zero")]
    pub files_omitted: usize,
    /// Whether the layer is hidden (only pull requests with real changes show).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// The version the layer shows and how many it has, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<(usize, usize)>,
}

/// One file of a layer's comparison.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FileDelta {
    /// Path.
    pub path: String,
    /// `changed`, `new`, `dropped`, `unchanged`, `unknown`.
    pub since: String,
    /// Lines added since.
    pub added: u32,
    /// Lines removed since.
    pub removed: u32,
}

/// A source that is not a pull request layer, compared with the revisions its last completed
/// review pass recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SourceResult {
    /// Workspace source id.
    pub id: String,
    /// Label.
    pub label: String,
    /// `changed`, `rebased_only`, `unchanged`, `new_to_you`, `with_rebase`, `unavailable`, or
    /// `computing`.
    pub state: String,
    /// Head of the baseline (the pass's recorded revision), when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Head compared with it.
    pub to: String,
    /// Files differing from the baseline.
    pub files: Vec<FileDelta>,
    /// Files differing from the baseline left out of `files`.
    #[serde(skip_serializing_if = "is_zero")]
    pub files_omitted: usize,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// One push of the stack, as the comparison can pick it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PushView {
    /// Push number (1-based, oldest first), as `set_comparison pushes` takes it.
    pub number: usize,
    /// When it happened, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// What it changed.
    pub summary: String,
}

/// The review's active comparison.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ComparisonView {
    /// The baseline: `off`, `last_looked`, `previous_push`, `ago=<duration>`,
    /// `between=<from>..<to>`, `stack_at=<time>`, plus picked versions.
    pub baseline: String,
    /// `full` (the whole change with what is new marked) or `changes` (only what changed).
    pub view: String,
    /// Only pull requests with real changes show.
    pub only_changed: bool,
    /// How what is new is marked, when changed from the configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marks: Option<String>,
    /// Per-layer results, review order.
    pub layers: Vec<LayerView>,
    /// Per-source results of the sources that are not pull request layers, review order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceResult>,
    /// The stack's recorded pushes (the newest ones), when its history is loaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pushes: Option<Listed<PushView>>,
    /// Files hidden as unchanged since the baseline.
    pub hidden_files: usize,
}

/// The file the cursor is in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FileView {
    /// Path.
    pub path: String,
    /// Source it belongs to.
    pub source_id: String,
    /// Status (`added`, `modified`, …).
    pub status: String,
    /// Viewed state (`yes`, `no`, `changed since viewed`).
    pub viewed: String,
    /// How it changed since the baseline, under a comparison.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

/// The cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CursorView {
    /// Where (`path:line`, a range, or a file).
    pub place: String,
    /// Line, when on a line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// `new` or `old`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    /// Selected range (first, last) on `side`, when a range is selected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<(u32, u32)>,
}

/// Threads of the review around the cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ThreadsView {
    /// Open threads on the file.
    pub open: usize,
    /// Resolved threads on the file.
    pub resolved: usize,
    /// One line per thread on the file.
    pub on_file: Listed<String>,
    /// The thread the cursor is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused: Option<FocusedThread>,
}

/// The thread the cursor is on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FocusedThread {
    /// Id for `code_review.get_thread`.
    pub id: String,
    /// Where it comes from.
    pub origin: String,
    /// Resolved.
    pub resolved: bool,
    /// Its comments, first and latest kept, as `author: body`.
    pub comments: Listed<String>,
}

/// Pull request state already loaded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PullView {
    /// Number.
    pub number: u64,
    /// `open`, `closed`, `merged`, `draft`.
    pub state: String,
    /// Latest approvals.
    pub approvals: usize,
    /// Latest requests for changes.
    pub changes_requested: usize,
    /// Checks: `passing`, `failing f/n`, `pending`, or `none`.
    pub checks: String,
    /// Failing check names.
    pub failing: Vec<String>,
}

/// The review on screen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReviewView {
    /// Saved review id (`None` while unsaved).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Annotation scope id.
    pub scope_id: String,
    /// Title.
    pub title: String,
    /// Kind (`stack`, `stack_step_through`, `whole_stack`, `pull_request`, `branch`, `current_branch`,
    /// `range`, `commits`, `commit`, `last_commit`, `working_tree`, `staged`, `unstaged`,
    /// `files`, `mixed`, `loading`).
    pub kind: String,
    /// Sources.
    pub sources: Listed<SourceView>,
    /// The stack, for step-through reviews.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<StackView>,
    /// The comparison, while one is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<ComparisonView>,
    /// Files of the review, and files viewed.
    pub files: (usize, usize),
    /// The file the cursor is in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<FileView>,
    /// The cursor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<CursorView>,
    /// The selected lines (diff lines, prefixed), when a range is selected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<Listed<String>>,
    /// Threads.
    pub threads: ThreadsView,
    /// Pull request state already loaded, for the current layer or pull request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PullView>,
    /// Whether the review is still opening.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub loading: bool,
}

/// Where the rest can be read.
const MORE_REVIEW: &str = "more: bcode.code-review.current_view, code_review.summary, \
code_review.list_files, code_review.file_diff, code_review.list_threads, code_review.get_thread";
/// Where more about home can be read.
const MORE_HOME: &str = "more: bcode.code-review.current_view, code_review.list_reviews";

/// A value as one line: kept bare when it is plain, else quoted with control characters
/// escaped; long values are cut on a character boundary.
fn value(text: &str) -> String {
    let cut = cut(text, MAX_VALUE_BYTES);
    let plain = !cut.is_empty()
        && cut
            .chars()
            .all(|c| c.is_alphanumeric() || "._/-#@:+=,".contains(c));
    if plain { cut } else { format!("{cut:?}") }
}

/// `text` within `max` bytes, cut on a character boundary with `…`.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// A short commit id.
fn short(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

/// The cut marker of a list.
fn more(omitted: usize, what: &str, tool: &str) -> String {
    format!("… {omitted} more {what} ({tool})")
}

impl ViewContext {
    /// A number that changes whenever the screen, review, layer, file, cursor or comparison
    /// changes.
    #[must_use]
    pub fn generation(&self) -> u64 {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::hash::DefaultHasher::new();
        let mut identity = self.clone();
        identity.as_of_ms = 0;
        identity.screen_generation = 0;
        serde_json::to_string(&identity)
            .unwrap_or_default()
            .hash(&mut hasher);
        hasher.finish()
    }

    /// The view and detail blocks of this snapshot; the detail block is left out when the user
    /// detached the context or nothing detailed applies.
    #[must_use]
    pub fn blocks(&self) -> Vec<ContextBlock> {
        let mut blocks = vec![ContextBlock {
            label: VIEW_LABEL.to_owned(),
            text: self.view_text(),
            request_only: false,
        }];
        let detail = self.detail_text();
        if !self.detached && !detail.is_empty() {
            blocks.push(ContextBlock {
                label: DETAIL_LABEL.to_owned(),
                text: detail,
                request_only: true,
            });
        }
        blocks
    }

    /// The view block's text, at most [`VIEW_BUDGET`] bytes.
    #[must_use]
    pub fn view_text(&self) -> String {
        let mut lines = vec![
            format!("screen: {}", self.screen),
            format!("repo: {}", value(&self.repo)),
        ];
        if let Some(review) = &self.review {
            lines.push(review_line(review));
            if self.detached {
                lines.push("context: detached by the user".to_owned());
            } else {
                review_lines(review, &mut lines);
            }
            lines.push(MORE_REVIEW.to_owned());
        } else if let Some(home) = &self.home {
            if !self.detached {
                home_lines(home, &mut lines);
            }
            lines.push(MORE_HOME.to_owned());
        } else if let Some(wizard) = &self.wizard {
            if !self.detached {
                wizard_lines(wizard, &mut lines);
            }
            lines.push(MORE_HOME.to_owned());
        }
        fit(lines, VIEW_BUDGET)
    }

    /// The detail block's text, at most [`DETAIL_BUDGET`] bytes; empty when nothing detailed
    /// applies.
    #[must_use]
    pub fn detail_text(&self) -> String {
        let Some(review) = &self.review else {
            return String::new();
        };
        // In order of importance: sections are dropped from the end when the total would be
        // exceeded.
        let sections: Vec<(Vec<String>, usize)> = vec![
            (selection_section(review), DETAIL_SELECTION_BYTES),
            (threads_section(review), DETAIL_THREADS_BYTES),
            (focused_section(review), DETAIL_FOCUSED_BYTES),
            (deltas_section(review), DETAIL_DELTAS_BYTES),
            (changed_section(review), DETAIL_BUDGET),
            (hidden_section(review), DETAIL_BUDGET),
            (pull_section(review), DETAIL_BUDGET),
        ];
        let mut text = String::new();
        let mut dropped = Vec::new();
        for (lines, budget) in sections {
            if lines.is_empty() {
                continue;
            }
            let section = fit(lines, budget);
            if text.len() + section.len() + 1 + 120 > DETAIL_BUDGET {
                dropped.push(section.lines().next().unwrap_or_default().to_owned());
                continue;
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&section);
        }
        if !dropped.is_empty() {
            let _ = write!(
                text,
                "\n… {} sections left out ({CURRENT_VIEW_TOOL})",
                dropped.len()
            );
        }
        text
    }
}

/// Bytes of the context tool's answer at most.
pub const TOOL_BUDGET: usize = 64 * 1024;

/// Arguments of the context tool.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolArguments {
    #[serde(default)]
    sections: Option<Vec<String>>,
    /// Files listed per layer or source at most.
    #[serde(default)]
    max_files: Option<usize>,
}

/// Files listed per layer or source when `max_files` is not given.
const DEFAULT_TOOL_FILES: usize = 100;

/// Lists the answer shortens, in order, when it does not fit: (JSON pointer of the array, or
/// of arrays inside the array's items with `/*/`, items kept).
const SHORTENED: &[(&str, usize)] = &[
    ("/review/comparison/layers/*/files", 10),
    ("/review/comparison/sources/*/files", 10),
    ("/review/selection/items", 20),
    ("/review/threads/on_file/items", 20),
    ("/review/sources/items", 20),
    ("/review/comparison/pushes/items", 5),
    ("/review/comparison/layers/*/files", 0),
    ("/review/comparison/sources/*/files", 0),
    ("/review/selection/items", 0),
    ("/review/threads/on_file/items", 0),
];

/// Sections dropped, in order, when shortened lists still do not fit.
const DROPPED: &[&str] = &[
    "/review/pull_request",
    "/review/threads",
    "/review/selection",
    "/review/comparison",
    "/review/sources",
    "/home",
    "/wizard",
];

impl ViewContext {
    /// The context tool's answer for JSON `arguments`: this snapshot as JSON, only the
    /// `sections` asked for, with at most `max_files` files per layer or source, within
    /// [`TOOL_BUDGET`] bytes. The answer is always valid JSON: when it does not fit, long lists
    /// are shortened, then whole sections dropped, and a `truncated` object names what was
    /// left out and where to read it in full.
    ///
    /// # Errors
    ///
    /// * A message naming the problem when the arguments are not the tool's.
    pub fn tool_answer(&self, arguments: &str) -> Result<String, String> {
        let arguments: ToolArguments = if arguments.trim().is_empty() {
            ToolArguments::default()
        } else {
            serde_json::from_str(arguments)
                .map_err(|error| format!("invalid arguments: {error}"))?
        };
        let mut value = serde_json::to_value(self).map_err(|error| error.to_string())?;
        if let Some(sections) = &arguments.sections {
            keep_sections(&mut value, sections);
        }
        let max_files = arguments.max_files.unwrap_or(DEFAULT_TOOL_FILES);
        let mut shortened = Vec::new();
        for pointer in [
            "/review/comparison/layers/*/files",
            "/review/comparison/sources/*/files",
        ] {
            if shorten(&mut value, pointer, max_files) {
                shortened.push(pointer.replace("/*/", "[]/"));
            }
        }
        Ok(fit_json(value, shortened, TOOL_BUDGET))
    }
}

/// `value` as JSON within `budget` bytes, shortening lists and dropping sections as needed;
/// `shortened` names lists already shortened.
fn fit_json(mut value: serde_json::Value, mut shortened: Vec<String>, budget: usize) -> String {
    let mut dropped: Vec<&str> = Vec::new();
    let mark = |value: &mut serde_json::Value, shortened: &[String], dropped: &[&str]| {
        if shortened.is_empty() && dropped.is_empty() {
            return;
        }
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "truncated".to_owned(),
                serde_json::json!({
                    "shortened": shortened,
                    "dropped": dropped,
                    "read_in_full": "code_review.summary, code_review.list_files, \
                        code_review.file_diff, code_review.list_threads, and \
                        bcode.code-review.current_view with fewer sections or max_files",
                }),
            );
        }
    };
    mark(&mut value, &shortened, &dropped);
    let mut text = value.to_string();
    for (pointer, keep) in SHORTENED {
        if text.len() <= budget {
            return text;
        }
        if shorten(&mut value, pointer, *keep) {
            let name = pointer.replace("/*/", "[]/");
            if !shortened.contains(&name) {
                shortened.push(name);
            }
            mark(&mut value, &shortened, &dropped);
            text = value.to_string();
        }
    }
    for pointer in DROPPED {
        if text.len() <= budget {
            return text;
        }
        if remove_pointer(&mut value, pointer) {
            dropped.push(pointer);
            mark(&mut value, &shortened, &dropped);
            text = value.to_string();
        }
    }
    if text.len() <= budget {
        return text;
    }
    // Only the screen's identity is left.
    serde_json::json!({
        "screen": value.get("screen").cloned().unwrap_or_default(),
        "screen_generation": value.get("screen_generation").cloned().unwrap_or_default(),
        "truncated": {
            "dropped": "everything but the screen",
            "read_in_full": "bcode.code-review.current_view with fewer sections",
        },
    })
    .to_string()
}

/// Keep at most `keep` items of the array at `pointer` (`/*/` reaches into every item of an
/// array on the way). Returns whether anything was left out.
fn shorten(value: &mut serde_json::Value, pointer: &str, keep: usize) -> bool {
    if let Some((head, tail)) = pointer.split_once("/*/") {
        let Some(serde_json::Value::Array(items)) = value.pointer_mut(head) else {
            return false;
        };
        let mut cut = false;
        for item in items {
            cut |= shorten(item, &format!("/{tail}"), keep);
        }
        return cut;
    }
    let removed = match value.pointer_mut(pointer) {
        Some(serde_json::Value::Array(items)) if items.len() > keep => {
            let removed = items.len() - keep;
            items.truncate(keep);
            removed
        }
        _ => return false,
    };
    let Some((parent, field)) = pointer.rsplit_once('/') else {
        return true;
    };
    let Some(list) = value
        .pointer_mut(parent)
        .and_then(|list| list.as_object_mut())
    else {
        return true;
    };
    if field == "items"
        && let Some(total) = list.get("total").and_then(serde_json::Value::as_u64)
    {
        // A bounded list (`items`, `total`, `omitted`) keeps saying how many are left out.
        let kept = u64::try_from(keep).unwrap_or(u64::MAX).min(total);
        list.insert("omitted".to_owned(), serde_json::Value::from(total - kept));
    } else {
        // A plain list keeps a count of what it left out beside it (`files_omitted`).
        let counted = format!("{field}_omitted");
        let before = list
            .get(&counted)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let removed = u64::try_from(removed).unwrap_or(u64::MAX);
        list.insert(
            counted,
            serde_json::Value::from(before.saturating_add(removed)),
        );
    }
    true
}

/// Remove the field at `pointer`. Returns whether it was there.
fn remove_pointer(value: &mut serde_json::Value, pointer: &str) -> bool {
    let Some((parent, field)) = pointer.rsplit_once('/') else {
        return false;
    };
    let parent = if parent.is_empty() {
        Some(value)
    } else {
        value.pointer_mut(parent)
    };
    parent
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|object| object.remove(field))
        .is_some()
}

/// Keep only `sections` of the snapshot `value` (the screen fields always stay).
fn keep_sections(value: &mut serde_json::Value, sections: &[String]) {
    let wants = |name: &str| sections.iter().any(|section| section == name);
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if !wants("home") {
        object.remove("home");
    }
    if !wants("wizard") {
        object.remove("wizard");
    }
    let Some(review) = object
        .get_mut("review")
        .and_then(|review| review.as_object_mut())
    else {
        return;
    };
    for (section, fields) in [
        ("stack", &["stack"][..]),
        ("comparison", &["comparison"]),
        ("file", &["file", "cursor"]),
        ("selection", &["selection"]),
        ("threads", &["threads"]),
        ("pull_request", &["pull_request"]),
        (
            "review",
            &[
                "id", "scope_id", "title", "kind", "sources", "files", "loading",
            ],
        ),
    ] {
        if !wants(section) {
            for field in fields {
                review.remove(*field);
            }
        }
    }
}

/// `lines` joined within `budget` bytes: lines that do not fit are left out from the end
/// (keeping a final `more:` line), with a marker naming the tool with the rest.
fn fit(lines: Vec<String>, budget: usize) -> String {
    let joined = lines.join("\n");
    if joined.len() <= budget {
        return joined;
    }
    let (body, tail): (Vec<String>, Option<String>) = match lines.last() {
        Some(last) if last.starts_with("more:") => {
            let mut body = lines.clone();
            let tail = body.pop();
            (body, tail)
        }
        _ => (lines, None),
    };
    let marker_room = 80 + tail.as_ref().map_or(0, |tail| tail.len() + 1);
    let mut text = String::new();
    let mut kept = 0;
    for line in &body {
        let line = cut(line, budget.saturating_sub(marker_room));
        if text.len() + line.len() + 1 + marker_room > budget {
            break;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&line);
        kept += 1;
    }
    let left = body.len() - kept;
    let _ = write!(
        text,
        "\n… {left} more lines cut to fit ({CURRENT_VIEW_TOOL})"
    );
    if let Some(tail) = tail {
        text.push('\n');
        text.push_str(&tail);
    }
    text
}

fn review_line(review: &ReviewView) -> String {
    format!(
        "review: id={}  title={}  kind={}  saved={}",
        review
            .id
            .as_deref()
            .map_or_else(|| value(&review.scope_id), value),
        value(&review.title),
        review.kind,
        if review.id.is_some() { "yes" } else { "no" }
    )
}

fn review_lines(review: &ReviewView, lines: &mut Vec<String>) {
    if review.loading {
        lines.push("state: opening (files not loaded yet)".to_owned());
    }
    // Sources.
    let mut sources = format!("sources: {}", review.sources.total);
    for source in review.sources.items.iter().take(VIEW_SOURCES) {
        sources.push_str("  ");
        sources.push_str(&source_text(source));
    }
    let shown = review.sources.items.len().min(VIEW_SOURCES);
    let left = review.sources.total - shown;
    if left > 0 {
        let _ = write!(
            sources,
            "  {}",
            more(left, "sources", "code_review.summary")
        );
    }
    lines.push(sources);
    stack_lines(review, lines);
    comparison_lines(review, lines);
    place_lines(review, lines);
}

/// The stack and the file counts.
fn stack_lines(review: &ReviewView, lines: &mut Vec<String>) {
    // Stack.
    if let Some(stack) = &review.stack {
        let numbers: Vec<String> = stack
            .layers
            .iter()
            .take(VIEW_STACK_LAYERS)
            .map(|number| format!("#{number}"))
            .collect();
        let mut line = format!("stack: layers={}", numbers.join(","));
        if stack.layers.len() > VIEW_STACK_LAYERS {
            let _ = write!(
                line,
                " {}",
                more(
                    stack.layers.len() - VIEW_STACK_LAYERS,
                    "layers",
                    "code_review.summary"
                )
            );
        }
        if let Some(current) = stack.current {
            let position = stack
                .layers
                .iter()
                .position(|number| *number == current)
                .map(|index| format!(" ({}/{})", index + 1, stack.layers.len()))
                .unwrap_or_default();
            let _ = write!(line, "  current=#{current}{position}");
        }
        let _ = write!(line, "  view={}", stack.view);
        lines.push(line);
    }
    lines.push(format!(
        "files: {}  viewed={}",
        review.files.0, review.files.1
    ));
}

/// The comparison, its per-layer summary and counts.
fn comparison_lines(review: &ReviewView, lines: &mut Vec<String>) {
    // Comparison.
    if let Some(comparison) = &review.comparison {
        let mut line = format!(
            "comparison: baseline={}  view={}  only_changed={}",
            comparison.baseline,
            comparison.view,
            if comparison.only_changed { "yes" } else { "no" }
        );
        if let Some(marks) = &comparison.marks {
            let _ = write!(line, "  marks={marks}");
        }
        lines.push(line);
        if !comparison.layers.is_empty() {
            let mut layers = String::from("layers:");
            for (index, layer) in comparison.layers.iter().take(VIEW_LAYERS).enumerate() {
                layers.push_str(if index == 0 { " " } else { " · " });
                layers.push_str(&layer_summary(layer));
            }
            if comparison.layers.len() > VIEW_LAYERS {
                let _ = write!(
                    layers,
                    "  {}",
                    more(
                        comparison.layers.len() - VIEW_LAYERS,
                        "layers",
                        CURRENT_VIEW_TOOL
                    )
                );
            }
            lines.push(layers);
            let count = |state: &str| {
                comparison
                    .layers
                    .iter()
                    .filter(|layer| layer.state == state)
                    .count()
            };
            lines.push(format!(
                "counts: changed={} rebased={} unchanged={} new_to_you={} pending={}  \
                 hidden_layers={} hidden_files={}",
                count("changed") + count("with_rebase") + count("unavailable"),
                count("rebased_only"),
                count("unchanged"),
                count("new_to_you"),
                count("computing"),
                comparison
                    .layers
                    .iter()
                    .filter(|layer| layer.hidden)
                    .count(),
                comparison.hidden_files
            ));
        }
        if !comparison.sources.is_empty() {
            let mut sources = String::from("source_results:");
            for (index, source) in comparison.sources.iter().take(VIEW_LAYERS).enumerate() {
                sources.push_str(if index == 0 { " " } else { " · " });
                sources.push_str(&source_summary(source));
            }
            if comparison.sources.len() > VIEW_LAYERS {
                let _ = write!(
                    sources,
                    "  {}",
                    more(
                        comparison.sources.len() - VIEW_LAYERS,
                        "sources",
                        CURRENT_VIEW_TOOL
                    )
                );
            }
            lines.push(sources);
        }
    } else {
        lines.push("comparison: off".to_owned());
    }
}

fn source_summary(source: &SourceResult) -> String {
    let mut text = format!("{} {}", value(&source.label), source.state);
    if source.state == "changed" || source.state == "with_rebase" {
        let (added, removed) = source
            .files
            .iter()
            .fold((0, 0), |(a, r), file| (a + file.added, r + file.removed));
        let _ = write!(
            text,
            " {} file{} +{added} −{removed}",
            source.files.len(),
            if source.files.len() == 1 { "" } else { "s" }
        );
    }
    text
}

/// The file, cursor, threads and pull request state.
fn place_lines(review: &ReviewView, lines: &mut Vec<String>) {
    // File and cursor.
    if let Some(file) = &review.file {
        let mut line = format!(
            "file: {}  status={}  viewed={}",
            value(&file.path),
            file.status,
            value(&file.viewed)
        );
        if let Some(since) = &file.since {
            let _ = write!(line, "  since={since}");
        }
        if let Some((current, total)) = review
            .comparison
            .as_ref()
            .and_then(|comparison| {
                comparison.layers.iter().find(|layer| {
                    Some(layer.number) == review.stack.as_ref().and_then(|s| s.current)
                })
            })
            .and_then(|layer| layer.version)
        {
            let _ = write!(line, "  version=v{current} of {total}");
        }
        lines.push(line);
    }
    if let Some(cursor) = &review.cursor {
        let mut line = format!("cursor: {}", value(&cursor.place));
        if let Some(side) = &cursor.side {
            let _ = write!(line, "  side={side}");
        }
        if let Some((first, last)) = cursor.range {
            let count = review
                .selection
                .as_ref()
                .map_or(0, |selection| selection.total);
            let _ = write!(
                line,
                "  selection={first}-{last} ({count} lines, text in detail)"
            );
        }
        lines.push(line);
    }
    let threads = &review.threads;
    if threads.open + threads.resolved > 0 || threads.focused.is_some() {
        let mut line = format!(
            "threads: file open={} resolved={}",
            threads.open, threads.resolved
        );
        if let Some(focused) = &threads.focused {
            let _ = write!(
                line,
                "  focused={} ({}, {} comments)",
                value(&focused.id),
                value(&focused.origin),
                focused.comments.total
            );
        }
        lines.push(line);
    }
    if let Some(pull) = &review.pull_request {
        lines.push(format!(
            "pull_request: #{} {}  approvals={}  changes_requested={}  checks={}",
            pull.number, pull.state, pull.approvals, pull.changes_requested, pull.checks
        ));
    } else if review
        .sources
        .items
        .iter()
        .any(|source| source.number.is_some())
    {
        lines.push("pull_request: not loaded (code_review.pull_request reads it)".to_owned());
    }
}

fn source_text(source: &SourceView) -> String {
    let mut text = source.number.map_or_else(
        || format!("{}:{}", source.kind, value(&source.label)),
        |number| format!("#{number}"),
    );
    if let Some(branch) = &source.branch {
        let _ = write!(text, " {}", value(branch));
    }
    if let Some(head) = &source.head {
        let _ = write!(text, "@{}", short(head));
    }
    if !source.included {
        text.push_str(" (excluded)");
    }
    text
}

fn layer_summary(layer: &LayerView) -> String {
    let mut text = format!("#{} {}", layer.number, layer.state);
    if layer.state == "changed" || layer.state == "with_rebase" {
        let (added, removed) = layer
            .files
            .iter()
            .fold((0, 0), |(a, r), file| (a + file.added, r + file.removed));
        let _ = write!(
            text,
            " {} file{} +{added} −{removed}",
            layer.files.len(),
            if layer.files.len() == 1 { "" } else { "s" }
        );
    }
    if layer.hidden {
        text.push_str(" (hidden)");
    }
    text
}

fn home_lines(home: &HomeView, lines: &mut Vec<String>) {
    if let Some(branch) = &home.branch {
        lines.push(format!("branch: {}", value(branch)));
    }
    let mut line = format!(
        "reviews: needs_attention={} in_progress={} published={} archived={}",
        home.needs_attention, home.in_progress, home.published, home.archived
    );
    if home.loading {
        line.push_str("  (loading)");
    }
    if let Some((id, title)) = &home.selected {
        let _ = write!(line, "  selected={} {}", value(id), value(title));
    }
    lines.push(line);
    if let Some(filter) = &home.filter {
        lines.push(format!("filter: {}", value(filter)));
    }
    if !home.suggestions.is_empty() {
        let mut line = String::from("suggestions:");
        for (index, suggestion) in home.suggestions.iter().take(VIEW_SUGGESTIONS).enumerate() {
            line.push_str(if index == 0 { " " } else { " · " });
            line.push_str(&value(suggestion));
        }
        if home.suggestions.len() > VIEW_SUGGESTIONS {
            let _ = write!(
                line,
                "  {}",
                more(
                    home.suggestions.len() - VIEW_SUGGESTIONS,
                    "suggestions",
                    CURRENT_VIEW_TOOL
                )
            );
        }
        lines.push(line);
    }
}

fn wizard_lines(wizard: &WizardView, lines: &mut Vec<String>) {
    let mut line = format!("wizard: step={}", wizard.step);
    if let Some(kind) = &wizard.kind {
        let _ = write!(line, "  kind={kind}");
    }
    if let Some(editing) = &wizard.editing {
        let _ = write!(line, "  editing={}", value(editing));
    }
    if !wizard.sources.is_empty() {
        let sources: Vec<String> = wizard.sources.iter().map(|source| value(source)).collect();
        let _ = write!(line, "  sources={}", sources.join(","));
    }
    lines.push(line);
}

fn selection_section(review: &ReviewView) -> Vec<String> {
    let (Some(selection), Some(cursor)) = (&review.selection, &review.cursor) else {
        return Vec::new();
    };
    bounded(
        format!("selection: {}", value(&cursor.place)),
        selection
            .items
            .iter()
            .map(|line| format!("  {}", cut(line, 400))),
        selection.total,
        usize::MAX,
        DETAIL_SELECTION_BYTES,
        |left| {
            format!(
                "  {}",
                more(left, "selected lines", "code_review.file_diff")
            )
        },
    )
}

/// A section: `header`, then as many of `items` as fit in `budget` bytes (at most `max`),
/// then the marker of the `total - kept` left out.
fn bounded(
    header: String,
    items: impl Iterator<Item = String>,
    total: usize,
    max: usize,
    budget: usize,
    marker: impl Fn(usize) -> String,
) -> Vec<String> {
    // Room kept for the marker, whatever it ends up counting.
    let marker_room = marker(total).len() + 8;
    let mut bytes = header.len();
    let mut lines = vec![header];
    let mut kept = 0;
    for item in items.take(max) {
        if bytes + item.len() + 1 + marker_room > budget {
            break;
        }
        bytes += item.len() + 1;
        lines.push(item);
        kept += 1;
    }
    let left = total.saturating_sub(kept);
    if left > 0 {
        lines.push(marker(left));
    }
    lines
}

fn threads_section(review: &ReviewView) -> Vec<String> {
    let threads = &review.threads.on_file;
    if threads.total == 0 {
        return Vec::new();
    }
    let path = review
        .file
        .as_ref()
        .map_or_else(String::new, |file| format!(" path={}", value(&file.path)));
    bounded(
        "threads_on_file:".to_owned(),
        threads
            .items
            .iter()
            .map(|thread| format!("  - {}", cut(thread, 200))),
        threads.total,
        DETAIL_THREADS,
        DETAIL_THREADS_BYTES,
        |left| {
            format!(
                "  {}",
                more(left, "threads", &format!("code_review.list_threads{path}"))
            )
        },
    )
}

fn focused_section(review: &ReviewView) -> Vec<String> {
    let Some(focused) = &review.threads.focused else {
        return Vec::new();
    };
    let id = value(&focused.id);
    bounded(
        format!(
            "focused_thread: id={id}  origin={}  resolved={}",
            value(&focused.origin),
            if focused.resolved { "yes" } else { "no" }
        ),
        focused
            .comments
            .items
            .iter()
            .map(|comment| format!("  {}", value(comment))),
        focused.comments.total,
        usize::MAX,
        DETAIL_FOCUSED_BYTES,
        |left| {
            format!(
                "  {}",
                more(left, "comments", &format!("code_review.get_thread id={id}"))
            )
        },
    )
}

fn deltas_section(review: &ReviewView) -> Vec<String> {
    let Some(comparison) = &review.comparison else {
        return Vec::new();
    };
    if comparison.layers.is_empty() {
        return source_deltas_section(comparison);
    }
    let lines = comparison.layers.iter().map(|layer| {
        let mut line = format!(
            "  #{} {}  {}→{}",
            layer.number,
            layer.state,
            layer.from.as_deref().map_or("none", short),
            short(&layer.to)
        );
        if let Some((current, total)) = layer.version {
            let _ = write!(line, "  v{current} of {total}");
        }
        let files: Vec<String> = layer.files.iter().take(6).map(file_delta).collect();
        if !files.is_empty() {
            let _ = write!(line, ": {}", files.join(", "));
            if layer.files.len() > 6 {
                let _ = write!(line, " +{} more", layer.files.len() - 6);
            }
        }
        cut(&line, 400)
    });
    bounded(
        "layer_deltas:".to_owned(),
        lines,
        comparison.layers.len(),
        DETAIL_LAYERS,
        DETAIL_DELTAS_BYTES,
        |left| format!("  {}", more(left, "layers", CURRENT_VIEW_TOOL)),
    )
}

/// The non-layer sources' results, in a review without pull request layers.
fn source_deltas_section(comparison: &ComparisonView) -> Vec<String> {
    if comparison.sources.is_empty() {
        return Vec::new();
    }
    let lines = comparison.sources.iter().map(|source| {
        let mut line = format!(
            "  {} {}  {}→{}",
            value(&source.label),
            source.state,
            source.from.as_deref().map_or("none", short),
            short(&source.to)
        );
        let files: Vec<String> = source.files.iter().take(6).map(file_delta).collect();
        if !files.is_empty() {
            let _ = write!(line, ": {}", files.join(", "));
            if source.files.len() > 6 {
                let _ = write!(line, " +{} more", source.files.len() - 6);
            }
        }
        cut(&line, 400)
    });
    bounded(
        "source_deltas:".to_owned(),
        lines,
        comparison.sources.len(),
        DETAIL_LAYERS,
        DETAIL_DELTAS_BYTES,
        |left| format!("  {}", more(left, "sources", CURRENT_VIEW_TOOL)),
    )
}

fn file_delta(file: &FileDelta) -> String {
    match file.since.as_str() {
        "changed" | "new" => format!(
            "{} {} +{} −{}",
            value(&file.path),
            file.since,
            file.added,
            file.removed
        ),
        other => format!("{} {other}", value(&file.path)),
    }
}

fn changed_section(review: &ReviewView) -> Vec<String> {
    let (Some(comparison), Some(current)) = (
        &review.comparison,
        review
            .stack
            .as_ref()
            .and_then(|stack| stack.current)
            .or_else(|| {
                review
                    .comparison
                    .as_ref()
                    .and_then(|comparison| comparison.layers.first())
                    .map(|layer| layer.number)
            }),
    ) else {
        return Vec::new();
    };
    let Some(layer) = comparison
        .layers
        .iter()
        .find(|layer| layer.number == current)
    else {
        return Vec::new();
    };
    if layer.files.is_empty() {
        return Vec::new();
    }
    bounded(
        format!("changed_since: (#{current})"),
        layer
            .files
            .iter()
            .map(|file| format!("  {}", cut(&file_delta(file), 300))),
        layer.files.len(),
        DETAIL_CHANGED_FILES,
        DETAIL_BUDGET / 3,
        |left| format!("  {}", more(left, "files", "code_review.list_files")),
    )
}

fn hidden_section(review: &ReviewView) -> Vec<String> {
    let Some(comparison) = &review.comparison else {
        return Vec::new();
    };
    if comparison.hidden_files == 0 {
        return Vec::new();
    }
    vec![format!(
        "hidden_files: {} unchanged since the baseline (code_review.list_files lists them; \
         at most {DETAIL_HIDDEN_FILES} are named in {CURRENT_VIEW_TOOL})",
        comparison.hidden_files
    )]
}

fn pull_section(review: &ReviewView) -> Vec<String> {
    let Some(pull) = &review.pull_request else {
        return Vec::new();
    };
    let mut line = format!(
        "pull_request_state: #{} {}  approvals={}  changes_requested={}  checks={}",
        pull.number, pull.state, pull.approvals, pull.changes_requested, pull.checks
    );
    if !pull.failing.is_empty() {
        let failing: Vec<String> = pull
            .failing
            .iter()
            .take(10)
            .map(|name| value(name))
            .collect();
        let _ = write!(line, "  failing={}", failing.join(","));
    }
    vec![line]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn huge_comparison(layers: Vec<LayerView>) -> ComparisonView {
        ComparisonView {
            baseline: "last_looked".to_owned(),
            view: "changes".to_owned(),
            only_changed: true,
            marks: None,
            layers,
            sources: (0..300)
                .map(|index| SourceResult {
                    id: format!("source-{index}"),
                    label: format!("branch {index} {}", "é".repeat(100)),
                    state: "changed".to_owned(),
                    from: Some("a".repeat(40)),
                    to: "b".repeat(40),
                    files: (0..200)
                        .map(|file| FileDelta {
                            path: format!("src/{}/f{file}.rs", "日本".repeat(30)),
                            since: "changed".to_owned(),
                            added: 3,
                            removed: 1,
                        })
                        .collect(),
                    files_omitted: 0,
                })
                .collect(),
            pushes: Some(Listed::of(
                (1..=20).map(|number| PushView {
                    number,
                    at_ms: 1_000 * number as u64,
                    summary: "#101 changed".to_owned(),
                }),
                20,
            )),
            hidden_files: 14,
        }
    }

    fn huge_review() -> ViewContext {
        let layers: Vec<LayerView> = (1..=200)
            .map(|number| LayerView {
                number,
                state: "changed".to_owned(),
                from: Some("a".repeat(40)),
                to: "b".repeat(40),
                files: (0..50)
                    .map(|index| FileDelta {
                        path: format!("src/{}/{index}.rs", "deep".repeat(20)),
                        since: "changed".to_owned(),
                        added: 3,
                        removed: 1,
                    })
                    .collect(),
                files_omitted: 0,
                hidden: false,
                version: Some((2, 4)),
            })
            .collect();
        ViewContext {
            screen: "review".to_owned(),
            repo: "/repo".to_owned(),
            review: Some(ReviewView {
                id: Some("rv".to_owned()),
                scope_id: "rv".to_owned(),
                title: "T\u{7}\n".repeat(400),
                kind: "stack_step_through".to_owned(),
                sources: Listed::of(
                    (1..=200).map(|number| SourceView {
                        id: format!("source-{number}"),
                        label: format!("#{number}"),
                        kind: "stack_layer".to_owned(),
                        number: Some(number),
                        branch: Some("feature/".repeat(30)),
                        head: Some("c".repeat(40)),
                        included: true,
                    }),
                    1_000,
                ),
                stack: Some(StackView {
                    layers: (1..=200).collect(),
                    current: Some(7),
                    view: "one_pr".to_owned(),
                }),
                comparison: Some(huge_comparison(layers)),
                files: (5_000, 10),
                file: Some(FileView {
                    path: "p".repeat(2_000),
                    source_id: "source-7".to_owned(),
                    status: "modified".to_owned(),
                    viewed: "no".to_owned(),
                    since: Some("changed +3 −1".to_owned()),
                }),
                cursor: Some(CursorView {
                    place: "src/a.rs:40-48".to_owned(),
                    line: Some(48),
                    side: Some("new".to_owned()),
                    range: Some((40, 48)),
                }),
                selection: Some(Listed::of(
                    (0..5_000).map(|index| format!("+ line {index} {}", "x".repeat(300))),
                    10_000,
                )),
                threads: ThreadsView {
                    open: 300,
                    resolved: 3,
                    on_file: Listed::of(
                        (0..300).map(|index| format!("thread {index} {}", "y".repeat(300))),
                        1_000,
                    ),
                    focused: Some(FocusedThread {
                        id: "t".repeat(300),
                        origin: "local".to_owned(),
                        resolved: false,
                        comments: Listed::of(
                            (0..40).map(|index| format!("me: {index} {}", "z".repeat(600))),
                            12,
                        ),
                    }),
                },
                pull_request: Some(PullView {
                    number: 7,
                    state: "open".to_owned(),
                    approvals: 1,
                    changes_requested: 0,
                    checks: "failing 2/14".to_owned(),
                    failing: (0..40).map(|index| format!("check-{index}")).collect(),
                }),
                loading: false,
            }),
            ..ViewContext::default()
        }
    }

    #[test]
    fn blocks_at_maximum_size_stay_in_bounds_and_pass_turn_context_validation() {
        let context = huge_review();
        let blocks = context.blocks();
        assert_eq!(blocks.len(), 2);
        assert!(
            blocks[0].text.len() <= VIEW_BUDGET,
            "{}",
            blocks[0].text.len()
        );
        assert!(
            blocks[1].text.len() <= DETAIL_BUDGET,
            "{}",
            blocks[1].text.len()
        );
        assert!(!blocks[0].request_only && blocks[1].request_only);
        let view = &blocks[0].text;
        assert!(
            view.lines()
                .last()
                .is_some_and(|line| line.starts_with("more:")),
            "{view}"
        );
        assert!(
            view.contains("more sources (code_review.summary)"),
            "{view}"
        );
        assert!(view.contains("more layers"), "{view}");
        // Every line is single-line text with control characters escaped.
        assert!(!view.contains('\u{7}'));
        // As Bcode checks it.
        let mut turn = bcode_session_models::TurnContext::default();
        for block in &blocks {
            let target = if block.request_only {
                &mut turn.request_only
            } else {
                &mut turn.blocks
            };
            target.push(bcode_session_models::TurnContextBlock::new(
                block.label.clone(),
                block.text.clone(),
            ));
        }
        assert_eq!(turn.validate(), Ok(()));
        let detail = &blocks[1].text;
        assert!(
            detail.contains("more selected lines (code_review.file_diff)"),
            "{detail}"
        );
        assert!(
            detail.contains("more threads (code_review.list_threads"),
            "{detail}"
        );
        assert!(
            detail.contains("more comments (code_review.get_thread"),
            "{detail}"
        );
    }

    #[test]
    fn the_tool_answer_is_always_valid_bounded_json_that_says_what_it_left_out() {
        let context = huge_review();
        for arguments in ["", r#"{"max_files":2}"#, r#"{"sections":["comparison"]}"#] {
            let answer = context.tool_answer(arguments).expect("answer");
            assert!(answer.len() <= TOOL_BUDGET, "{} bytes", answer.len());
            let value: serde_json::Value = serde_json::from_str(&answer).expect("valid JSON");
            assert_eq!(value["screen"], "review");
            assert!(
                value["truncated"].is_object(),
                "{arguments}: says what was cut"
            );
        }
        // A small answer is whole and unmarked; `max_files` bounds each layer's files.
        let mut small = huge_review();
        let review = small.review.as_mut().expect("review");
        review.selection = None;
        review.threads = ThreadsView::default();
        review.sources = Listed::of(Vec::new(), 1);
        let comparison = review.comparison.as_mut().expect("comparison");
        comparison.layers.truncate(2);
        comparison.sources.truncate(1);
        review.file = None;
        let answer = small.tool_answer(r#"{"max_files":3}"#).expect("answer");
        let value: serde_json::Value = serde_json::from_str(&answer).expect("valid JSON");
        assert!(
            value["review"]["comparison"]["sources"][0]["files"]
                .as_array()
                .is_some_and(|files| files.len() == 3),
            "{answer}"
        );
        // A shortened files list says how many it left out.
        assert_eq!(
            value["review"]["comparison"]["sources"][0]["files_omitted"], 197,
            "{answer}"
        );
        assert_eq!(
            value["review"]["comparison"]["layers"][0]["files_omitted"], 47,
            "{answer}"
        );
        // Files left out before the answer was built count too.
        let mut capped = small.clone();
        let layer = &mut capped
            .review
            .as_mut()
            .and_then(|review| review.comparison.as_mut())
            .expect("comparison")
            .layers[0];
        layer.files.truncate(10);
        layer.files_omitted = 40;
        let answer = capped.tool_answer(r#"{"max_files":3}"#).expect("answer");
        let value: serde_json::Value = serde_json::from_str(&answer).expect("valid JSON");
        assert_eq!(
            value["review"]["comparison"]["layers"][0]["files_omitted"], 47,
            "{answer}"
        );
        let whole = capped.tool_answer("").expect("answer");
        let value: serde_json::Value = serde_json::from_str(&whole).expect("valid JSON");
        assert_eq!(
            value["review"]["comparison"]["layers"][0]["files_omitted"], 40,
            "{whole}"
        );
        assert!(
            value["review"]["comparison"]["layers"][1]
                .get("files_omitted")
                .is_none(),
            "a whole list has no count: {whole}"
        );
        assert_eq!(
            value["review"]["comparison"]["pushes"]["items"][0]["number"], 1,
            "{answer}"
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(
                &small
                    .tool_answer(r#"{"max_files":"x"}"#)
                    .unwrap_or_default()
            )
            .is_err(),
            "bad arguments are refused"
        );
    }

    #[test]
    fn a_detached_context_names_only_the_screen_and_review() {
        let mut context = huge_review();
        context.detached = true;
        let blocks = context.blocks();
        assert_eq!(blocks.len(), 1, "no detail block");
        let view = &blocks[0].text;
        assert!(view.contains("context: detached"), "{view}");
        assert!(!view.contains("cursor:"), "{view}");
        assert!(!view.contains("comparison:"), "{view}");
    }

    #[test]
    fn values_are_plain_or_quoted_and_cut() {
        assert_eq!(value("src/retry.rs"), "src/retry.rs");
        assert_eq!(value("Retry backoff"), "\"Retry backoff\"");
        assert_eq!(value("a\nb"), "\"a\\nb\"");
        let long = value(&"é".repeat(400));
        assert!(long.len() <= MAX_VALUE_BYTES + 2, "{}", long.len());
        assert!(long.trim_end_matches('"').ends_with('…'), "{long}");
    }
}
