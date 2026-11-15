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
use crate::context::UiContext;
use crate::loader::Message;
use bcode_code_review::document::{FileRef, ItemKey};
use bcode_code_review_models::{
    REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION, ReviewAssistantBinding, ReviewLineSide,
    ReviewWorkspace,
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
];
/// The tool call that moves the reviewer's view.
const NAVIGATE_TOOL: &str = "code_review.navigate";

/// Result of the review's background work for reflected tools.
#[derive(Debug)]
pub enum ToolsResult {
    /// The review's stored viewed state, read after a tool changed it.
    Viewed(Result<Option<Box<ReviewWorkspace>>, String>),
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
            || (Vec::new(), Vec::new()),
            |review| {
                let mut resolved = Vec::new();
                let mut stacks = Vec::new();
                for group in review.doc.groups() {
                    resolved.push(group.source.clone());
                    if let Some(stack) = group.stack
                        && !stacks.iter().any(
                            |known: &bcode_code_review_models::ResolvedReviewStack| {
                                known.source_id == stack.stack.source_id
                            },
                        )
                    {
                        stacks.push(stack.stack.clone());
                    }
                }
                (resolved, stacks)
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
            self.reload_viewed(host);
        }
        if let Some(arguments) = navigate {
            self.navigate_to(&arguments);
        }
    }

    /// A finished piece of reflection work.
    pub(super) fn finish_tools(&mut self, result: ToolsResult) {
        match result {
            ToolsResult::Viewed(Ok(None)) => {}
            ToolsResult::Viewed(Ok(Some(stored))) => self.apply_stored_viewed(&stored),
            ToolsResult::Viewed(Err(error)) => {
                self.toasts.push(
                    ToastSeverity::Error,
                    "Could not reload viewed files",
                    Some(error),
                );
            }
        }
    }

    /// Read the review's stored viewed state again.
    fn reload_viewed(&mut self, host: &UiContext) {
        let workspace = self.workspace_mut().clone();
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
            inbox.push(Message::Tools(Box::new(ToolsResult::Viewed(result))));
        }));
    }

    /// Show viewed files as stored.
    fn apply_stored_viewed(&mut self, stored: &ReviewWorkspace) {
        let workspace = self.workspace_mut();
        if workspace.viewed_files == stored.viewed_files
            && workspace.viewed_marks == stored.viewed_marks
            && workspace.viewed_threads == stored.viewed_threads
        {
            return;
        }
        workspace.viewed_files.clone_from(&stored.viewed_files);
        workspace.viewed_marks.clone_from(&stored.viewed_marks);
        workspace.viewed_threads.clone_from(&stored.viewed_threads);
        let workspace = workspace.clone();
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
        let Some(review) = self.review.as_mut() else {
            return;
        };
        let path = path.trim().trim_start_matches("./").to_owned();
        let file: Option<FileRef> = review
            .doc
            .file_keys()
            .find(|file| {
                file.path == path
                    && target
                        .source
                        .as_deref()
                        .is_none_or(|source| file.source_id == source)
            })
            .cloned();
        let Some(file) = file else {
            return;
        };
        let side = if target.side.as_deref() == Some("old") {
            ReviewLineSide::Old
        } else {
            ReviewLineSide::New
        };
        let key = target.line.map_or_else(
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
        let key = if review.doc.index_of(&key).is_some() || target.line.is_none() {
            key
        } else {
            ItemKey::FileHeader(file.clone())
        };
        super::annotate::jump_to_annotation(review, &key);
        let label = target
            .line
            .map_or_else(|| file.path.clone(), |line| format!("{}:{line}", file.path));
        self.set_status(
            format!("the assistant showed {label}"),
            StatusSeverity::Info,
        );
    }
}
