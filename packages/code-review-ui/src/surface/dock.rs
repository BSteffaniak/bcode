//! The review assistant: a docked chat with a Bcode session working on this review.
//!
//! * **Dock.** `ctrl+k` opens the dock and focuses it, focuses it when it is open elsewhere, and
//!   closes it when it is focused. It sits to the right of the document on wide bodies and under
//!   it otherwise ([`super::super::panels`]); its divider resizes it, and a click focuses it.
//! * **Session.** The first message creates a session in the repository and binds it to the
//!   review (`review.assistant.bind`), so the code review plugin's `code_review.*` tools act on
//!   this review; reopening the review finds the newest bound session and continues it. The
//!   binding records the revisions the review's sources resolved to and is refreshed after the
//!   review reopens.
//! * **Conversation.** The dock follows the session like an AI card ([`crate::agent`]): every
//!   observed turn with the question, reasoning, tool lines and answers, a pending permission
//!   prompt (answered with the feed keys, `y`/`a`/`n`/`d`), and a status line while it works.
//! * **Context chips.** The header row names the review and the place the cursor is (its line,
//!   range selection or file); while that chip is attached (`ctrl+x` detaches it), a message
//!   carries it as a short context block after the text.
//! * **Live reflection.** When a `code_review.*` tool that changes the review finishes in a
//!   followed session, the review reloads its comments and viewed files from the store, as for
//!   the reviewer's own edits; a finished `code_review.navigate` moves the cursor there. Tool
//!   calls already finished when a session is first observed are not replayed.

use super::{Focus, ReviewSurface};
use crate::action::Action;
use crate::context::UiContext;
use crate::loader::Message;
use crate::rows::{ActionCells, AgentAction, ItemAction};
use bcode_code_review::document::{FileRef, ItemKey};
use bcode_code_review_models::{
    BindReviewAssistantRequest, GetReviewAssistantRequest, REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION,
    ReviewAssistantBinding, ReviewAssistantLookup, ReviewLineSide, ReviewWorkspace,
};
use bmux_keyboard::KeyStroke;
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutNode, LogicalSize,
};
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::{Point, Rect, Size};
use bmux_tui::paint::PaintCx;
use bmux_tui::text::{Line, Span, Text};
use bmux_tui_components::status_bar::StatusSeverity;
use bmux_tui_components::text_input::{
    EdgePolicy, TextInputControl, TextInputOutcome, TextInputPolicy, TextInputState,
};
use bmux_tui_components::toast_stack::ToastSeverity;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Keymap scope of the assistant dock.
pub const ASSISTANT_SCOPE: &str = "code-review-assistant";
/// Pane of the assistant dock.
pub const ASSISTANT_PANE_ID: &str = "review.assistant";
/// The dock's conversation.
pub const ASSISTANT_FEED_ID: &str = "review.assistant.feed";
/// The dock's message input.
pub const ASSISTANT_INPUT_ID: &str = "review.assistant.input";
/// Rows of the message input at most.
const INPUT_MAX_ROWS: u16 = 6;
/// Rows a wheel step scrolls the conversation.
const WHEEL_ROWS: usize = 3;
/// Bytes of a message the dock sends at most.
const MAX_MESSAGE_BYTES: usize = 32 * 1024;

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

/// Result of the dock's background work.
#[derive(Debug)]
pub enum DockResult {
    /// The newest session bound to this review, when there is one, looked up during
    /// conversation `epoch`.
    Found {
        /// The conversation the lookup was made for.
        epoch: u64,
        /// The session found.
        session: Option<String>,
    },
    /// A session was created, bound, and sent the first message of conversation `epoch` (or
    /// why not).
    Started {
        /// The conversation the session was started for.
        epoch: u64,
        /// The session, or why it could not be started.
        result: Result<String, String>,
    },
    /// A message was sent (or why not).
    Sent(Result<(), String>),
    /// The review's stored viewed state, read after a tool changed it.
    Viewed(Result<Option<Box<ReviewWorkspace>>, String>),
}

/// Whether the newest bound session was looked up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lookup {
    NotStarted,
    Running,
    Done,
}

/// The conversation's rows at one width.
#[derive(Debug)]
struct Rows {
    key: (u64, u64, u64, u16),
    lines: Vec<Line>,
    actions: Vec<ActionCells<ItemAction>>,
}

