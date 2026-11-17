//! The review's side of the app-wide assistant and of AI card sessions.
//!
//! * **Place.** [`ReviewSurface::assistant_review_place`] tells the assistant panel which review
//!   is on screen: its id, title, current stack layer, the cursor's place, and the binding that
//!   attaches a session to it (the review's workspace and the revisions its open document
//!   resolved to, so tools read exactly what the review shows).
//! * **Opening the panel.** The review's `assistant_toggle` action (a key or the palette) asks
//!   the app to open, focus or close the panel ([`ReviewSurface::take_assistant_request`]).
//! * **Live reflection.** When a `code_review.*` tool that changes the review finishes in a
//!   followed AI card session, or in the assistant's session (handed over by the app,
//!   [`ReviewSurface::apply_assistant_tools`]), the review reloads its comments and viewed files
//!   from the store, as for the reviewer's own edits; a finished `code_review.navigate` moves the
//!   cursor there. Card tool calls already finished when a session is first observed are not
//!   replayed.

use super::ReviewSurface;
use crate::action::Action;
use crate::context::UiContext;
use crate::loader::Message;
use bcode_code_review::document::{FileRef, ItemKey};
use bcode_code_review_models::{
    REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION, ReviewAssistantBinding, ReviewAssistantStacks,
    ReviewLineSide, ReviewWorkspace,
};
use bmux_tui_components::status_bar::StatusSeverity;
use bmux_tui_components::toast_stack::ToastSeverity;
use std::collections::{BTreeMap, BTreeSet};

/// Tool calls that change what the review shows.
const WRITE_TOOLS: &[&str] = &[
    "code_review.add_comment",
    "code_review.reply",
    "code_review.resolve_thread",
    "code_review.suggest_change",
    "code_review.mark_viewed",
    "code_review.add_comments",
    "code_review.resolve_threads",
];
/// Tool calls that change the saved reviews home lists.
pub const REVIEW_LIST_TOOLS: &[&str] = &[
    "code_review.create_review",
    "code_review.update_review",
    "code_review.delete_review",
];
/// The tool call that moves the reviewer's view.
const NAVIGATE_TOOL: &str = "code_review.navigate";

/// Result of the review's background work for reflected tools.
#[derive(Debug)]
pub enum ToolsResult {
    /// The review's stored record, read after a tool (or another app) may have changed it,
    /// with the count of the review's own writes taken when the read started.
    Stored {
        /// The record read, `None` when the review no longer exists.
        result: Result<Option<Box<ReviewWorkspace>>, String>,
        /// [`super::viewed_sync::Tracking::writes`] when the read started.
        writes: u64,
    },
}

/// Card sessions followed, with the finished tool calls already applied.
#[derive(Debug, Default)]
pub struct Reflection {
    applied: BTreeMap<String, BTreeSet<String>>,
}

impl Reflection {
    /// Follow a session this review just created: every review tool call it finishes is a reply
    /// to the review's question, so none of them is skipped as already applied.
    pub(super) fn follow_new_session(&mut self, session: &str) {
        self.applied.entry(session.to_owned()).or_default();
    }
}

/// Where a navigation tool call points.
#[derive(Debug, Default, serde::Deserialize)]
struct NavigateArguments {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    side: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

impl ReviewSurface {
    /// What the screen asked of the assistant since the last call.
    pub fn take_assistant_requests(&mut self) -> Vec<crate::assistant::LinkRequest> {
        self.link.take_requests()
    }

    /// The review another review should replace on screen (a folder of the file panel
    /// reviewed as it is now), asked for since the last call.
    pub const fn take_start_request(&mut self) -> Option<crate::entry::Entry> {
        self.leaving.start.take()
    }

    /// The review's `assistant_toggle` action ran since the last call.
    pub const fn take_assistant_request(&mut self) -> bool {
        self.link.take_request()
    }

    /// The review on screen for the assistant panel; with `binding` the binding that attaches a
    /// session to it (cloning the workspace and resolved sources).
    pub fn assistant_review_place(&mut self, binding: bool) -> crate::assistant::ReviewPlace {
        let workspace_id = self
            .workspace()
            .map(bcode_code_review::scope::annotation_workspace_id)
            .unwrap_or_default();
        crate::assistant::ReviewPlace {
            workspace_id,
            title: self.title.clone(),
            layer: self.current_layer_number(),
            chip: self.assistant_place(),
            generation: self.generation,
            binding: binding.then(|| Box::new(self.assistant_binding(String::new()))),
        }
    }