/// State of the assistant dock.
#[derive(Debug)]
pub struct Dock {
    session: Option<String>,
    lookup: Lookup,
    /// A session is being created; messages wait in `queued`.
    starting: bool,
    /// Messages waiting for a session, with the conversation they were written in.
    queued: VecDeque<(u64, String)>,
    /// The current conversation. A new conversation (`ctrl+n`) starts a new one, so lookups and
    /// session starts of an earlier conversation never replace the current one.
    epoch: u64,
    /// Document generation the session was last bound for.
    bound_generation: Option<u64>,
    input: RefCell<TextInputState>,
    policy: TextInputPolicy,
    /// Whether messages carry the cursor's place.
    pub context: bool,
    /// Rows scrolled up from the newest row.
    scroll: Cell<usize>,
    rows: RefCell<Option<Rows>>,
    /// Rows shown in the last frame: `(first row, row count)`.
    shown: Cell<(usize, usize)>,
    pub(super) pane: Cell<bmux_tui_components::pane::PaneState>,
    /// Finished tool calls already applied, per followed session.
    applied: BTreeMap<String, BTreeSet<String>>,
    revision: u64,
}

impl Default for Dock {
    fn default() -> Self {
        let mut policy = TextInputPolicy::chat_composer();
        policy.viewport.min_rows = 1;
        policy.viewport.max_rows = Some(INPUT_MAX_ROWS);
        policy.edge = EdgePolicy::disabled();
        Self {
            session: None,
            lookup: Lookup::NotStarted,
            starting: false,
            queued: VecDeque::new(),
            epoch: 0,
            bound_generation: None,
            input: RefCell::new(TextInputState::new(bmux_text_edit::TextEditBuffer::new())),
            policy,
            context: true,
            scroll: Cell::new(0),
            rows: RefCell::new(None),
            shown: Cell::new((0, 0)),
            pane: Cell::new(bmux_tui_components::pane::PaneState::new(Rect::default())),
            applied: BTreeMap::new(),
            revision: 0,
        }
    }
}

impl Dock {
    /// The bound session, once there is one.
    #[must_use]
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// The message input.
    #[must_use]
    pub const fn input(&self) -> &RefCell<TextInputState> {
        &self.input
    }

    /// Editing policy of the message input.
    #[must_use]
    pub const fn policy(&self) -> &TextInputPolicy {
        &self.policy
    }

    const fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Follow a session this app just created: every review tool call it finishes is a reply
    /// to the app's messages, so none of them is skipped as already applied.
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
    /// `ctrl+k`: open and focus the dock, focus it, or close it when it is focused.
    pub(super) fn toggle_assistant(&mut self, host: &UiContext) -> Action {
        if !self.panels.assistant_open {
            self.panels.toggle_assistant();
            self.focus = Focus::Assistant;
            self.find_assistant(host);
        } else if self.focus == Focus::Assistant {
            self.panels.toggle_assistant();
            self.focus = Focus::Review;
        } else {
            self.focus = Focus::Assistant;
        }
        Action::Redraw
    }