    /// The binding of this review for `session_id`: its workspace and the revisions its open
    /// document resolved to.
    pub(super) fn assistant_binding(&mut self, session_id: String) -> ReviewAssistantBinding {
        let workspace = self.workspace_mut().clone();
        let (resolved, stacks) = self.review.as_ref().map_or_else(
            || (Vec::new(), ReviewAssistantStacks::default()),
            |review| {
                let mut resolved = Vec::new();
                let mut stacks: Vec<&bcode_code_review_models::ResolvedReviewStack> = Vec::new();
                let mut seen = BTreeSet::new();
                for group in review.doc.groups() {
                    resolved.push(group.source.clone());
                    if let Some(stack) = group.stack
                        && seen.insert(stack.stack.source_id.as_str())
                    {
                        stacks.push(&stack.stack);
                    }
                }
                (resolved, ReviewAssistantStacks::from_stacks(stacks))
            },
        );
        ReviewAssistantBinding {
            schema_version: REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION,
            session_id,
            workspace,
            resolved,
            stacks,
            created_at_ms: 0,
            updated_at_ms: 0,
            started: None,
            detached_at_ms: None,
        }
    }

    /// The cursor's place as `path:line`, `path:start-end`, or `path`.
    pub(super) fn assistant_place(&self) -> Option<String> {
        let review = self.review.as_ref()?;
        let anchor = review.selection.as_ref().map_or_else(
            || {
                review
                    .cursor
                    .as_ref()
                    .and_then(|cursor| review.doc.anchor_for(cursor))
            },
            |selection| review.doc.range_anchor(&selection.anchor, &selection.end),
        )?;
        anchor.file.as_ref()?;
        Some(bcode_code_review::assistant::location_label(
            &anchor.to_draft_anchor(),
        ))
    }

    /// Apply finished `code_review.*` tool calls of followed card sessions: reload what the
    /// writes changed and follow navigation. Calls already finished when a session is first
    /// observed are only recorded, unless the review created the session itself (a question's
    /// own session); then every call answers the review's messages and is applied.
    pub(super) fn reflect_review_tools(&mut self, host: &UiContext) {
        let mut sessions: Vec<String> = self
            .notes
            .exchanges()
            .iter()
            .filter_map(|exchange| exchange.session_id.clone())
            .collect();
        sessions.sort();
        sessions.dedup();
        let mut wrote = false;
        let mut viewed = false;
        let mut navigate = None;
        for session in &sessions {
            let Some(state) = self.agents.state(session) else {
                continue;
            };
            if state.phase == crate::agent::AgentPhase::Waiting {
                continue;
            }
            let finished: Vec<&crate::agent::feed::ToolLine> = state
                .feed
                .turns
                .iter()
                .flat_map(|turn| &turn.items)
                .filter_map(|item| match item {
                    crate::agent::feed::FeedItem::Tool(tool)
                        if tool.state == crate::agent::feed::ToolState::Done
                            && tool
                                .name
                                .starts_with(crate::agent::feed::REVIEW_TOOL_PREFIX) =>
                    {
                        Some(tool)
                    }
                    _ => None,
                })
                .collect();
            let first = !self.reflection.applied.contains_key(session);
            let applied = self.reflection.applied.entry(session.clone()).or_default();
            let mut kept = BTreeSet::new();
            for tool in finished {
                kept.insert(tool.id.clone());
                if first || applied.contains(&tool.id) {
                    continue;
                }
                if WRITE_TOOLS.contains(&tool.name.as_str()) {
                    wrote = true;
                    viewed |= tool.name == "code_review.mark_viewed";
                } else if REVIEW_LIST_TOOLS.contains(&tool.name.as_str()) {
                    viewed = true;
                } else if tool.name == NAVIGATE_TOOL {
                    navigate = Some(tool.arguments.clone());
                }
            }
            // Calls that left the observed window never come back.
            *applied = kept;
        }
        self.reflection
            .applied
            .retain(|session, _| sessions.contains(session));
        self.apply_tool_effects(wrote, viewed, navigate, host);
    }

    /// Apply the assistant session's finished `code_review.*` calls (name, JSON arguments),
    /// oldest first: reload what the writes changed and follow navigation.
    pub fn apply_assistant_tools(&mut self, tools: &[(String, String)], host: &UiContext) {
        let mut wrote = false;
        let mut viewed = false;
        let mut navigate = None;
        for (name, arguments) in tools {
            if WRITE_TOOLS.contains(&name.as_str()) {
                wrote = true;
                viewed |= name == "code_review.mark_viewed";
            } else if REVIEW_LIST_TOOLS.contains(&name.as_str()) {
                // A rename, archive or delete of any review may be this one's.
                viewed = true;
            } else if name == NAVIGATE_TOOL {
                navigate = Some(arguments.clone());
            }
        }
        self.apply_tool_effects(wrote, viewed, navigate, host);
    }