    /// Look up the newest session bound to this review, once.
    fn find_assistant(&mut self, host: &UiContext) {
        if self.dock.lookup != Lookup::NotStarted || self.dock.session.is_some() {
            return;
        }
        self.dock.lookup = Lookup::Running;
        let epoch = self.dock.epoch;
        let workspace = self.workspace_mut().clone();
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        host.spawn(Box::pin(async move {
            let result = services
                .call(GetReviewAssistantRequest {
                    repo_path: workspace.repo_root.clone(),
                    lookup: ReviewAssistantLookup::Review {
                        workspace_id: bcode_code_review::scope::annotation_workspace_id(&workspace),
                    },
                })
                .await
                .ok()
                .and_then(|response| response.binding)
                .map(|binding| binding.session_id);
            inbox.push(Message::Dock(Box::new(DockResult::Found {
                epoch,
                session: result,
            })));
        }));
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
        }
    }

    /// A finished piece of dock work.
    pub(super) fn finish_dock(&mut self, result: DockResult, host: &UiContext) {
        self.dock.touch();
        match result {
            DockResult::Found { epoch, session } => {
                self.dock.lookup = Lookup::Done;
                if epoch == self.dock.epoch && self.dock.session.is_none() && !self.dock.starting {
                    self.dock.session = session;
                }
            }
            DockResult::Started {
                epoch,
                result: Ok(session),
            } => {
                let queued = self.take_queued(epoch);
                if epoch == self.dock.epoch {
                    self.dock.starting = false;
                    self.dock.follow_new_session(&session);
                    self.dock.session = Some(session);
                    self.dock.bound_generation = Some(self.generation);
                    for text in queued {
                        self.send_to_session(text, host);
                    }
                } else {
                    // The reviewer started a new conversation meanwhile: the earlier one keeps
                    // its session and gets the messages written for it, and the dock stays on
                    // the new conversation.
                    for text in queued {
                        self.send_to(session.clone(), None, text, host);
                    }
                }
            }
            DockResult::Started {
                epoch,
                result: Err(error),
            } => {
                if epoch == self.dock.epoch {
                    self.dock.starting = false;
                }
                drop(self.take_queued(epoch));
                self.toasts.push(
                    ToastSeverity::Error,
                    "Could not start the assistant",
                    Some(error),
                );
            }
            DockResult::Sent(Ok(())) | DockResult::Viewed(Ok(None)) => {}
            DockResult::Sent(Err(error)) => {
                self.toasts.push(
                    ToastSeverity::Error,
                    "Could not send to the assistant",
                    Some(error),
                );
            }
            DockResult::Viewed(Ok(Some(stored))) => self.apply_stored_viewed(&stored),
            DockResult::Viewed(Err(error)) => {
                self.toasts.push(
                    ToastSeverity::Error,
                    "Could not reload viewed files",
                    Some(error),
                );
            }
        }
    }

    /// Send the input's text (with the context block while the context chip is attached).
    fn send_input(&mut self, host: &UiContext) -> Action {
        let text = self.dock.input.borrow().buffer().text().to_string();
        if text.trim().is_empty() {
            return Action::None;
        }
        let mut message = text.trim_end().to_owned();
        if self.dock.context
            && let Some(context) = self.assistant_context()
        {
            message.push_str(&context);
        }
        if message.len() > MAX_MESSAGE_BYTES {
            self.set_status(
                "the message is too long for the assistant",
                StatusSeverity::Warning,
            );
            return Action::Redraw;
        }
        *self.dock.input.borrow_mut() = TextInputState::new(bmux_text_edit::TextEditBuffer::new());
        self.dock.scroll.set(0);
        self.dock.touch();
        if self.dock.starting {
            self.dock.queued.push_back((self.dock.epoch, message));
        } else if self.dock.session.is_some() {
            self.send_to_session(message, host);
        } else {
            self.start_assistant(message, host);
        }
        Action::Redraw
    }

    /// Create the assistant's session, bind it to the review, and send `message`.
    fn start_assistant(&mut self, message: String, host: &UiContext) {
        self.dock.starting = true;
        let epoch = self.dock.epoch;
        let template = self.assistant_binding(String::new());
        let title = format!("Review assistant: {}", template.workspace.title);
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        host.spawn(Box::pin(async move {
            let repo = template.workspace.repo_root.clone();
            let result = async {
                let session = services
                    .host()
                    .create_session(
                        title,
                        repo.clone(),
                        crate::host::review_session_origin(
                            crate::host::ReviewSessionPurpose::Assistant,
                            &template.workspace.id,
                        ),
                    )
                    .await
                    .map_err(|error| error.message)?;
                services
                    .call(BindReviewAssistantRequest {
                        repo_path: repo,
                        binding: ReviewAssistantBinding {
                            session_id: session.clone(),
                            ..template
                        },
                    })
                    .await
                    .map_err(|error| error.message)?;
                services
                    .host()
                    .send_message(session.clone(), message)
                    .await
                    .map_err(|error| error.message)?;
                Ok::<_, String>(session)
            }
            .await;
            inbox.push(Message::Dock(Box::new(DockResult::Started {
                epoch,
                result,
            })));
        }));
    }

    /// Send `message` to the bound session, refreshing the binding first when the review
    /// reopened since it was bound.
    fn send_to_session(&mut self, message: String, host: &UiContext) {
        let Some(session) = self.dock.session.clone() else {
            return;
        };
        let rebind = (self.dock.bound_generation != Some(self.generation))
            .then(|| self.assistant_binding(session.clone()));
        self.dock.bound_generation = Some(self.generation);
        self.send_to(session, rebind, message, host);
    }

    /// The queued messages of conversation `epoch`, in order.
    fn take_queued(&mut self, epoch: u64) -> Vec<String> {
        let (taken, kept) = std::mem::take(&mut self.dock.queued)
            .into_iter()
            .partition::<VecDeque<_>, _>(|(queued, _)| *queued == epoch);
        self.dock.queued = kept;
        taken.into_iter().map(|(_, text)| text).collect()
    }

    /// Send `message` to `session`, binding it first when `rebind` is given.
    fn send_to(
        &self,
        session: String,
        rebind: Option<ReviewAssistantBinding>,
        message: String,
        host: &UiContext,
    ) {
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        host.spawn(Box::pin(async move {
            let result = async {
                if let Some(binding) = rebind {
                    services
                        .call(BindReviewAssistantRequest {
                            repo_path: binding.workspace.repo_root.clone(),
                            binding,
                        })
                        .await
                        .map_err(|error| error.message)?;
                }
                services
                    .host()
                    .send_message(session, message)
                    .await
                    .map_err(|error| error.message)
            }
            .await;
            inbox.push(Message::Dock(Box::new(DockResult::Sent(result))));
        }));
    }

    /// The context block of the cursor's place: the review, and the selected range, line, or
    /// file. `None` outside a file.
    fn assistant_context(&self) -> Option<String> {
        let place = self.assistant_place()?;
        Some(format!(
            "{}\"{}\"; the reviewer is looking at {place}. Use the code_review tools to read and change this review.]",
            crate::agent::feed::CONTEXT_MARKER,
            self.title
        ))
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

    /// Keys while the dock is focused: the permission prompt's answers, the dock's own keys,
    /// and editing the message.
    pub(super) fn assistant_key(&mut self, stroke: KeyStroke, host: &UiContext) -> Action {
        // A pending permission prompt takes the feed keys (the input is hidden meanwhile).
        if self.assistant_prompt_pending()
            && let Some(action) = host.keymap_action(super::FEED_SCOPE, stroke)
        {
            let action = action.strip_prefix("bcode.code_review.").unwrap_or(&action);
            let answer = match action {
                "feed_allow" => Some(AgentAction::Allow),
                "feed_always" => Some(AgentAction::Always),
                "feed_decline" => Some(AgentAction::Decline),
                "feed_never" => Some(AgentAction::Never),
                _ => None,
            };
            if let Some(answer) = answer {
                self.assistant_action(answer);
                return Action::Redraw;
            }
        }
        if let Some(action) = host.keymap_action(ASSISTANT_SCOPE, stroke) {
            let action = action.strip_prefix("bcode.code_review.").unwrap_or(&action);
            match action {
                "assistant_close" => return self.toggle_assistant(host),
                "assistant_back" => {
                    self.focus = Focus::Review;
                    return Action::Redraw;
                }
                "assistant_stop" => {
                    self.assistant_action(AgentAction::Stop);
                    return Action::Redraw;
                }
                "assistant_context" => {
                    self.dock.context = !self.dock.context;
                    self.dock.touch();
                    return Action::Redraw;
                }
                "assistant_new" => {
                    self.dock.session = None;
                    self.dock.lookup = Lookup::Done;
                    self.dock.starting = false;
                    self.dock.epoch = self.dock.epoch.wrapping_add(1);
                    self.dock.scroll.set(0);
                    self.dock.touch();
                    self.set_status(
                        "the next message starts a new conversation",
                        StatusSeverity::Info,
                    );
                    return Action::Redraw;
                }
                "assistant_open" => {
                    let session = self.dock.session.clone();
                    return session
                        .and_then(|session| self.open_session(&session))
                        .unwrap_or(Action::Redraw);
                }
                "assistant_scroll_up" | "assistant_scroll_down" => {
                    let page = self.dock.shown.get().1.max(2) - 1;
                    self.scroll_assistant(if action == "assistant_scroll_up" {
                        i64::try_from(page).unwrap_or(1)
                    } else {
                        -i64::try_from(page).unwrap_or(1)
                    });
                    return Action::Redraw;
                }
                _ => {}
            }
        }
        if self.assistant_prompt_pending() {
            return Action::None;
        }
        let outcome = TextInputControl::new(&self.dock.policy)
            .handle_key(&mut self.dock.input.borrow_mut(), stroke);
        match outcome {
            TextInputOutcome::Submitted => self.send_input(host),
            TextInputOutcome::Ignored => Action::None,
            _ => {
                self.dock.touch();
                Action::Redraw
            }
        }
    }

    /// Paste into the dock's input.
    pub(super) fn assistant_paste(&mut self, text: &str) {
        if self.assistant_prompt_pending() {
            return;
        }
        TextInputControl::new(&self.dock.policy)
            .handle_paste(&mut self.dock.input.borrow_mut(), text);
        self.dock.touch();
    }

    /// Whether the dock's session waits on an unanswered permission prompt.
    pub(super) fn assistant_prompt_pending(&self) -> bool {
        self.dock
            .session
            .as_deref()
            .and_then(|session| self.agents.state(session))
            .and_then(|state| state.feed.permission.as_ref())
            .is_some_and(|permission| !self.agents.is_answering(&permission.permission_id))
    }

    /// Run a conversation action: answer the permission prompt, stop, open the session in
    /// Bcode, or open or close a feed item.
    pub(super) fn assistant_action(&mut self, action: AgentAction) -> Option<Action> {
        let session = self.dock.session.clone()?;
        match action {
            AgentAction::Open => return self.open_session(&session),
            AgentAction::Stop => {
                if self
                    .agents
                    .state(&session)
                    .is_some_and(|state| state.working && !state.cancelling)
                {
                    self.feed_requests
                        .push_back(super::feed::FeedRequest::Stop { session });
                    self.set_status("stopping the assistant…", StatusSeverity::Info);
                }
            }
            AgentAction::Toggle(index) => {
                let index = usize::from(index);
                let per_turn = crate::agent::feed::MAX_TURN_ITEMS;
                let id = self
                    .agents
                    .state(&session)
                    .and_then(|state| state.feed.turns.get(index / per_turn))
                    .and_then(|turn| turn.items.get(index % per_turn))
                    .and_then(crate::rows::item_id)
                    .map(|id| crate::rows::open_key(&crate::rows::dock_key(&session), id));
                if let Some(id) = id {
                    if !self.folds.feed_open.remove(&id) {
                        self.folds.feed_open.insert(id);
                    }
                    self.folds.revision = self.folds.revision.wrapping_add(1);
                }
            }
            AgentAction::Allow
            | AgentAction::Always
            | AgentAction::Decline
            | AgentAction::Never => {
                self.answer_permission(&session, action);
            }
            _ => {}
        }
        None
    }

    /// Scroll the conversation `rows` up (positive) or down.
    fn scroll_assistant(&self, rows: i64) {
        let (total, height) = self
            .dock
            .rows
            .borrow()
            .as_ref()
            .map_or((0, 0), |rows| (rows.lines.len(), self.dock.shown.get().1));
        let max = total.saturating_sub(height);
        let current = self.dock.scroll.get();
        let next = if rows >= 0 {
            current.saturating_add(usize::try_from(rows).unwrap_or(usize::MAX))
        } else {
            current.saturating_sub(usize::try_from(rows.unsigned_abs()).unwrap_or(usize::MAX))
        };
        self.dock.scroll.set(next.min(max));
    }

    /// A mouse event over the dock's conversation: wheel scrolls, a click runs the action under
    /// it. Returns whether it was handled.
    pub(super) fn assistant_mouse(&mut self, mouse: bmux_tui::event::MouseEvent) -> Option<Action> {
        use bmux_tui::event::{MouseButton, MouseEventKind};
        let area = self.body_area(ASSISTANT_FEED_ID)?;
        if !area.contains(mouse.position) {
            return None;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_assistant(i64::try_from(WHEEL_ROWS).unwrap_or(1));
                Some(Action::Redraw)
            }
            MouseEventKind::ScrollDown => {
                self.scroll_assistant(-i64::try_from(WHEEL_ROWS).unwrap_or(1));
                Some(Action::Redraw)
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.focus = Focus::Assistant;
                let Point { x, y } = mouse.position;
                let (first, count) = self.dock.shown.get();
                let row = usize::from(y.saturating_sub(area.y));
                let column = x.saturating_sub(area.x);
                if row >= count {
                    return Some(Action::Redraw);
                }
                let row = first + row;
                let action = self.dock.rows.borrow().as_ref().and_then(|rows| {
                    rows.actions
                        .iter()
                        .find(|cells| {
                            usize::from(cells.row) == row && cells.cells.contains(&column)
                        })
                        .map(|cells| cells.action)
                });
                if let Some(ItemAction::Agent(action)) = action {
                    return Some(self.assistant_action(action).unwrap_or(Action::Redraw));
                }
                Some(Action::Redraw)
            }
            _ => None,
        }
    }

    /// Lay out the conversation at `width` columns when it changed.
    pub(super) fn prepare_assistant(&self, width: u16) {
        let session = self.dock.session.clone();
        let key = (
            session
                .as_deref()
                .map_or(0, |session| self.agents.session_revision(session)),
            self.folds.revision,
            self.dock.revision,
            width,
        );
        if self
            .dock
            .rows
            .borrow()
            .as_ref()
            .is_some_and(|rows| rows.key == key)
        {
            return;
        }
        let theme = &self.theme;
        let width = usize::from(width);
        let (lines, actions) = session.as_deref().map_or_else(
            || (self.assistant_intro(width), Vec::new()),
            |session| crate::rows::dock_rows(session, &self.agents, &self.folds, theme, width),
        );
        *self.dock.rows.borrow_mut() = Some(Rows {
            key,
            lines,
            actions,
        });
    }

    /// What the dock shows before its first message.
    fn assistant_intro(&self, width: usize) -> Vec<Line> {
        let theme = &self.theme;
        let lines: &[(&str, bmux_tui::style::Style)] = if self.dock.starting {
            &[("starting the assistant…", theme.muted)]
        } else {
            &[
                ("Ask about this review.", theme.tiers.strong),
                (
                    "The assistant reads the review with code_review tools;",
                    theme.muted,
                ),
                ("changes it makes ask for permission first.", theme.muted),
            ]
        };
        lines
            .iter()
            .map(|(text, style)| {
                Line::from_spans(vec![Span::styled((*text).to_owned(), *style)]).truncate(width)
            })
            .collect()
    }

    /// Footer hints while the dock is focused.
    pub(super) fn assistant_hints(&self) -> Vec<(Option<String>, &'static str)> {
        let key = |action: &str| self.action_keys.get(action).cloned();
        if self.assistant_prompt_pending() {
            let keys = self.agents.keys();
            // Always and Never only when the request can be remembered.
            let remember = self
                .dock
                .session
                .as_deref()
                .and_then(|session| self.agents.state(session))
                .and_then(|state| state.feed.permission.as_ref())
                .is_some_and(|permission| permission.can_remember);
            return [
                (Some(keys.allow.clone()), "allow", true),
                (Some(keys.always.clone()), "always", remember),
                (Some(keys.decline.clone()), "decline", true),
                (Some(keys.never.clone()), "never", remember),
                (key("assistant_back"), "back", true),
            ]
            .into_iter()
            .filter(|(_, _, shown)| *shown)
            .map(|(key, label, _)| (key.filter(|key| !key.is_empty()), label))
            .collect();
        }
        vec![
            (Some("enter".to_owned()), "send"),
            (key("assistant_stop"), "stop"),
            (key("assistant_context"), "context"),
            (key("assistant_new"), "new"),
            (key("assistant_open"), "open in Bcode"),
            (key("assistant_back"), "back"),
            (key("assistant_close"), "close"),
        ]
    }

    /// The dock's header chips: the cursor's place and the review.
    pub(super) fn assistant_chips(&self) -> Line {
        let theme = &self.theme;
        // The place first: it is what a message carries, and the row clips at the dock's edge.
        let mut spans = match self.assistant_place() {
            Some(place) if self.dock.context => {
                vec![Span::styled(format!(" {place} "), theme.pill_accent)]
            }
            Some(_) => vec![Span::styled("context detached", theme.tiers.dim)],
            None => Vec::new(),
        };
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(format!(" {} ", self.title), theme.pill));
        Line::from_spans(spans)
    }

    /// The dock's conversation component.
    pub(super) const fn assistant_feed(&self) -> AssistantFeed<'_> {
        AssistantFeed { surface: self }
    }

    /// Follow the dock's session along with the cards' sessions.
    pub(super) fn assistant_sessions(&self) -> Option<String> {
        self.dock.session.clone()
    }

    /// Apply finished `code_review.*` tool calls of followed sessions: reload what the writes
    /// changed and follow navigation. Calls already finished when a session is first observed
    /// are only recorded, unless the app created the session itself (the dock's conversation or
    /// a question's own session); then every call answers the app's messages and is applied.
    pub(super) fn reflect_review_tools(&mut self, host: &UiContext) {
        let mut sessions: Vec<String> = self
            .notes
            .exchanges()
            .iter()
            .filter_map(|exchange| exchange.session_id.clone())
            .collect();
        sessions.extend(self.dock.session.clone());
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
            let first = !self.dock.applied.contains_key(session);
            let applied = self.dock.applied.entry(session.clone()).or_default();
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
        self.dock
            .applied
            .retain(|session, _| sessions.contains(session));
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
            inbox.push(Message::Dock(Box::new(DockResult::Viewed(result))));
        }));
    }

    /// Show viewed files as stored.
    fn apply_stored_viewed(&mut self, stored: &ReviewWorkspace) {
        let workspace = self.workspace_mut();
        if workspace.viewed_files == stored.viewed_files
            && workspace.viewed_marks == stored.viewed_marks
        {
            return;
        }
        workspace.viewed_files.clone_from(&stored.viewed_files);
        workspace.viewed_marks.clone_from(&stored.viewed_marks);
        let workspace = workspace.clone();
        if let Some(review) = self.review.as_mut() {
            review.doc.set_viewed_marks(&workspace);
            review.note_splices();
        }
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

/// The dock's conversation: the rows laid out by [`ReviewSurface::prepare_assistant`], scrolled
/// to the newest unless the reviewer scrolled up.
pub struct AssistantFeed<'a> {
    surface: &'a ReviewSurface,
}