    fn apply_tool_effects(
        &mut self,
        wrote: bool,
        viewed: bool,
        navigate: Option<String>,
        host: &UiContext,
    ) {
        if wrote {
            self.notes_requested = false;
        }
        if viewed {
            self.reload_stored(host);
        }
        if let Some(arguments) = navigate {
            self.navigate_to(&arguments);
        }
    }

    /// A finished piece of reflection work.
    pub(super) fn finish_tools(&mut self, result: ToolsResult, host: &UiContext) {
        let ToolsResult::Stored { result, writes } = result;
        if writes != self.viewed_sync.writes {
            // A save of the review's own was taken while this read ran, so the read may hold
            // the record from before it; read again rather than undo the save on screen.
            self.reload_stored(host);
            return;
        }
        match result {
            Ok(None) => {
                if !self.is_transient() {
                    self.deleted_elsewhere();
                }
            }
            Ok(Some(stored)) => {
                if self.is_transient() {
                    self.apply_stored_viewed(&stored);
                } else {
                    let base = self
                        .viewed_sync
                        .stored
                        .clone()
                        .unwrap_or_else(|| self.workspace_mut().clone());
                    self.rebase_onto(&base, *stored);
                }
            }
            Err(error) => {
                self.toasts.push(
                    ToastSeverity::Error,
                    "Could not reload the review",
                    Some(error),
                );
            }
        }
    }

    /// Read the review's stored record again (viewed state, title, archive, or its deletion).
    pub fn reload_stored(&mut self, host: &UiContext) {
        if self.viewed_sync.deleted {
            return;
        }
        let workspace = self.workspace_mut().clone();
        let writes = self.viewed_sync.writes;
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        host.spawn(Box::pin(async move {
            let result = services
                .call(bcode_code_review_models::GetReviewWorkspaceRequest {
                    repo_path: workspace.repo_root.clone(),
                    workspace_id: bcode_code_review::scope::annotation_workspace_id(&workspace),
                })
                .await
                .map(|response| response.workspace.map(Box::new))
                .map_err(|error| error.message);
            inbox.push(Message::Tools(Box::new(ToolsResult::Stored {
                result,
                writes,
            })));
        }));
    }

    /// Show viewed files as stored, keeping the reviewer's changes not saved yet (an unsaved
    /// review, whose stored record holds only its viewed state).
    fn apply_stored_viewed(&mut self, stored: &ReviewWorkspace) {
        let stored = super::viewed_sync::Viewed::of(stored);
        let local = super::viewed_sync::Viewed::of(self.workspace_mut());
        let merged = self.viewed_sync.base.as_ref().map_or_else(
            || stored.clone(),
            |base| super::viewed_sync::merge(base, &local, &stored),
        );
        self.viewed_sync.base = Some(stored);
        self.show_viewed(&merged);
    }

    /// A save of `saving` wrote `written`: take what the store merged in, keeping changes the
    /// reviewer made while the save ran.
    pub(super) fn adopt_written(&mut self, saving: &ReviewWorkspace, written: ReviewWorkspace) {
        self.viewed_sync.writes += 1;
        if self.is_transient() {
            let local = super::viewed_sync::Viewed::of(self.workspace_mut());
            let written = super::viewed_sync::Viewed::of(&written);
            let merged = super::viewed_sync::merge(
                &super::viewed_sync::Viewed::of(saving),
                &local,
                &written,
            );
            self.viewed_sync.base = Some(written);
            self.show_viewed(&merged);
        } else {
            self.rebase_onto(saving, written);
        }
    }

    /// Rebase the review's copy onto `stored` from `base` (what the copy was last based on):
    /// what the reviewer changed since `base` stays, everything else becomes what is stored.
    fn rebase_onto(&mut self, base: &ReviewWorkspace, stored: ReviewWorkspace) {
        let local = self.workspace_mut().clone();
        let merged = bcode_code_review::workspace_merge::merge_workspace(base, &local, &stored);
        self.viewed_sync.base = Some(super::viewed_sync::Viewed::of(&stored));
        self.viewed_sync.stored = Some(stored);
        let renamed = merged.title != self.title;
        let workspace = self.workspace_mut();
        workspace.title.clone_from(&merged.title);
        workspace.archived_at_ms = merged.archived_at_ms;
        workspace.sources.clone_from(&merged.sources);
        workspace
            .reviewed_revisions
            .clone_from(&merged.reviewed_revisions);
        workspace.presentation.clone_from(&merged.presentation);
        workspace
            .stack_snapshots
            .clone_from(&merged.stack_snapshots);
        workspace.updated_at_ms = merged.updated_at_ms;
        if renamed {
            self.title.clone_from(&merged.title);
            self.set_status(
                format!("renamed to “{}”", merged.title),
                StatusSeverity::Info,
            );
        }
        self.show_viewed(&super::viewed_sync::Viewed::of(&merged));
    }

    /// The saved review was deleted elsewhere: stop saving and leave it.
    pub(super) const fn deleted_elsewhere(&mut self) {
        if std::mem::replace(&mut self.viewed_sync.deleted, true) {
            return;
        }
        self.pending_action = Some(Action::Close {
            edit_sources: false,
        });
    }

    /// Whether the saved review was deleted while it was open (the review then closes).
    #[must_use]
    pub const fn deleted(&self) -> bool {
        self.viewed_sync.deleted
    }

    /// Show `viewed` as the review's viewed state.
    fn show_viewed(&mut self, viewed: &super::viewed_sync::Viewed) {
        let workspace = self.workspace_mut();
        if super::viewed_sync::Viewed::of(workspace) == *viewed {
            return;
        }
        viewed.apply(workspace);
        let workspace = workspace.clone();
        if workspace.id == super::TRANSIENT_WORKSPACE_ID {
            self.viewed_sync.transient = Some((
                workspace.viewed_files.clone(),
                workspace.viewed_marks.clone(),
                workspace.viewed_threads.clone(),
            ));
        }
        if let Some(review) = self.review.as_mut() {
            review.doc.set_viewed_marks(&workspace);
            review.note_splices();
        }
        self.refresh_threads();
    }

    /// Move the cursor to where a navigation tool call pointed.
    fn navigate_to(&mut self, arguments: &str) {
        let Ok(target) = serde_json::from_str::<NavigateArguments>(arguments) else {
            return;
        };
        let Some(path) = target.path.filter(|path| !path.trim().is_empty()) else {
            return;
        };
        let given = |text: Option<String>| text.filter(|text| !text.trim().is_empty());
        if let Ok(label) = self.show_location(
            &path,
            target.line,
            given(target.side).as_deref(),
            given(target.source).as_deref(),
        ) {
            self.set_status(
                format!("the assistant showed {label}"),
                StatusSeverity::Info,
            );
        }
    }

    /// Move the cursor to `path` (at `line` on `side`, `new` unless `old`), revealing hidden
    /// lines when needed. Returns the place shown.
    pub(super) fn show_location(
        &mut self,
        path: &str,
        line: Option<u32>,
        side: Option<&str>,
        source: Option<&str>,
    ) -> Result<String, String> {
        let file = self.find_file(path, source, None)?;
        Ok(self.show_file_location(&file, line, side))
    }

    /// The file `path` (of `source` when given) names; with `within`, only one of that source
    /// (the stack layer that will be shown alone).
    pub(super) fn find_file(
        &self,
        path: &str,
        source: Option<&str>,
        within: Option<&str>,
    ) -> Result<FileRef, String> {
        let review = self.review.as_ref().ok_or("the review is still loading")?;
        let path = path.trim().trim_start_matches("./").to_owned();
        let mut named: Vec<&FileRef> = review
            .doc
            .file_keys()
            .filter(|file| {
                file.path == path && source.is_none_or(|source| file.source_id == source)
            })
            .collect();
        if named.is_empty() {
            return Err(format!(
                "{path} is not a file shown in this review (code_review.list_files lists them)"
            ));
        }
        if let Some(within) = within {
            named.retain(|file| file.source_id == within);
        }
        named
            .first()
            .map(|file| (*file).clone())
            .ok_or_else(|| format!("{path} is not in the layer shown; name its layer with `layer`"))
    }

    /// Move the cursor to `file` (at `line` on `side` when given, else its header) and say
    /// where it went.
    pub(super) fn show_file_location(
        &mut self,
        file: &FileRef,
        line: Option<u32>,
        side: Option<&str>,
    ) -> String {
        let Some(review) = self.review.as_mut() else {
            return file.path.clone();
        };
        let file = file.clone();
        let side = if side == Some("old") {
            ReviewLineSide::Old
        } else {
            ReviewLineSide::New
        };
        let key = line.map_or_else(
            || ItemKey::FileHeader(file.clone()),
            |line| ItemKey::Line {
                file: file.clone(),
                side,
                line,
            },
        );
        if let ItemKey::Line { line, .. } = &key
            && review.doc.index_of(&key).is_none()
            && review.doc.reveal_lines(&file, *line, *line)
        {
            review.note_splices();
        }
        let key = if review.doc.index_of(&key).is_some() || line.is_none() {
            key
        } else {
            ItemKey::FileHeader(file.clone())
        };
        super::annotate::jump_to_annotation(review, &key);
        line.map_or_else(|| file.path.clone(), |line| format!("{}:{line}", file.path))
    }
}