impl AssistantFeed<'_> {
    /// The rows shown in a viewport of `height` rows: `(first, count)`.
    fn window(&self, height: usize) -> (usize, usize) {
        let total = self
            .surface
            .dock
            .rows
            .borrow()
            .as_ref()
            .map_or(0, |rows| rows.lines.len());
        let max = total.saturating_sub(height);
        let scroll = self.surface.dock.scroll.get().min(max);
        self.surface.dock.scroll.set(scroll);
        let first = max - scroll;
        (first, total.min(height))
    }
}

impl Component for AssistantFeed<'_> {
    fn revision(&self) -> ComponentRevision {
        let key = self.surface.dock.rows.borrow().as_ref().map_or(0, |rows| {
            rows.key.0 ^ rows.key.1.rotate_left(17) ^ rows.key.2.rotate_left(31)
        });
        ComponentRevision::new(key, key ^ self.surface.dock.scroll.get() as u64)
    }

    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            LayoutId::new(ASSISTANT_FEED_ID),
            constraints.constrain(LogicalSize::new(
                constraints.max_width(),
                constraints.max_height().unwrap_or(0),
            )),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let height = usize::try_from(layout.size.height).unwrap_or(usize::MAX);
        let (first, count) = self.window(height);
        self.surface.dock.shown.set((first, count));
        let lines: Vec<Line> = self
            .surface
            .dock
            .rows
            .borrow()
            .as_ref()
            .map(|rows| rows.lines[first..first + count].to_vec())
            .unwrap_or_default();
        // Rows were laid out at this width; clipping keeps them (and their click cells) in place.
        let block = TextBlock::new(Text::from_lines(lines)).wrap(bmux_tui::text::TextWrap::None);
        let size = Size::new(
            u16::try_from(layout.size.width).unwrap_or(u16::MAX),
            u16::try_from(layout.size.height).unwrap_or(u16::MAX),
        );
        let node = block.layout(Constraints::tight(size), &mut LayoutCx::new());
        block.paint(&node, cx);
    }
}
