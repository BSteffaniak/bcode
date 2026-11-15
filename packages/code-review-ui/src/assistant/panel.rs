//! The assistant panel: one conversation the app keeps open across home, the wizard and every
//! review.
//!
//! * **Placement.** The app splits its whole area between the screen and the panel
//!   ([`Assistant::split`]): docked on the right from [`RIGHT_DOCK_WIDTH`] columns (the screen
//!   keeps [`SCREEN_MIN_WIDTH`]), docked at the bottom from [`BOTTOM_DOCK_HEIGHT`] rows (the
//!   screen keeps [`SCREEN_MIN_HEIGHT`]), and otherwise zoomed over the whole area while it is
//!   focused (hidden but open while it is not). The divider on the panel's inner edge drags.
//! * **Conversation.** The first message of a new conversation creates a Bcode session with the
//!   review app's origin, tagged with the review and stack layer on screen (or none on home and
//!   in the wizard), and sends the message. The panel follows the active session like an AI card
//!   ([`crate::agent`]): turns, reasoning, tool lines, answers, the permission prompt and a status
//!   line while it works.
//! * **Attachment.** The session's binding names the review its `code_review.*` tools act on by
//!   default. It is rewritten before each message and whenever the place on screen changes: the
//!   review on screen, or detached on home and in the wizard, so review tools never default to a
//!   review the user has left.
//! * **History.** `alt+h` (or `⟲`) opens the picker; `+ new` starts an empty conversation;
//!   `alt+r` renames and `alt+d` deletes (after a confirmation), with Bcode's own rename and
//!   delete; `ctrl+o` (or `↗`) opens the conversation in `bcode tui`.
//! * **Reflection.** Finished `code_review.*` calls of the active session are handed to the app
//!   ([`Assistant::take_finished_tools`]), which lets the review on screen reload what they
//!   changed. Calls already finished when a session is first followed are not replayed, unless
//!   the panel created the session.

use super::button::Badge;
use super::chips::{Chip, ChipId, ChipPlan, ChipRow};
use super::history::Tag;
use super::models::{ModelState, Target};
use super::picker::{Picker, RowState};
use super::profile::Category;
use super::{ASSISTANT_SCOPE, HISTORY_SCOPE, Place, PlaceKey, Screen};
use crate::agent::{AgentPhase, AgentStreams, FeedKeys};
use crate::context::UiContext;
use crate::host::{ContextBlock, TurnRequest};
use crate::rows::{ActionCells, AgentAction, Folds, ItemAction};
use crate::services::ReviewServices;
use crate::theme::ReviewTheme;
use bcode_code_review_models::{
    BindReviewAssistantRequest, GetReviewAssistantRequest, ReviewAssistantBinding,
    ReviewAssistantLookup, ReviewAssistantTag, UnbindReviewAssistantRequest,
};
use bmux_keyboard::KeyStroke;
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutNode, LogicalSize,
};
use bmux_tui::composition::{Column, Flex, Row, TextBlock};
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::geometry::{Point, Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::{Line, Span, Text, TextWrap};
use bmux_tui_components::pane::{Pane, PaneComponent, PaneState, PaneStyles, PaneTitleAction};
use bmux_tui_components::text_input::{
    EdgePolicy, TextInputComponent, TextInputControl, TextInputOutcome, TextInputPolicy,
    TextInputState,
};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Hit and layout id of the panel.
pub const PANEL_ID: &str = "app.assistant";
/// Layout id of the conversation.
pub const FEED_ID: &str = "app.assistant.feed";
/// Layout id of the message input.
pub const INPUT_ID: &str = "app.assistant.input";
/// Layout id of the history rows.
pub const HISTORY_ID: &str = "app.assistant.history";
/// Width from which the panel docks on the right.
pub const RIGHT_DOCK_WIDTH: u16 = 100;
/// Height from which the panel docks at the bottom (below [`RIGHT_DOCK_WIDTH`]).
pub const BOTTOM_DOCK_HEIGHT: u16 = 16;
/// Columns the screen keeps beside a right dock.
pub const SCREEN_MIN_WIDTH: u16 = 60;
/// Rows the screen keeps above a bottom dock.
pub const SCREEN_MIN_HEIGHT: u16 = 8;
/// Narrowest right dock.
pub const MIN_WIDTH: u16 = 36;
/// Widest right dock.
pub const MAX_WIDTH: u16 = 100;
/// Lowest bottom dock.
pub const MIN_HEIGHT: u16 = 8;
/// Below this width or [`COMPACT_HEIGHT`] rows the panel shows its title row and input only.
pub const COMPACT_WIDTH: u16 = 24;
/// See [`COMPACT_WIDTH`].
pub const COMPACT_HEIGHT: u16 = 6;
/// Below this width the panel shows its title row only.
pub const TITLE_ONLY_WIDTH: u16 = 12;
/// Conversations whose turned-off categories are remembered at most.
const MAX_REMEMBERED_CATEGORIES: usize = 64;
/// Rows of the message input at most.
const INPUT_MAX_ROWS: u16 = 6;
/// Rows a wheel step scrolls the conversation.
const WHEEL_ROWS: usize = 3;
/// Cells of a model's name in a question's or card's `model · effort` label at most.
const MODEL_LABEL_CELLS: usize = 24;
/// Cells the title keeps when it is cut to leave room for the usage.
const MIN_TITLE_CELLS: u16 = 12;
/// Cells of a history row's `✕` (a space and the glyph).
const DELETE_CELLS: usize = 2;
/// Two presses on the title row within this time zoom the panel.
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(400);
/// Bytes of a message the panel sends at most.
const MAX_MESSAGE_BYTES: usize = 32 * 1024;
/// Characters of a new conversation's title.
const MAX_TITLE_CHARS: usize = 60;
/// Tools whose permission prompt never offers Always or Never in the panel.
pub const NEVER_REMEMBER: &[&str] = &[
    "code_review.publish",
    "code_review.delete_review",
    "bcode.code-review.publish",
    "bcode.code-review.pull_request_review",
];

/// Whether the panel is shown and has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shown {
    Closed,
    Open,
    Focused,
}

/// Whether the focused panel was zoomed over the whole area by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Zoom {
    /// Placed by the area's size.
    Docked,
    /// Over the whole area.
    Zoomed,
}

/// Where the panel is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dock {
    /// Beside the screen, on the right.
    Right,
    /// Under the screen.
    Bottom,
    /// Over the whole area.
    Zoomed,
}

/// The app's area split between the screen and the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Split {
    /// Where the screen renders; `None` while the zoomed panel covers it.
    pub screen: Option<Rect>,
    /// Where the panel renders and how it is docked, while shown.
    pub panel: Option<(Rect, Dock)>,
}

/// What an input did, for the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The panel did not use the input.
    Ignored,
    /// The panel changed.
    Redraw,
    /// Open a session in Bcode.
    OpenSession(bcode_session_models::SessionId),
    /// Draft a comment with this body at the review's cursor line.
    DraftComment(String),
}

/// Background work of the panel that finished.
#[derive(Debug)]
enum Done {
    /// The conversation's session exists (claimed for the app's tools); its first message is
    /// being sent.
    Created {
        epoch: u64,
        session: String,
    },
    Started {
        epoch: u64,
        result: Result<String, String>,
    },
    Sent {
        session: String,
        request: Box<TurnRequest>,
        retried: bool,
        result: Result<(), crate::host::HostError>,
    },
    Attached(Result<(), String>),
    History(Result<super::picker::HistoryLoad, String>),
    Turns(Vec<(String, crate::host::TurnCount)>),
    Renamed {
        session: String,
        title: String,
        result: Result<(), String>,
    },
    Deleted {
        session: String,
        result: Result<(), String>,
    },
    Answered {
        session: String,
        permission_id: String,
        error: Option<String>,
    },
    Stopped(Option<String>),
    /// The models Bcode offers were listed.
    Catalog(Result<crate::host::ModelCatalog, String>),
    /// The app default was read.
    Default(Result<bcode_code_review_models::ReviewAssistantSettings, String>),
    /// A session's model was read.
    SessionModel {
        session: String,
        result: Result<crate::host::SessionModel, String>,
    },
    /// A model or effort choice was applied: the notice to show, or why it failed.
    Chosen {
        session: Option<String>,
        default: Option<bcode_code_review_models::ReviewAssistantSettings>,
        result: Result<String, String>,
    },
    /// The app's sessions to delete, listed for the confirmation, and whether more remain
    /// unlisted.
    ListedAll(Result<(Vec<String>, bool), String>),
    /// Every listed session was deleted, except those Bcode refused; `more` when sessions
    /// remained unlisted.
    DeletedAll {
        deleted: Vec<String>,
        refused: Vec<(String, String)>,
        more: bool,
    },
}

/// What an open assistant overlay is for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OverlayPurpose {
    /// Choosing the model of a target; the options by row id.
    Models(Target, Vec<crate::host::ModelOption>),
    /// Turning categories on or off.
    Permissions,
    /// The tools list.
    Tools,
}

/// The conversation's rows at one width.
#[derive(Debug)]
struct Rows {
    key: (u64, u64, u64, u16, &'static str),
    lines: Vec<Line>,
    actions: Vec<ActionCells<ItemAction>>,
}

/// What the input row shows.
#[derive(Debug)]
enum Mode {
    /// The message input.
    Chat,
    /// Renaming the active conversation.
    Rename(Box<RefCell<TextInputState>>),
    /// Asking before deleting the active conversation.
    ConfirmDelete,
    /// Asking before deleting every conversation the review app started (these sessions).
    ConfirmDeleteAll(Vec<String>, bool),
}

/// What the last frame painted, for pointer input.
#[derive(Debug, Clone, Default)]
struct Painted {
    panel: Option<(Rect, Dock)>,
    area: Rect,
    feed: Option<Rect>,
    history: Option<Rect>,
    actions: Vec<(String, Rect)>,
    /// Each chip's cells.
    chips: Vec<(ChipId, Rect)>,
    /// The `✕` of a history row: its cells and session.
    history_delete: Option<(Rect, String)>,
    /// The title row.
    title: Option<Rect>,
}

/// The app-wide assistant panel.
pub struct Assistant {
    services: ReviewServices,
    repo: PathBuf,
    visibility: Shown,
    width: Option<u16>,
    height: Option<u16>,
    /// The divider is being dragged.
    drag: Option<Dock>,
    session: Option<String>,
    /// Title of the active conversation, once known.
    title: Option<String>,
    /// Where the active conversation started, once known.
    tag: Option<Tag>,
    starting: bool,
    /// Messages not sent yet, with the conversation (epoch) they were written in and the
    /// attachment to write before sending.
    queued: VecDeque<(u64, TurnRequest, Option<Attachment>)>,
    /// Where the conversation's first message was sent from, while its session waits to be
    /// created (until the app's tools are registered).
    start_place: Option<Place>,
    epoch: u64,
    /// The place the session was last attached for.
    attached: Option<PlaceKey>,
    input: RefCell<TextInputState>,
    policy: TextInputPolicy,
    /// Whether messages carry what is on screen.
    context: bool,
    agents: AgentStreams,
    folds: Folds,
    rows: RefCell<Option<Rows>>,
    scroll: Cell<usize>,
    shown: Cell<(usize, usize)>,
    /// Finished tool calls already applied, per followed session.
    applied: BTreeMap<String, BTreeSet<String>>,
    finished: Vec<(String, String)>,
    /// A message to send again with a fresh snapshot (its request-only context was lost).
    resend: Option<(String, TurnRequest)>,
    /// The connection providing the app's tools and agent profile, once asked for.
    tools: Option<crate::host::ClientToolLink>,
    /// Whether the connection was asked for (once per run).
    tools_asked: bool,
    picker: Option<Picker>,
    mode: Mode,
    notice: Option<(String, bool)>,
    inbox: Arc<Mutex<Vec<Done>>>,
    pane: Cell<PaneState>,
    painted: RefCell<Painted>,
    action_keys: BTreeMap<String, String>,
    feed_wake: Option<Instant>,
    revision: u64,
    /// The models Bcode offers, the app default and the sessions' models.
    models: ModelState,
    /// Categories turned off for the active conversation.
    off: BTreeSet<Category>,
    /// Categories turned off for the other conversations of this run, by session (bounded).
    off_elsewhere: BTreeMap<String, BTreeSet<Category>>,
    /// The open model, permissions or tools overlay.
    overlay: Option<(OverlayPurpose, crate::palette::Overlay)>,
    /// Why the last message was not sent or its conversation not started.
    failure: Option<String>,
    /// The chips row as last laid out.
    chip_plan: RefCell<ChipPlan>,
    /// The history row under the pointer.
    hover: Option<usize>,
    /// The `✕` painted on a history row: its row, column and session.
    history_delete: Cell<Option<(usize, u16)>>,
    /// The last press on the title row (for double-click zoom).
    title_press: Option<Instant>,
    /// Whether the focused panel covers the whole area.
    zoom: Zoom,
    /// Whether the active session's model was asked for.
    model_asked: Option<String>,
    /// The theme of the last frame (overlays are built with it).
    theme: Cell<ReviewTheme>,
}

impl std::fmt::Debug for Assistant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Assistant")
            .field("visibility", &self.visibility)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl Assistant {
    /// A closed panel for the repository at `repo`.
    #[must_use]
    pub fn new(services: ReviewServices, repo: PathBuf) -> Self {
        let mut policy = TextInputPolicy::chat_composer();
        policy.viewport.min_rows = 1;
        policy.viewport.max_rows = Some(INPUT_MAX_ROWS);
        policy.edge = EdgePolicy::disabled();
        Self {
            services,
            repo,
            visibility: Shown::Closed,
            width: None,
            height: None,
            drag: None,
            session: None,
            title: None,
            tag: None,
            starting: false,
            queued: VecDeque::new(),
            start_place: None,
            epoch: 0,
            attached: None,
            input: RefCell::new(empty_input()),
            policy,
            context: true,
            agents: AgentStreams::default(),
            folds: Folds::default(),
            rows: RefCell::new(None),
            scroll: Cell::new(0),
            shown: Cell::new((0, 0)),
            applied: BTreeMap::new(),
            finished: Vec::new(),
            resend: None,
            tools: None,
            tools_asked: false,
            picker: None,
            mode: Mode::Chat,
            notice: None,
            inbox: Arc::default(),
            pane: Cell::new(PaneState::new(Rect::default())),
            painted: RefCell::default(),
            action_keys: BTreeMap::new(),
            feed_wake: None,
            revision: 0,
            models: ModelState::default(),
            off: BTreeSet::new(),
            off_elsewhere: BTreeMap::new(),
            overlay: None,
            failure: None,
            chip_plan: RefCell::default(),
            hover: None,
            history_delete: Cell::new(None),
            title_press: None,
            zoom: Zoom::Docked,
            model_asked: None,
            theme: Cell::new(ReviewTheme::derive(
                None,
                crate::theme::IconStyle::default(),
            )),
        }
    }

    // ----------------------------------------------------------------------------------------
    // Queries
    // ----------------------------------------------------------------------------------------

    /// Whether the panel is open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        !matches!(self.visibility, Shown::Closed)
    }

    /// Whether the panel has keyboard focus.
    #[must_use]
    pub const fn is_focused(&self) -> bool {
        matches!(self.visibility, Shown::Focused)
    }

    /// The active conversation's session, once it has one.
    #[must_use]
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// The active conversation's title, once known.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Where the active conversation started, once known.
    #[must_use]
    pub const fn tag(&self) -> Option<&Tag> {
        self.tag.as_ref()
    }

    /// The history picker, while open.
    #[must_use]
    pub const fn picker(&self) -> Option<&Picker> {
        self.picker.as_ref()
    }

    /// Whether the context chip is attached.
    #[must_use]
    pub const fn context_attached(&self) -> bool {
        self.context
    }

    /// The notice shown under the chips, if any.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_ref().map(|(text, _)| text.as_str())
    }

    /// The message input.
    #[must_use]
    pub const fn input(&self) -> &RefCell<TextInputState> {
        &self.input
    }

    /// The models Bcode offers, the app default and the sessions' known models.
    #[must_use]
    pub const fn models(&self) -> &ModelState {
        &self.models
    }

    /// Categories turned off for the active conversation.
    #[must_use]
    pub const fn categories_off(&self) -> &BTreeSet<Category> {
        &self.off
    }

    /// Why the last message was not sent, shown in the conversation.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Labels of the open overlay's rows, when one is open.
    #[must_use]
    pub fn overlay_labels(&self) -> Option<Vec<&str>> {
        self.overlay
            .as_ref()
            .map(|(_, overlay)| overlay.shown_labels())
    }

    /// Whether the focused panel covers the whole area.
    #[must_use]
    pub const fn zoomed(&self) -> bool {
        matches!(self.zoom, Zoom::Zoomed)
    }

    /// The model and effort label a question or card shows: `model · effort`, for `session`
    /// (the app default without one).
    #[must_use]
    pub fn model_effort_label(&self, session: Option<&str>) -> String {
        let model = Line::from_spans(vec![Span::raw(self.models.model_label(session))])
            .truncate(MODEL_LABEL_CELLS);
        let model: String = model
            .spans
            .iter()
            .map(|span| span.content.as_str())
            .collect();
        match self.models.effort_label(session) {
            Some(effort) => format!("{model} · {effort}"),
            None => model,
        }
    }

    /// Ask for the models of `sessions` not known yet (Ask AI cards on screen), reading the
    /// app default and the models Bcode offers once.
    pub fn learn_models(&mut self, sessions: &[String], ui: &UiContext) {
        self.load_models(false, ui);
        for session in sessions.iter().take(8) {
            if self.models.session(session).is_none()
                && self.model_asked.as_deref() != Some(session)
            {
                self.read_session_model(session.clone(), ui);
            }
        }
    }

    /// The tool allowlist of the next turn: the profile's tools without the categories turned
    /// off; `None` while none is.
    #[must_use]
    pub fn tool_allowlist(&self) -> Option<Vec<String>> {
        if self.off.is_empty() {
            return None;
        }
        let mut config = (*self.services.assistant_config()).clone();
        for category in &self.off {
            config.categories.insert(*category, false);
        }
        Some(config.tools())
    }

    /// Whether the active session waits on an unanswered permission prompt.
    #[must_use]
    pub fn prompt_pending(&self) -> bool {
        self.pending_permission().is_some()
    }

    fn pending_permission(&self) -> Option<&crate::agent::feed::PendingPermission> {
        let session = self.session.as_deref()?;
        self.agents
            .state(session)?
            .feed
            .permission
            .as_ref()
            .filter(|permission| !self.agents.is_answering(&permission.permission_id))
    }

    /// Whether the panel was painted over `point` in the last frame.
    #[must_use]
    pub fn contains(&self, point: Point) -> bool {
        self.painted
            .borrow()
            .panel
            .is_some_and(|(rect, _)| rect.contains(point))
    }

    /// The badge the Ask AI button shows: only while the panel is not on screen.
    #[must_use]
    pub fn badge(&self, panel_shown: bool) -> Badge {
        if panel_shown {
            return Badge::None;
        }
        let Some(state) = self.session.as_deref().and_then(|s| self.agents.state(s)) else {
            return Badge::None;
        };
        if state.needs_you() {
            Badge::NeedsYou
        } else if state.working {
            let clock = self
                .session
                .as_deref()
                .map(|session| self.agents.clock(session))
                .unwrap_or_default();
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let index = usize::try_from(clock.step).unwrap_or(0) % frames.len();
            Badge::Working(frames[index].to_owned())
        } else {
            Badge::None
        }
    }

    const fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    // ----------------------------------------------------------------------------------------
    // Placement
    // ----------------------------------------------------------------------------------------

    /// Split `area` between the screen and the panel.
    #[must_use]
    pub fn split(&self, area: Rect) -> Split {
        if !self.is_open() || area.width == 0 || area.height == 0 {
            return Split {
                screen: Some(area),
                panel: None,
            };
        }
        if self.zoomed() && self.is_focused() {
            return Split {
                screen: None,
                panel: Some((area, Dock::Zoomed)),
            };
        }
        if area.width >= RIGHT_DOCK_WIDTH {
            let default = area.width * 2 / 5;
            let width = self
                .width
                .unwrap_or(default)
                .clamp(MIN_WIDTH, MAX_WIDTH)
                .min(area.width - SCREEN_MIN_WIDTH);
            let screen = Rect::new(area.x, area.y, area.width - width, area.height);
            let panel = Rect::new(screen.right(), area.y, width, area.height);
            return Split {
                screen: Some(screen),
                panel: Some((panel, Dock::Right)),
            };
        }
        if area.height >= BOTTOM_DOCK_HEIGHT {
            let default = area.height * 2 / 5;
            let height = self
                .height
                .unwrap_or(default)
                .max(MIN_HEIGHT)
                .min(area.height - SCREEN_MIN_HEIGHT);
            let screen = Rect::new(area.x, area.y, area.width, area.height - height);
            let panel = Rect::new(area.x, screen.bottom(), area.width, height);
            return Split {
                screen: Some(screen),
                panel: Some((panel, Dock::Bottom)),
            };
        }
        if self.is_focused() {
            Split {
                screen: None,
                panel: Some((area, Dock::Zoomed)),
            }
        } else {
            Split {
                screen: Some(area),
                panel: None,
            }
        }
    }

    // ----------------------------------------------------------------------------------------
    // Opening and focus
    // ----------------------------------------------------------------------------------------

    /// `ctrl+k`, the Ask AI button and the palette entry: open and focus the panel, focus it
    /// when it is open, and close it when it is focused.
    pub const fn toggle(&mut self) {
        self.visibility = match self.visibility {
            Shown::Closed | Shown::Open => Shown::Focused,
            Shown::Focused => Shown::Closed,
        };
        // A listing that failed (Bcode starting, a provider signed out) is tried again.
        self.models.relist = true;
        self.touch();
    }

    /// Open the panel and give it keyboard focus.
    pub const fn focus(&mut self) {
        self.visibility = Shown::Focused;
        self.models.relist = true;
        self.touch();
    }

    /// Show `text` on the notice line (as an error when `error`).
    pub fn set_notice(&mut self, text: String, error: bool) {
        self.notice = Some((text, error));
        self.touch();
    }

    /// Give keyboard focus back to the screen (the panel stays open).
    pub const fn unfocus(&mut self) {
        if matches!(self.visibility, Shown::Focused) {
            self.visibility = Shown::Open;
        }
        self.touch();
    }

    /// Read the keys of the panel's actions for hints and the feed's buttons.
    pub fn prepare_keymap(&mut self, ui: &UiContext) {
        let declared = crate::keymap::actions();
        let first = |scope: &str, action: &str| {
            let id = format!("bcode.code_review.{action}");
            let bound = ui.keymap_keys(scope, &id);
            declared
                .iter()
                .find(|declared| declared.id == id && declared.scope == scope)
                .and_then(|declared| {
                    declared
                        .defaults
                        .iter()
                        .find(|key| bound.contains(key))
                        .cloned()
                })
                .or_else(|| bound.into_iter().next())
                .unwrap_or_default()
        };
        let feed = crate::keymap::FEED_SCOPE;
        self.agents.set_keys(FeedKeys {
            allow: first(feed, "feed_allow"),
            always: first(feed, "feed_always"),
            decline: first(feed, "feed_decline"),
            never: first(feed, "feed_never"),
            stop: first(ASSISTANT_SCOPE, "assistant_stop"),
            open: first(ASSISTANT_SCOPE, "assistant_open"),
        });
        self.action_keys.clear();
        for scope in [ASSISTANT_SCOPE, HISTORY_SCOPE] {
            for action in crate::keymap::actions_for(scope) {
                let short = action
                    .id
                    .strip_prefix("bcode.code_review.")
                    .unwrap_or(&action.id)
                    .to_owned();
                let key = first(scope, &short);
                if !key.is_empty() {
                    self.action_keys.insert(short, key);
                }
            }
        }
    }

    fn key_of(&self, action: &str) -> Option<String> {
        self.action_keys.get(action).cloned()
    }

    // ----------------------------------------------------------------------------------------
    // Conversations
    // ----------------------------------------------------------------------------------------

    /// Start an empty conversation: its session is created by its first message, tagged with
    /// the place it is sent from.
    pub fn new_conversation(&mut self) {
        // The categories turned off stay off when the conversation is opened again.
        if let Some(session) = self.session.clone() {
            let off = std::mem::take(&mut self.off);
            if off.is_empty() {
                self.off_elsewhere.remove(&session);
            } else {
                self.off_elsewhere.insert(session, off);
                while self.off_elsewhere.len() > MAX_REMEMBERED_CATEGORIES {
                    self.off_elsewhere.pop_first();
                }
            }
        }
        // Only the active conversation is offered the app's tools.
        if let (Some(session), Some(_)) = (self.session.take(), &self.tools) {
            drop(self.services.host().release_tool_session(session));
        }
        self.start_place = None;
        self.title = None;
        self.tag = None;
        self.starting = false;
        self.attached = None;
        self.epoch = self.epoch.wrapping_add(1);
        self.scroll.set(0);
        self.mode = Mode::Chat;
        self.notice = None;
        self.failure = None;
        self.off.clear();
        self.touch();
    }

    /// Make `session` the active conversation (from the history).
    pub fn switch_to(&mut self, session: &str, title: Option<String>, tag: Option<Tag>) {
        if self.session.as_deref() == Some(session) {
            return;
        }
        self.new_conversation();
        self.session = Some(session.to_owned());
        self.off = self.off_elsewhere.remove(session).unwrap_or_default();
        self.title = title;
        self.tag = tag;
    }

    /// Whether the session's attachment must follow the place `key`.
    #[must_use]
    pub fn needs_attach(&self, key: &PlaceKey) -> bool {
        self.session.is_some() && !self.starting && self.attached.as_ref() != Some(key)
    }

    /// Attach the active session to the review on screen, or detach it where no review is.
    pub fn attach(&mut self, place: &Place, ui: &UiContext) {
        let Some(session) = self.session.clone() else {
            return;
        };
        self.attached = Some(place.key());
        let binding = self.binding_for(place, &session);
        let services = self.services.clone();
        let repo = self.repo.clone();
        let inbox = Arc::clone(&self.inbox);
        ui.spawn(Box::pin(async move {
            let result = attach(&services, &repo, &session, Attachment::of(binding)).await;
            push(&inbox, Done::Attached(result));
        }));
    }

    /// The binding of `session` for the review on screen; `None` where no review is.
    fn binding_for(&self, place: &Place, session: &str) -> Option<ReviewAssistantBinding> {
        let review = place.review.as_ref()?;
        let mut binding = *review.binding.clone()?;
        session.clone_into(&mut binding.session_id);
        binding.detached_at_ms = None;
        // Where it started, for a session the store has not seen yet.
        binding.started = Some(match &self.tag {
            Some(Tag::Review {
                workspace_id,
                title,
                layer,
            }) => ReviewAssistantTag {
                workspace_id: Some(workspace_id.clone()),
                title: Some(title.clone()),
                layer: *layer,
            },
            Some(Tag::DeletedReview {
                workspace_id,
                layer,
            }) => ReviewAssistantTag {
                workspace_id: Some(workspace_id.clone()),
                title: None,
                layer: *layer,
            },
            Some(Tag::Home) => ReviewAssistantTag::none(),
            None => place.tag(),
        });
        Some(binding)
    }

    /// The context blocks of a message sent from `place`: the snapshot taken when it was sent,
    /// shrunk to the screen and review when the user detached the context.
    fn context_blocks(&self, place: &Place) -> Vec<ContextBlock> {
        let mut context =
            place
                .context
                .as_deref()
                .cloned()
                .unwrap_or_else(|| super::context::ViewContext {
                    screen: place.screen.name().to_owned(),
                    repo: self.repo.display().to_string(),
                    ..super::context::ViewContext::default()
                });
        context.detached = !self.context;
        context.blocks()
    }

    /// Send the input's text from `place`.
    ///
    /// Every message names the assistant's profile, so it is sent only while the app's tools
    /// and profile are registered with Bcode; until then it waits (fail closed: a turn never
    /// runs without the profile's tool limits).
    fn send_input(&mut self, place: &Place, ui: &UiContext) -> Outcome {
        let text = self.input.borrow().buffer().text().to_string();
        let text = text.trim_end().to_owned();
        if text.trim().is_empty() {
            return Outcome::Ignored;
        }
        if text.len() > MAX_MESSAGE_BYTES {
            self.notice = Some(("the message is too long for the assistant".to_owned(), true));
            return Outcome::Redraw;
        }
        self.serve_tools();
        if self.tools.is_none() {
            self.notice = Some((
                "the assistant needs the review app's tools, which this host cannot provide"
                    .to_owned(),
                true,
            ));
            return Outcome::Redraw;
        }
        let request = TurnRequest {
            text,
            context: self.context_blocks(place),
            agent: Some(super::profile::PROFILE_ID.to_owned()),
            tool_allowlist: self.tool_allowlist(),
        };
        *self.input.borrow_mut() = empty_input();
        self.scroll.set(0);
        self.notice = None;
        self.failure = None;
        self.touch();
        let attachment = match self.session.clone() {
            // Rebinding first keeps the session's default review the one on screen.
            Some(session) if !self.starting => (self.attached.as_ref() != Some(&place.key()))
                .then(|| Attachment::of(self.binding_for(place, &session))),
            _ => None,
        };
        if self.session.is_some() && !self.starting {
            self.attached = Some(place.key());
        } else if self.session.is_none() && !self.starting && self.start_place.is_none() {
            self.start_place = Some(Place {
                context: None,
                ..place.clone()
            });
        }
        self.queued.push_back((self.epoch, request, attachment));
        self.flush(ui);
        Outcome::Redraw
    }

    /// Send the active conversation's waiting messages once the app's tools are registered:
    /// the first creates the session when it has none.
    fn flush(&mut self, ui: &UiContext) {
        if self.starting || !self.queued.iter().any(|(epoch, _, _)| *epoch == self.epoch) {
            return;
        }
        if !self.tools_ready() {
            self.notice = Some((self.waiting_notice(), false));
            return;
        }
        if matches!(&self.notice, Some((_, false))) {
            self.notice = None;
        }
        if let Some(session) = self.session.clone() {
            for (request, attachment) in self.take_queued(self.epoch) {
                self.submit(session.clone(), attachment, request, ui);
            }
            return;
        }
        let Some(place) = self.start_place.take() else {
            return;
        };
        let index = self
            .queued
            .iter()
            .position(|(epoch, _, _)| *epoch == self.epoch);
        if let Some((_, request, _)) = index.and_then(|index| self.queued.remove(index)) {
            self.start(&place, request, ui);
        }
    }

    /// Why waiting messages are not sent yet.
    fn waiting_notice(&self) -> String {
        let state = self
            .tools
            .as_ref()
            .and_then(|link| link.state.lock().ok().map(|state| state.clone()));
        match state {
            Some(crate::host::ToolLinkState::Failed(error)) => format!(
                "waiting for the review app's tools to register with Bcode ({error}); retrying"
            ),
            _ => "waiting for the review app's tools to register with Bcode…".to_owned(),
        }
    }

    /// Create the conversation's session from `place` on the tool connection (so its first
    /// turn already has the app's tools), attach it, and send `request`.
    fn start(&mut self, place: &Place, request: TurnRequest, ui: &UiContext) {
        self.starting = true;
        self.tag = Some(place.history_tag());
        let title = conversation_title(&request.text);
        self.title = Some(title.clone());
        let epoch = self.epoch;
        let origin = crate::host::review_session_origin(
            crate::host::ReviewSessionPurpose::Assistant,
            place
                .review
                .as_ref()
                .map(|review| review.workspace_id.as_str()),
            place.review.as_ref().and_then(|review| review.layer),
        );
        let template = self.binding_for(place, "");
        self.attached = Some(place.key());
        let services = self.services.clone();
        let repo = self.repo.clone();
        let inbox = Arc::clone(&self.inbox);
        ui.spawn(Box::pin(async move {
            let result = async {
                let session = services
                    .host()
                    .create_tool_session(title, repo.clone(), origin)
                    .await
                    .map_err(|error| error.message)?;
                push(
                    &inbox,
                    Done::Created {
                        epoch,
                        session: session.clone(),
                    },
                );
                // The app default applies before the first message; a refused model leaves
                // Bcode's default and says why.
                if let Err(error) =
                    super::models::apply_default(&services, repo.clone(), &session).await
                {
                    push(
                        &inbox,
                        Done::Chosen {
                            session: Some(session.clone()),
                            default: None,
                            result: Err(error),
                        },
                    );
                }
                if let Some(mut binding) = template {
                    binding.session_id.clone_from(&session);
                    services
                        .call(BindReviewAssistantRequest {
                            repo_path: repo.clone(),
                            binding,
                        })
                        .await
                        .map_err(|error| error.message)?;
                }
                services
                    .host()
                    .submit_turn(session.clone(), request)
                    .await
                    .map_err(|error| error.message)?;
                Ok::<_, String>(session)
            }
            .await;
            push(&inbox, Done::Started { epoch, result });
        }));
    }

    /// Send `request` to `session`, attaching it first when `attachment` is given.
    fn submit(
        &self,
        session: String,
        attachment: Option<Attachment>,
        request: TurnRequest,
        ui: &UiContext,
    ) {
        self.submit_once(session, attachment, request, false, ui);
    }

    fn submit_once(
        &self,
        session: String,
        attachment: Option<Attachment>,
        request: TurnRequest,
        retried: bool,
        ui: &UiContext,
    ) {
        let services = self.services.clone();
        let repo = self.repo.clone();
        let inbox = Arc::clone(&self.inbox);
        ui.spawn(Box::pin(async move {
            let result = async {
                if let Some(attachment) = attachment {
                    attach(&services, &repo, &session, attachment)
                        .await
                        .map_err(crate::host::HostError::failed)?;
                }
                // Claimed before the turn starts, which fixes the tools it is offered (a
                // conversation from the history, or after Bcode restarted).
                services.host().claim_tool_session(session.clone()).await?;
                services
                    .host()
                    .submit_turn(session.clone(), request.clone())
                    .await
            }
            .await;
            push(
                &inbox,
                Done::Sent {
                    session,
                    request: Box::new(request),
                    retried,
                    result,
                },
            );
        }));
    }

    /// A message whose request-only context the daemon no longer held, waiting to be sent again
    /// with a fresh snapshot.
    #[must_use]
    pub const fn wants_resend(&self) -> bool {
        self.resend.is_some()
    }

    /// Send the waiting message again, once, with the context of `place`.
    pub fn resend(&mut self, place: &Place, ui: &UiContext) {
        let Some((session, mut request)) = self.resend.take() else {
            return;
        };
        request.context = self.context_blocks(place);
        self.submit_once(session, None, request, true, ui);
    }

    // ----------------------------------------------------------------------------------------
    // Polling
    // ----------------------------------------------------------------------------------------

    /// Whether the app's tools and the assistant's profile are registered with Bcode.
    #[must_use]
    pub fn tools_ready(&self) -> bool {
        self.tools.as_ref().is_some_and(|link| {
            link.state
                .lock()
                .is_ok_and(|state| *state == crate::host::ToolLinkState::Ready)
        })
    }

    /// Register the app's tools and the assistant's profile once. Sessions are claimed when a
    /// message is sent to them.
    fn serve_tools(&mut self) {
        if !self.tools_asked {
            self.tools_asked = true;
            let config = self.services.assistant_config();
            // On the host that creates the sessions and submits their turns.
            self.tools = self
                .services
                .host()
                .serve_client_tools(super::profile::client_tools(), vec![config.profile()])
                .ok();
        }
    }

    /// Calls of the app's tools waiting for an answer, oldest first.
    pub fn take_tool_calls(&mut self) -> Vec<crate::host::ClientToolCall> {
        let mut calls = Vec::new();
        if let Some(link) = self.tools.as_mut() {
            while let Ok(call) = link.calls.try_recv() {
                calls.push(call);
            }
        }
        calls
    }

    /// Collect finished work, follow the active session and advance its spinner. Returns
    /// whether anything changed.
    pub fn poll(&mut self, ui: &UiContext) -> bool {
        self.serve_tools();
        if self.is_open() {
            let relist = std::mem::take(&mut self.models.relist);
            self.load_models(relist, ui);
        }
        if let Some(session) = self.session.clone()
            && !self.starting
            && self.models.session(&session).is_none()
            && self.model_asked.as_deref() != Some(session.as_str())
        {
            self.read_session_model(session, ui);
        }
        let done: Vec<Done> = self
            .inbox
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default();
        let mut changed = !done.is_empty();
        for done in done {
            self.finish(done, ui);
        }
        if !self.queued.is_empty() {
            let notice = self.notice.clone();
            self.flush(ui);
            changed |= self.notice != notice;
        }
        changed |= self
            .agents
            .ensure(ui, self.session.iter().map(String::as_str));
        changed |= self.agents.drain();
        drop(self.agents.take_attention());
        self.collect_finished_tools();
        let before = self.agents.revision();
        let now = Instant::now();
        if let Some(delay) = self
            .agents
            .tick(now, bmux_tui_components::spinner::DEFAULT_INTERVAL)
        {
            let due = now + delay;
            if self.feed_wake.is_none_or(|wake| wake <= now || due < wake) {
                self.feed_wake = Some(due);
                ui.host().request_redraw_after(delay);
            }
        }
        changed | (self.agents.revision() != before)
    }

    fn finish(&mut self, done: Done, ui: &UiContext) {
        self.touch();
        match done {
            Done::Created { epoch, session } => {
                if epoch == self.epoch {
                    // Every review tool call of a session the panel created answers its
                    // messages, so none is skipped as already applied.
                    self.applied.entry(session.clone()).or_default();
                    self.session = Some(session);
                } else {
                    // A new conversation started meanwhile: only the active conversation is
                    // offered the app's tools, and the session was claimed when created.
                    self.release_inactive(session);
                }
            }
            Done::Started {
                epoch,
                result: Ok(session),
            } => {
                if epoch == self.epoch {
                    self.starting = false;
                    self.read_session_model(session.clone(), ui);
                    self.session = Some(session);
                    self.flush(ui);
                } else {
                    // A new conversation started meanwhile: the earlier one keeps its session
                    // and gets the messages written for it.
                    for (request, attachment) in self.take_queued(epoch) {
                        self.submit(session.clone(), attachment, request, ui);
                    }
                }
            }
            Done::Started {
                epoch,
                result: Err(error),
            } => {
                if epoch == self.epoch {
                    self.starting = false;
                    self.attached = None;
                    self.session = None;
                }
                drop(self.take_queued(epoch));
                self.failure = Some(failure_text("could not start the assistant", None, &error));
            }
            Done::Sent {
                session,
                request,
                retried,
                result: Err(error),
            } => {
                if !retried && error.code == crate::host::REQUEST_ONLY_CONTEXT_UNAVAILABLE {
                    self.resend = Some((session, *request));
                } else {
                    self.failure = Some(failure_text(
                        "could not send",
                        Some(&error.code),
                        &error.message,
                    ));
                    self.release_inactive(session);
                }
            }
            Done::Sent {
                session,
                result: Ok(()),
                ..
            } => self.release_inactive(session),
            Done::Attached(Err(error)) => {
                self.attached = None;
                self.notice = Some((format!("could not attach the review: {error}"), true));
            }
            Done::Attached(Ok(())) => {}
            other @ (Done::History(_)
            | Done::Turns(_)
            | Done::Renamed { .. }
            | Done::Deleted { .. }) => self.finish_history(other, ui),
            Done::Answered {
                session,
                permission_id,
                error,
            } => {
                if let Some(error) = error {
                    self.agents.set_answering(&session, &permission_id, false);
                    self.notice = Some((format!("could not answer: {error}"), true));
                }
            }
            Done::Stopped(error) => {
                if let Some(error) = error {
                    self.notice = Some((format!("could not stop: {error}"), true));
                }
            }
            other => self.finish_models(other, ui),
        }
    }

    /// Release `session` from the app's tools unless it is the active conversation (sending to
    /// a session claims it, and only the active conversation is offered the tools).
    fn release_inactive(&self, session: String) {
        if self.tools.is_some() && self.session.as_deref() != Some(session.as_str()) {
            drop(self.services.host().release_tool_session(session));
        }
    }

    /// Finished history work: a load, turn counts, a rename or a delete.
    fn finish_history(&mut self, done: Done, ui: &UiContext) {
        match done {
            Done::History(result) => {
                let counts: Vec<String> = result
                    .as_ref()
                    .map(|load| {
                        load.rows
                            .iter()
                            .take(super::picker::TURN_ROWS)
                            .map(|row| row.session_id.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(picker) = self.picker.as_mut() {
                    picker.load = Some(result);
                    picker.clamp();
                    if let Some(active) = &self.session
                        && let Some(index) = picker
                            .matching()
                            .iter()
                            .position(|row| &row.session_id == active)
                    {
                        picker.selected = index;
                    }
                }
                self.count_turns(counts, ui);
            }
            Done::Turns(turns) => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.turns.extend(turns);
                }
            }
            Done::Renamed {
                session,
                title,
                result,
            } => match result {
                Ok(()) => {
                    if self.session.as_deref() == Some(session.as_str()) {
                        self.title = Some(title.clone());
                    }
                    if let Some(picker) = self.picker.as_mut() {
                        picker.rename(&session, &title);
                    }
                }
                Err(error) => self.notice = Some((format!("could not rename: {error}"), true)),
            },
            Done::Deleted { session, result } => match result {
                Ok(()) => {
                    if let Some(picker) = self.picker.as_mut() {
                        picker.remove(&session);
                    }
                    if self.session.as_deref() == Some(session.as_str()) {
                        self.new_conversation();
                    }
                    self.notice = Some(("conversation deleted".to_owned(), false));
                }
                Err(error) => self.notice = Some((format!("could not delete: {error}"), true)),
            },
            _ => {}
        }
    }

    fn take_queued(&mut self, epoch: u64) -> Vec<(TurnRequest, Option<Attachment>)> {
        let (taken, kept) = std::mem::take(&mut self.queued)
            .into_iter()
            .partition::<VecDeque<_>, _>(|(queued, _, _)| *queued == epoch);
        self.queued = kept;
        taken
            .into_iter()
            .map(|(_, request, attachment)| (request, attachment))
            .collect()
    }

    /// Note finished `code_review.*` calls of the active session not applied yet.
    fn collect_finished_tools(&mut self) {
        let Some(session) = self.session.clone() else {
            self.applied.clear();
            return;
        };
        let Some(state) = self.agents.state(&session) else {
            return;
        };
        if state.phase == AgentPhase::Waiting {
            return;
        }
        let finished: Vec<(String, String, String)> = state
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
                    Some((tool.id.clone(), tool.name.clone(), tool.arguments.clone()))
                }
                _ => None,
            })
            .collect();
        let first = !self.applied.contains_key(&session);
        let applied = self.applied.entry(session.clone()).or_default();
        let mut kept = BTreeSet::new();
        for (id, name, arguments) in finished {
            kept.insert(id.clone());
            if first || applied.contains(&id) {
                continue;
            }
            self.finished.push((name, arguments));
        }
        // Calls that left the observed window never come back.
        *applied = kept;
        self.applied.retain(|known, _| *known == session);
    }

    /// Finished `code_review.*` calls (name, JSON arguments) not handed out yet, oldest first.
    pub fn take_finished_tools(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.finished)
    }

    // ----------------------------------------------------------------------------------------
    // History
    // ----------------------------------------------------------------------------------------

    /// Open the history picker and load it.
    pub fn open_history(&mut self, ui: &UiContext) {
        self.picker = Some(Picker::default());
        self.mode = Mode::Chat;
        self.touch();
        let services = self.services.clone();
        let repo = self.repo.clone();
        let inbox = Arc::clone(&self.inbox);
        ui.spawn(Box::pin(async move {
            let result = super::picker::load(services, repo).await;
            push(&inbox, Done::History(result));
        }));
    }

    fn count_turns(&self, sessions: Vec<String>, ui: &UiContext) {
        if sessions.is_empty() {
            return;
        }
        let host = Arc::clone(ui.host());
        let inbox = Arc::clone(&self.inbox);
        ui.spawn(Box::pin(async move {
            let mut turns = Vec::new();
            for session in sessions {
                if let Ok(count) = host.count_turns(session.clone()).await {
                    turns.push((session, count));
                }
            }
            push(&inbox, Done::Turns(turns));
        }));
    }

    fn rename(&self, session: String, title: String, ui: &UiContext) {
        let host = Arc::clone(ui.host());
        let inbox = Arc::clone(&self.inbox);
        ui.spawn(Box::pin(async move {
            let result = host
                .rename_session(session.clone(), title.clone())
                .await
                .map_err(|error| error.message);
            push(
                &inbox,
                Done::Renamed {
                    session,
                    title,
                    result,
                },
            );
        }));
    }

    /// Delete `session` from Bcode (stopping its turn first) and forget its binding.
    fn delete(&self, session: String, ui: &UiContext) {
        let working = self
            .agents
            .state(&session)
            .is_some_and(|state| state.working);
        let services = self.services.clone();
        let repo = self.repo.clone();
        let inbox = Arc::clone(&self.inbox);
        ui.spawn(Box::pin(async move {
            let host = services.host();
            if working {
                let _ = host.cancel_turn(session.clone()).await;
            }
            let result = async {
                host.delete_session(session.clone())
                    .await
                    .map_err(|error| error.message)?;
                services
                    .call(UnbindReviewAssistantRequest {
                        repo_path: repo,
                        session_id: session.clone(),
                    })
                    .await
                    .map_err(|error| error.message)?;
                Ok(())
            }
            .await;
            push(&inbox, Done::Deleted { session, result });
        }));
    }

    // ----------------------------------------------------------------------------------------
    // Input
    // ----------------------------------------------------------------------------------------

    /// A key while the panel is focused.
    pub fn key(&mut self, stroke: KeyStroke, place: &Place, ui: &UiContext) -> Outcome {
        self.touch();
        if self.picker.is_some() {
            return self.history_key(stroke, place, ui);
        }
        match &self.mode {
            Mode::Rename(_) => return self.rename_key(stroke, ui),
            Mode::ConfirmDelete | Mode::ConfirmDeleteAll(..) => {
                return self.confirm_key(stroke, ui);
            }
            Mode::Chat => {}
        }
        // A pending permission prompt takes the feed keys (the input is hidden meanwhile).
        if self.prompt_pending()
            && let Some(action) = ui.keymap_action(crate::keymap::FEED_SCOPE, stroke)
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
                self.answer(answer, ui);
                return Outcome::Redraw;
            }
        }
        if let Some(action) = ui.keymap_action(ASSISTANT_SCOPE, stroke) {
            let action = action.strip_prefix("bcode.code_review.").unwrap_or(&action);
            let outcome = self.run_action(action, place, ui);
            if outcome != Outcome::Ignored {
                return outcome;
            }
        }
        if self.prompt_pending() {
            return Outcome::Ignored;
        }
        let outcome =
            TextInputControl::new(&self.policy).handle_key(&mut self.input.borrow_mut(), stroke);
        match outcome {
            TextInputOutcome::Submitted => self.send_input(place, ui),
            TextInputOutcome::Ignored => Outcome::Ignored,
            _ => Outcome::Redraw,
        }
    }

    fn start_rename(&mut self) {
        if self.session.is_none() {
            self.notice = Some(("send a message first; then rename it".to_owned(), false));
            return;
        }
        let input = TextInputState::new(bmux_text_edit::TextEditBuffer::from_text(
            self.title.clone().unwrap_or_default(),
        ));
        self.mode = Mode::Rename(Box::new(RefCell::new(input)));
    }

    fn rename_key(&mut self, stroke: KeyStroke, ui: &UiContext) -> Outcome {
        let Mode::Rename(input) = &self.mode else {
            return Outcome::Ignored;
        };
        if stroke.key == bmux_keyboard::KeyCode::Escape {
            self.mode = Mode::Chat;
            return Outcome::Redraw;
        }
        let outcome =
            TextInputControl::new(&RENAME_POLICY).handle_key(&mut input.borrow_mut(), stroke);
        if outcome == TextInputOutcome::Submitted || stroke.key == bmux_keyboard::KeyCode::Enter {
            let title = input.borrow().buffer().text().trim().to_owned();
            self.mode = Mode::Chat;
            if let Some(session) = self.session.clone()
                && !title.is_empty()
            {
                self.rename(session, title, ui);
            }
        }
        Outcome::Redraw
    }

    fn confirm_key(&mut self, stroke: KeyStroke, ui: &UiContext) -> Outcome {
        use bmux_keyboard::KeyCode;
        match stroke.key {
            KeyCode::Char('y') | KeyCode::Enter => {
                match std::mem::replace(&mut self.mode, Mode::Chat) {
                    Mode::ConfirmDeleteAll(sessions, more) => self.delete_all(sessions, more, ui),
                    _ => {
                        if let Some(session) = self.session.clone() {
                            self.delete(session, ui);
                        }
                    }
                }
            }
            KeyCode::Char('n') | KeyCode::Escape => self.mode = Mode::Chat,
            _ => return Outcome::Ignored,
        }
        Outcome::Redraw
    }

    fn history_key(&mut self, stroke: KeyStroke, place: &Place, ui: &UiContext) -> Outcome {
        let Some(picker) = self.picker.as_mut() else {
            return Outcome::Ignored;
        };
        if let Some(action) = ui.keymap_action(HISTORY_SCOPE, stroke) {
            let action = action.strip_prefix("bcode.code_review.").unwrap_or(&action);
            match action {
                "history_up" => picker.move_by(-1),
                "history_down" => picker.move_by(1),
                "history_back" => self.picker = None,
                "history_switch" => return self.switch_selected(place, ui),
                "history_open" => {
                    return picker
                        .selected_row()
                        .and_then(|row| row.session_id.parse().ok())
                        .map_or(Outcome::Redraw, Outcome::OpenSession);
                }
                "history_rename" => {
                    if let Some(row) = picker.selected_row().cloned() {
                        self.picker = None;
                        self.switch_to(&row.session_id, Some(row.title), Some(row.tag));
                        self.start_rename();
                    }
                }
                "history_delete" => {
                    let Some(row) = picker.selected_row().cloned() else {
                        return Outcome::Redraw;
                    };
                    if picker.marked.as_deref() == Some(row.session_id.as_str()) {
                        picker.marked = None;
                        self.delete(row.session_id, ui);
                    } else {
                        picker.marked = Some(row.session_id);
                    }
                }
                _ => return Outcome::Ignored,
            }
            return Outcome::Redraw;
        }
        let before = picker.query();
        let outcome = TextInputControl::new(picker.policy())
            .handle_key(&mut picker.query_input().borrow_mut(), stroke);
        if picker.query() != before {
            picker.selected = 0;
            picker.marked = None;
        }
        match outcome {
            TextInputOutcome::Ignored => Outcome::Ignored,
            _ => Outcome::Redraw,
        }
    }

    /// Switch to the picker's selected conversation and attach it to the place on screen.
    fn switch_selected(&mut self, place: &Place, ui: &UiContext) -> Outcome {
        let Some(row) = self
            .picker
            .as_ref()
            .and_then(|picker| picker.selected_row().cloned())
        else {
            return Outcome::Redraw;
        };
        self.picker = None;
        self.switch_to(&row.session_id, Some(row.title), Some(row.tag));
        self.attach(place, ui);
        Outcome::Redraw
    }

    fn open_in_bcode(&mut self) -> Outcome {
        if let Some(session) = self
            .session
            .as_deref()
            .and_then(|session| session.parse().ok())
        {
            return Outcome::OpenSession(session);
        }
        self.notice = Some(("start or open a conversation first".to_owned(), false));
        Outcome::Redraw
    }

    /// Paste into the focused input.
    pub fn paste(&mut self, text: &str) {
        self.touch();
        if let Some(picker) = self.picker.as_mut() {
            TextInputControl::new(picker.policy())
                .handle_paste(&mut picker.query_input().borrow_mut(), text);
            picker.selected = 0;
            return;
        }
        match &self.mode {
            Mode::Rename(input) => {
                TextInputControl::new(&RENAME_POLICY).handle_paste(&mut input.borrow_mut(), text);
            }
            Mode::ConfirmDelete | Mode::ConfirmDeleteAll(..) => {}
            Mode::Chat if self.prompt_pending() => {}
            Mode::Chat => {
                TextInputControl::new(&self.policy)
                    .handle_paste(&mut self.input.borrow_mut(), text);
            }
        }
    }

    fn stop(&mut self, ui: &UiContext) {
        let Some(session) = self.session.clone() else {
            return;
        };
        if !self
            .agents
            .state(&session)
            .is_some_and(|state| state.working && !state.cancelling)
        {
            return;
        }
        self.notice = Some(("stopping the assistant…".to_owned(), false));
        let host = Arc::clone(ui.host());
        let inbox = Arc::clone(&self.inbox);
        ui.spawn(Box::pin(async move {
            let error = host
                .cancel_turn(session)
                .await
                .err()
                .map(|error| error.message);
            push(&inbox, Done::Stopped(error));
        }));
    }

    /// Answer the active session's permission prompt. Always and Never are not offered for
    /// publishing or deleting.
    fn answer(&mut self, action: AgentAction, ui: &UiContext) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let Some(permission) = self.pending_permission().cloned() else {
            return;
        };
        let remembered =
            permission.can_remember && !NEVER_REMEMBER.contains(&permission.tool.as_str());
        let (approved, remember) = match action {
            AgentAction::Allow => (true, false),
            AgentAction::Decline => (false, false),
            AgentAction::Always if remembered => (true, true),
            AgentAction::Never if remembered => (false, true),
            _ => return,
        };
        self.agents
            .set_answering(&session, &permission.permission_id, true);
        let host = Arc::clone(ui.host());
        let inbox = Arc::clone(&self.inbox);
        let permission_id = permission.permission_id;
        ui.spawn(Box::pin(async move {
            let error = host
                .resolve_permission(permission_id.clone(), approved, remember)
                .await
                .err()
                .map(|error| error.message);
            push(
                &inbox,
                Done::Answered {
                    session,
                    permission_id,
                    error,
                },
            );
        }));
    }

    fn scroll_by(&self, rows: i64) {
        let total = self
            .rows
            .borrow()
            .as_ref()
            .map_or(0, |rows| rows.lines.len());
        let max = total.saturating_sub(self.shown.get().1);
        let current = self.scroll.get();
        let next = if rows >= 0 {
            current.saturating_add(usize::try_from(rows).unwrap_or(usize::MAX))
        } else {
            current.saturating_sub(usize::try_from(rows.unsigned_abs()).unwrap_or(usize::MAX))
        };
        self.scroll.set(next.min(max));
    }

    /// A pointer event. `None` when it is outside the panel (and no divider drag runs).
    pub fn mouse(&mut self, event: &Event, place: &Place, ui: &UiContext) -> Option<Outcome> {
        let Event::Mouse(mouse) = event else {
            return None;
        };
        let painted = self.painted.borrow().clone();
        let (panel, dock) = painted.panel?;
        if let Some(dragged) = self.drag {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    let area = painted.area;
                    match dragged {
                        Dock::Right => {
                            self.width = Some(area.right().saturating_sub(mouse.position.x));
                        }
                        Dock::Bottom => {
                            self.height = Some(area.bottom().saturating_sub(mouse.position.y));
                        }
                        Dock::Zoomed => {}
                    }
                    self.touch();
                    return Some(Outcome::Redraw);
                }
                MouseEventKind::Up(_) => {
                    self.drag = None;
                    return Some(Outcome::Redraw);
                }
                _ => {}
            }
        }
        if !panel.contains(mouse.position) {
            // The pointer left: the history row's `✕` goes back to the selected row. The move
            // stays the review's, so the panel asks for its own redraw.
            if mouse.kind == MouseEventKind::Move && self.hover.take().is_some() {
                self.touch();
                ui.request_redraw();
            }
            return None;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                Some(self.mouse_down(mouse.position, panel, dock, &painted, place, ui))
            }
            MouseEventKind::Move => {
                let hover = painted
                    .history
                    .filter(|area| area.contains(mouse.position))
                    .and_then(|area| {
                        let picker = self.picker.as_ref()?;
                        let index = picker.first.get() + usize::from(mouse.position.y - area.y);
                        (index < picker.matching().len()).then_some(index)
                    });
                if hover == self.hover {
                    return Some(Outcome::Ignored);
                }
                self.hover = hover;
                self.touch();
                Some(Outcome::Redraw)
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                if let Some(picker) = self.picker.as_mut() {
                    picker.move_by(if up { -1 } else { 1 });
                } else {
                    let rows = i64::try_from(WHEEL_ROWS).unwrap_or(1);
                    self.scroll_by(if up { rows } else { -rows });
                }
                Some(Outcome::Redraw)
            }
            _ => Some(Outcome::Ignored),
        }
    }

    /// A left press inside the panel at `position`.
    fn mouse_down(
        &mut self,
        position: Point,
        panel: Rect,
        dock: Dock,
        painted: &Painted,
        place: &Place,
        ui: &UiContext,
    ) -> Outcome {
        let Point { x, y } = position;
        self.visibility = Shown::Focused;
        self.touch();
        if let Some(outcome) = self.press_control(position, painted, place, ui) {
            return outcome;
        }
        let divider = match dock {
            Dock::Right => x == panel.x,
            Dock::Bottom => y == panel.y,
            Dock::Zoomed => false,
        };
        if divider {
            self.drag = Some(dock);
            return Outcome::Redraw;
        }
        if let Some(area) = painted.history.filter(|area| area.contains(position)) {
            let Some(picker) = self.picker.as_mut() else {
                return Outcome::Redraw;
            };
            let index = picker.first.get() + usize::from(y - area.y);
            if index < picker.matching().len() {
                picker.selected = index;
                return self.switch_selected(place, ui);
            }
            return Outcome::Redraw;
        }
        if let Some(area) = painted.feed.filter(|area| area.contains(position)) {
            let (first, count) = self.shown.get();
            let row = usize::from(y - area.y);
            if row < count {
                let row = first + row;
                let column = x - area.x;
                let action = self.rows.borrow().as_ref().and_then(|rows| {
                    rows.actions
                        .iter()
                        .find(|cells| {
                            usize::from(cells.row) == row && cells.cells.contains(&column)
                        })
                        .map(|cells| cells.action)
                });
                if let Some(ItemAction::Agent(action)) = action {
                    return self.feed_action(action, ui);
                }
            }
        }
        Outcome::Redraw
    }

    /// The active conversation's latest answer, once its turn finished.
    fn latest_answer(&self) -> Option<String> {
        let state = self.agents.state(self.session.as_deref()?)?;
        (!state.working && !state.answer.trim().is_empty()).then(|| state.answer.trim().to_owned())
    }

    fn chip_clicked(&mut self, chip: ChipId, place: &Place, ui: &UiContext) -> Outcome {
        match chip {
            ChipId::Context => {
                self.context = !self.context;
                Outcome::Redraw
            }
            ChipId::Model => self.run_action("assistant_model", place, ui),
            ChipId::Effort => self.run_action("assistant_effort", place, ui),
            ChipId::Tools => self.run_action("assistant_tools", place, ui),
            ChipId::Category(category) => {
                self.toggle_category(category);
                Outcome::Redraw
            }
        }
    }

    /// A press on a control: a history row's `✕` (arms, then deletes), a chip, a title-row
    /// button, or the title row (a double press zooms). `None` elsewhere.
    fn press_control(
        &mut self,
        position: Point,
        painted: &Painted,
        place: &Place,
        ui: &UiContext,
    ) -> Option<Outcome> {
        if let Some((_, session)) = painted
            .history_delete
            .as_ref()
            .filter(|(area, _)| area.contains(position))
        {
            let session = session.clone();
            if let Some(picker) = self.picker.as_mut() {
                if picker.marked.as_deref() == Some(session.as_str()) {
                    picker.marked = None;
                    self.delete(session, ui);
                } else {
                    picker.marked = Some(session);
                }
            }
            return Some(Outcome::Redraw);
        }
        if let Some((chip, _)) = painted
            .chips
            .iter()
            .find(|(_, area)| area.contains(position))
        {
            return Some(self.chip_clicked(*chip, place, ui));
        }
        if let Some((id, _)) = painted
            .actions
            .iter()
            .find(|(_, area)| area.contains(position))
        {
            return Some(match id.as_str() {
                "close" => {
                    self.visibility = Shown::Closed;
                    self.zoom = Zoom::Docked;
                    Outcome::Redraw
                }
                "new" => {
                    self.new_conversation();
                    Outcome::Redraw
                }
                "history" => {
                    if self.picker.is_some() {
                        self.picker = None;
                    } else {
                        self.open_history(ui);
                    }
                    Outcome::Redraw
                }
                "open" => self.open_in_bcode(),
                _ => Outcome::Redraw,
            });
        }
        if painted.title.is_some_and(|title| title.contains(position)) {
            let now = Instant::now();
            let double = self
                .title_press
                .is_some_and(|last| now.duration_since(last) <= DOUBLE_CLICK);
            self.title_press = (!double).then_some(now);
            if double {
                self.zoom = match self.zoom {
                    Zoom::Docked => Zoom::Zoomed,
                    Zoom::Zoomed => Zoom::Docked,
                };
                return Some(Outcome::Redraw);
            }
        }
        None
    }

    fn feed_action(&mut self, action: AgentAction, ui: &UiContext) -> Outcome {
        match action {
            AgentAction::Open => return self.open_in_bcode(),
            AgentAction::Copy => {
                if let Some(answer) = self.latest_answer() {
                    self.notice = Some(match ui.copy_text(&answer) {
                        Ok(()) => ("copied the answer".to_owned(), false),
                        Err(error) => (format!("could not copy: {}", error.message), true),
                    });
                }
            }
            AgentAction::Draft => {
                if let Some(answer) = self.latest_answer() {
                    return Outcome::DraftComment(answer);
                }
            }
            AgentAction::Stop => self.stop(ui),
            AgentAction::Retry => {
                if let Some(session) = self.session.clone() {
                    self.agents.retry(&session);
                }
            }
            AgentAction::Toggle(index) => {
                let Some(session) = self.session.clone() else {
                    return Outcome::Redraw;
                };
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
                self.answer(action, ui);
            }
            _ => {}
        }
        Outcome::Redraw
    }

    // ----------------------------------------------------------------------------------------
    // Model, effort and permissions
    // ----------------------------------------------------------------------------------------

    /// Read the app default and list the models once; again after a failed listing when
    /// `retry`.
    fn load_models(&mut self, retry: bool, ui: &UiContext) {
        if !self.models.default_asked || (retry && self.models.default_error.is_some()) {
            self.models.default_asked = true;
            self.models.default_error = None;
            let services = self.services.clone();
            let repo = self.repo.clone();
            let inbox = Arc::clone(&self.inbox);
            ui.spawn(Box::pin(async move {
                let result = super::models::load_default(&services, repo).await;
                push(&inbox, Done::Default(result));
            }));
        }
        let failed = matches!(self.models.catalog, Some(Err(_)));
        if self.models.listing || (self.models.catalog.is_some() && !(retry && failed)) {
            return;
        }
        self.models.listing = true;
        let host = Arc::clone(ui.host());
        let inbox = Arc::clone(&self.inbox);
        ui.spawn(Box::pin(async move {
            let result = host.model_catalog().await.map_err(|error| error.message);
            push(&inbox, Done::Catalog(result));
        }));
    }

    fn read_session_model(&mut self, session: String, ui: &UiContext) {
        self.model_asked = Some(session.clone());
        let host = Arc::clone(ui.host());
        let inbox = Arc::clone(&self.inbox);
        ui.spawn(Box::pin(async move {
            let result = host
                .session_model(session.clone())
                .await
                .map_err(|error| error.message);
            push(&inbox, Done::SessionModel { session, result });
        }));
    }

    /// Finished model work: a listing, the app default, a session's model or a choice.
    fn finish_models(&mut self, done: Done, ui: &UiContext) {
        match done {
            Done::Catalog(result) => {
                self.models.listing = false;
                self.models.catalog = Some(result);
                self.refresh_model_overlay(ui);
            }
            Done::Default(result) => match result {
                Ok(settings) => self.models.default = Some(settings),
                Err(error) => {
                    self.notice = Some((
                        format!("could not read the assistant's model: {error}"),
                        true,
                    ));
                    self.models.default_error = Some(error);
                }
            },
            Done::SessionModel {
                session,
                result: Ok(model),
            } => {
                if let Some(requested) = model
                    .unavailable
                    .as_ref()
                    .filter(|_| self.session.as_deref() == Some(session.as_str()))
                {
                    self.failure = Some(format!(
                        "Bcode does not offer the model {requested}; this conversation runs {} \
                         instead — choose a model with the model chip (alt+m)",
                        super::models::label(&model.model)
                    ));
                }
                self.models.learn(session, model);
            }
            Done::Chosen {
                session,
                default,
                result,
            } => {
                if let Some(default) = default {
                    self.models.default = Some(default);
                }
                match result {
                    Ok(notice) => self.notice = Some((notice, false)),
                    Err(error) => self.failure = Some(error),
                }
                if let Some(session) = session {
                    self.read_session_model(session, ui);
                }
            }
            Done::ListedAll(result) => match result {
                Ok((sessions, _)) if sessions.is_empty() => {
                    self.notice = Some(("the review app has no conversations".to_owned(), false));
                }
                Ok((sessions, more)) => self.mode = Mode::ConfirmDeleteAll(sessions, more),
                Err(error) => {
                    self.notice = Some((format!("could not list conversations: {error}"), true));
                }
            },
            Done::DeletedAll {
                deleted,
                refused,
                more,
            } => {
                if let Some(picker) = self.picker.as_mut() {
                    for session in &deleted {
                        picker.remove(session);
                    }
                }
                if self
                    .session
                    .as_ref()
                    .is_some_and(|session| deleted.contains(session))
                {
                    self.new_conversation();
                }
                let mut notice = format!(
                    "deleted {} conversation{}",
                    deleted.len(),
                    if deleted.len() == 1 { "" } else { "s" }
                );
                if let Some((_, reason)) = refused.first() {
                    use std::fmt::Write as _;
                    let _ = write!(notice, "; Bcode kept {} ({reason})", refused.len());
                }
                if more {
                    notice.push_str("; more remain, delete every conversation again for the rest");
                }
                self.notice = Some((notice, !refused.is_empty()));
            }
            _ => {}
        }
    }

    /// The session a target's choice applies to, when it has one.
    fn target_session(&self, target: &Target) -> Option<String> {
        match target {
            Target::Conversation => self.session.clone(),
            Target::Session(session) => Some(session.clone()),
            Target::AppDefault => None,
        }
    }

    /// Open the model picker for `target`.
    pub fn open_model_picker(&mut self, target: Target, ui: &UiContext) {
        self.load_models(true, ui);
        let theme = self.theme.get();
        let (entries, options) = self.model_entries(&target);
        let overlay =
            crate::palette::Overlay::new(crate::palette::OverlayKind::Models, entries, &theme);
        self.overlay = Some((OverlayPurpose::Models(target, options), overlay));
        self.touch();
    }

    /// Rebuild an open model picker once the models are listed.
    fn refresh_model_overlay(&mut self, ui: &UiContext) {
        let Some((OverlayPurpose::Models(target, _), _)) = &self.overlay else {
            return;
        };
        let target = target.clone();
        self.open_model_picker(target, ui);
    }

    /// The picker's rows: Bcode's default, then each provider's models (a provider that failed
    /// as a row with its reason), the current one marked.
    fn model_entries(
        &self,
        target: &Target,
    ) -> (
        Vec<crate::palette::OverlayEntry>,
        Vec<crate::host::ModelOption>,
    ) {
        use crate::palette::OverlayEntry;
        let session = self.target_session(target);
        let (current, _) = self.models.effective(session.as_deref());
        let chosen = session
            .as_deref()
            .and_then(|session| self.models.session(session))
            .is_some()
            || self.models.default_model().is_some();
        let mut entries = Vec::new();
        let mut options = Vec::new();
        let mark = |active: bool| if active { "● " } else { "  " };
        match &self.models.catalog {
            None => entries.push(OverlayEntry {
                action: None,
                label: "listing the models Bcode offers…".to_owned(),
                keys: String::new(),
            }),
            Some(Err(error)) => entries.push(OverlayEntry {
                action: None,
                label: format!("⚠ models unavailable: {error}"),
                keys: String::new(),
            }),
            Some(Ok(catalog)) => {
                let default = self.models.bcode_default();
                entries.push(OverlayEntry {
                    action: Some("model:default".to_owned()),
                    label: format!(
                        "{}Bcode's default{}",
                        mark(!chosen),
                        default.map_or_else(String::new, |model| format!(
                            " ({})",
                            super::models::label(model)
                        ))
                    ),
                    keys: String::new(),
                });
                for provider in &catalog.providers {
                    match &provider.models {
                        Err(error) => entries.push(OverlayEntry {
                            action: None,
                            label: format!("{} — unavailable: {error}", provider.name),
                            keys: String::new(),
                        }),
                        Ok(models) => {
                            entries.push(OverlayEntry {
                                action: None,
                                label: provider.name.clone(),
                                keys: String::new(),
                            });
                            for model in models {
                                let active = chosen
                                    && current.as_ref().is_some_and(|current| {
                                        current.is(model.provider.as_deref(), &model.model_id)
                                    });
                                entries.push(OverlayEntry {
                                    action: Some(format!("model:{}", options.len())),
                                    label: format!(
                                        "{}{} {}",
                                        mark(active),
                                        super::models::label(model),
                                        model.model_id
                                    ),
                                    keys: model_details(model),
                                });
                                options.push(model.clone());
                            }
                        }
                    }
                }
            }
        }
        (entries, options)
    }

    /// Use `choice` (Bcode's default when `None`) for `target`, and save it as the app default.
    fn choose_model(
        &mut self,
        target: &Target,
        choice: Option<&crate::host::ModelOption>,
        ui: &UiContext,
    ) {
        let session = self.target_session(target);
        let (_, effort) = self.models.effective(session.as_deref());
        let bcode_default = self.models.bcode_default().cloned();
        if choice.is_none() && session.is_some() && bcode_default.is_none() {
            // The session can only be put back on Bcode's default by naming that model.
            let notice = format!(
                "Bcode's default model is not known{}; this conversation keeps {}",
                self.models
                    .listed()
                    .and_then(|catalog| catalog.default.as_ref())
                    .and_then(|default| default.as_ref().err())
                    .map_or_else(String::new, |error| format!(" ({error})")),
                self.models.model_label(session.as_deref())
            );
            self.failure = Some(notice);
            return;
        }
        let model = choice.cloned().or_else(|| bcode_default.clone());
        let effort = model
            .as_ref()
            .and_then(|model| super::models::kept_effort(model, effort.as_deref()));
        let settings = choice.map_or_else(
            bcode_code_review_models::ReviewAssistantSettings::default,
            |choice| bcode_code_review_models::ReviewAssistantSettings {
                provider_plugin_id: choice.provider.clone(),
                model_id: Some(choice.model_id.clone()),
                effort: effort.clone(),
                updated_at_ms: 0,
            },
        );
        let name = model.as_ref().map_or_else(
            || "Bcode's default".to_owned(),
            |model| super::models::label(model).to_owned(),
        );
        let services = self.services.clone();
        let repo = self.repo.clone();
        let inbox = Arc::clone(&self.inbox);
        ui.spawn(Box::pin(async move {
            let result = async {
                if let (Some(session), Some(model)) = (&session, &model) {
                    let host = services.host();
                    host.set_session_model(
                        session.clone(),
                        model.provider.clone(),
                        model.model_id.clone(),
                    )
                    .await
                    .map_err(|error| {
                        format!("could not use the model {name}: {}", error.message)
                    })?;
                    host.set_session_reasoning(session.clone(), effort.clone())
                        .await
                        .map_err(|error| error.message)?;
                }
                let saved = super::models::save_default(&services, &repo, settings)
                    .await
                    .map_err(|error| format!("could not remember the model: {error}"))?;
                let notice = if session.is_some() {
                    format!("{name} from the next message")
                } else {
                    format!("{name} for new conversations")
                };
                Ok::<_, String>((notice, saved))
            }
            .await;
            let (result, default) = match result {
                Ok((notice, saved)) => (Ok(notice), Some(saved)),
                Err(error) => (Err(error), None),
            };
            push(
                &inbox,
                Done::Chosen {
                    session,
                    default,
                    result,
                },
            );
        }));
    }

    /// Cycle `target`'s reasoning effort over its model's values, and save it in the app
    /// default.
    pub fn cycle_effort(&mut self, target: &Target, ui: &UiContext) {
        self.load_models(false, ui);
        let session = self.target_session(target);
        let (model, effort) = self.models.effective(session.as_deref());
        let Some(model) = model else {
            self.notice = Some(("the models are still being listed".to_owned(), false));
            return;
        };
        let Some(next) = super::models::effort_after(
            &model.effort_values,
            effort.as_deref(),
            model.default_effort.as_deref(),
        ) else {
            self.notice = Some((
                format!(
                    "{} has no reasoning effort setting",
                    super::models::label(&model)
                ),
                false,
            ));
            return;
        };
        let Some(current) = self.models.default.as_ref() else {
            // Saving without the current default would overwrite the model it names.
            self.notice = Some((
                self.models.default_error.as_ref().map_or_else(
                    || "the assistant's model is still being read".to_owned(),
                    |error| format!("could not read the assistant's model: {error}"),
                ),
                true,
            ));
            return;
        };
        let settings = super::models::default_with_effort(
            current,
            &model,
            self.models.bcode_default(),
            next.clone(),
        );
        // An optimistic label until the session is read back.
        if let Some(session) = &session
            && let Some(known) = self.models.session(session).cloned()
        {
            self.models.learn(
                session.clone(),
                crate::host::SessionModel {
                    effort: Some(next.clone()),
                    ..known
                },
            );
        }
        self.models.default = Some(settings.clone());
        let services = self.services.clone();
        let repo = self.repo.clone();
        let inbox = Arc::clone(&self.inbox);
        ui.spawn(Box::pin(async move {
            let result = async {
                if let Some(session) = &session {
                    services
                        .host()
                        .set_session_reasoning(session.clone(), Some(next.clone()))
                        .await
                        .map_err(|error| {
                            format!("could not use effort {next}: {}", error.message)
                        })?;
                }
                let saved = super::models::save_default(&services, &repo, settings)
                    .await
                    .map_err(|error| format!("could not remember the effort: {error}"))?;
                Ok::<_, String>((format!("effort {next}"), saved))
            }
            .await;
            let (result, default) = match result {
                Ok((notice, saved)) => (Ok(notice), Some(saved)),
                Err(error) => (Err(error), None),
            };
            push(
                &inbox,
                Done::Chosen {
                    session,
                    default,
                    result,
                },
            );
        }));
    }

    /// Turn `category` on or off for the active conversation; a category off in configuration
    /// stays off.
    fn toggle_category(&mut self, category: Category) {
        let configured = self
            .services
            .assistant_config()
            .categories
            .get(&category)
            .copied()
            .unwrap_or(true);
        if !configured {
            self.notice = Some((
                format!(
                    "{} is turned off in configuration ({})",
                    category_name(category),
                    category.key()
                ),
                false,
            ));
            return;
        }
        let on = if self.off.remove(&category) {
            true
        } else {
            self.off.insert(category);
            false
        };
        self.notice = Some((
            format!(
                "{} {} for this conversation from the next message",
                category_name(category),
                if on { "on" } else { "off" }
            ),
            false,
        ));
        self.touch();
    }

    /// Whether `category` is on for the active conversation (and in configuration).
    fn category_on(&self, category: Category) -> bool {
        !self.off.contains(&category)
            && self
                .services
                .assistant_config()
                .categories
                .get(&category)
                .copied()
                .unwrap_or(true)
    }

    fn open_permissions(&mut self) {
        use crate::palette::OverlayEntry;
        let configured = self.services.assistant_config();
        let entries = CHIP_CATEGORIES
            .iter()
            .map(|category| {
                let state = if !configured.categories.get(category).copied().unwrap_or(true) {
                    "off in configuration"
                } else if self.off.contains(category) {
                    "off"
                } else {
                    "on"
                };
                OverlayEntry {
                    action: Some(format!("category:{}", category.key())),
                    label: format!(
                        "{}  {}",
                        category_glyph(*category),
                        category_name(*category)
                    ),
                    keys: format!("{state} · {}", category.tools().join(", ")),
                }
            })
            .collect();
        let overlay = crate::palette::Overlay::new(
            crate::palette::OverlayKind::Permissions,
            entries,
            &self.theme.get(),
        );
        self.overlay = Some((OverlayPurpose::Permissions, overlay));
        self.touch();
    }

    fn open_tools(&mut self) {
        use crate::palette::OverlayEntry;
        let configured = self.services.assistant_config();
        let mut entries = Vec::new();
        for category in Category::ALL {
            let on = self.category_on(category);
            entries.push(OverlayEntry {
                action: None,
                label: format!(
                    "{} — {}",
                    category_name(category),
                    if on { "on" } else { "off" }
                ),
                keys: String::new(),
            });
            let reads = matches!(category, Category::ReviewReads | Category::FileReads);
            for tool in category.tools() {
                entries.push(OverlayEntry {
                    action: Some(format!("tool:{tool}")),
                    label: (*tool).to_owned(),
                    keys: if !on {
                        "off".to_owned()
                    } else if reads {
                        "reads: runs".to_owned()
                    } else {
                        "changes: asks".to_owned()
                    },
                });
            }
        }
        if !configured.extra.is_empty() {
            entries.push(OverlayEntry {
                action: None,
                label: "extra".to_owned(),
                keys: String::new(),
            });
            for tool in &configured.extra {
                entries.push(OverlayEntry {
                    action: Some(format!("tool:{tool}")),
                    label: tool.clone(),
                    keys: "Bcode's rule".to_owned(),
                });
            }
        }
        let overlay = crate::palette::Overlay::new(
            crate::palette::OverlayKind::Tools,
            entries,
            &self.theme.get(),
        );
        self.overlay = Some((OverlayPurpose::Tools, overlay));
        self.touch();
    }

    /// List the app's conversations, then ask before deleting them all.
    fn ask_delete_all(&self, ui: &UiContext) {
        let services = self.services.clone();
        let repo = self.repo.clone();
        let inbox = Arc::clone(&self.inbox);
        ui.spawn(Box::pin(async move {
            let result = super::picker::list_all(services, repo).await;
            push(&inbox, Done::ListedAll(result));
        }));
    }

    /// Stop, delete and unbind each of `sessions`; sessions Bcode refuses are kept.
    fn delete_all(&self, sessions: Vec<String>, more: bool, ui: &UiContext) {
        let working: BTreeSet<String> = sessions
            .iter()
            .filter(|session| {
                self.agents
                    .state(session)
                    .is_some_and(|state| state.working)
            })
            .cloned()
            .collect();
        let services = self.services.clone();
        let repo = self.repo.clone();
        let inbox = Arc::clone(&self.inbox);
        ui.spawn(Box::pin(async move {
            let host = services.host();
            let (mut deleted, mut refused) = (Vec::new(), Vec::new());
            for session in sessions {
                if working.contains(&session) {
                    let _ = host.cancel_turn(session.clone()).await;
                }
                match host.delete_session(session.clone()).await {
                    Ok(()) => {
                        let _ = services
                            .call(UnbindReviewAssistantRequest {
                                repo_path: repo.clone(),
                                session_id: session.clone(),
                            })
                            .await;
                        deleted.push(session);
                    }
                    Err(error) => refused.push((session, error.message)),
                }
            }
            push(
                &inbox,
                Done::DeletedAll {
                    deleted,
                    refused,
                    more,
                },
            );
        }));
    }

    /// Run one of the panel's actions (a key, a chip, or a screen's palette).
    pub fn run_action(&mut self, action: &str, place: &Place, ui: &UiContext) -> Outcome {
        self.touch();
        match action {
            "assistant_close" => self.visibility = Shown::Closed,
            "assistant_back" => self.visibility = Shown::Open,
            "assistant_stop" => self.stop(ui),
            "assistant_context" => self.context = !self.context,
            "assistant_new" => self.new_conversation(),
            "assistant_open" => return self.open_in_bcode(),
            "assistant_history" => self.open_history(ui),
            "assistant_rename" => self.start_rename(),
            "assistant_delete" => {
                if self.session.is_some() {
                    self.mode = Mode::ConfirmDelete;
                } else {
                    self.notice = Some(("start or open a conversation first".to_owned(), false));
                }
            }
            "assistant_delete_all" => self.ask_delete_all(ui),
            "assistant_model" => self.open_model_picker(Target::Conversation, ui),
            "assistant_effort" => self.cycle_effort(&Target::Conversation, ui),
            "assistant_permissions" => self.open_permissions(),
            "assistant_tools" => self.open_tools(),
            "assistant_scroll_up" | "assistant_scroll_down" => {
                let page = self.shown.get().1.max(2) - 1;
                let rows = i64::try_from(page).unwrap_or(1);
                self.scroll_by(if action == "assistant_scroll_up" {
                    rows
                } else {
                    -rows
                });
            }
            _ => return Outcome::Ignored,
        }
        let _ = place;
        Outcome::Redraw
    }

    /// Whether an overlay is open (it takes every input).
    #[must_use]
    pub const fn overlay_open(&self) -> bool {
        self.overlay.is_some()
    }

    /// Paint the open overlay over the app's `area`.
    pub fn paint_overlay(&mut self, area: Rect, theme: &ReviewTheme, cx: &mut PaintCx<'_, '_>) {
        if let Some((_, overlay)) = self.overlay.as_mut() {
            overlay.paint(theme, area, cx);
        }
    }

    /// Route an input to the open overlay; `None` when none is open.
    pub fn overlay_event(&mut self, event: &Event, ui: &UiContext) -> Option<Outcome> {
        let theme = self.theme.get();
        let (_, overlay) = self.overlay.as_mut()?;
        let outcome = match event {
            Event::Key(stroke) => overlay.key(*stroke, &theme),
            Event::Paste(text) => overlay.paste(text, &theme),
            Event::Mouse(_) => overlay.mouse(event, &theme),
            _ => crate::palette::OverlayOutcome::Redraw,
        };
        self.touch();
        Some(match outcome {
            crate::palette::OverlayOutcome::Close => {
                self.overlay = None;
                Outcome::Redraw
            }
            crate::palette::OverlayOutcome::Run(id) => {
                let (purpose, _) = self.overlay.take()?;
                self.overlay_choice(purpose, &id, ui);
                Outcome::Redraw
            }
            crate::palette::OverlayOutcome::Redraw | crate::palette::OverlayOutcome::Ignored => {
                Outcome::Redraw
            }
        })
    }

    fn overlay_choice(&mut self, purpose: OverlayPurpose, id: &str, ui: &UiContext) {
        match purpose {
            OverlayPurpose::Models(target, options) => {
                if id == "model:default" {
                    self.choose_model(&target, None, ui);
                } else if let Some(option) = id
                    .strip_prefix("model:")
                    .and_then(|index| index.parse::<usize>().ok())
                    .and_then(|index| options.get(index).cloned())
                {
                    self.choose_model(&target, Some(&option), ui);
                }
            }
            OverlayPurpose::Permissions => {
                if let Some(category) = id.strip_prefix("category:").and_then(|key| {
                    Category::ALL
                        .into_iter()
                        .find(|category| category.key() == key)
                }) {
                    self.toggle_category(category);
                }
            }
            OverlayPurpose::Tools => {
                if let Some(tool) = id.strip_prefix("tool:") {
                    self.notice = Some((
                        format!(
                            "change how Bcode decides {tool} in bcode.toml: \
                             [agent.build.permission.tools] \"{tool}\" = \"allow\", \"ask\" or \"deny\""
                        ),
                        false,
                    ));
                }
            }
        }
    }

    // ----------------------------------------------------------------------------------------
    // Painting
    // ----------------------------------------------------------------------------------------

    /// Footer hints while the panel is focused.
    #[must_use]
    pub fn hints(&self) -> Vec<(String, String)> {
        let marked = self.picker.as_ref().and_then(|picker| {
            let marked = picker.marked.as_ref()?;
            picker
                .matching()
                .into_iter()
                .find(|row| row.session_id == *marked)
                .map(|row| row.title.clone())
        });
        let mut hints: Vec<(String, String)> = Vec::new();
        if let Some(title) = marked {
            hints.push((
                self.key_of("history_delete")
                    .unwrap_or_else(|| "✕".to_owned()),
                format!("again deletes “{title}”"),
            ));
        }
        let mut push = |key: Option<String>, label: &str| {
            if let Some(key) = key.filter(|key| !key.is_empty()) {
                hints.push((key, label.to_owned()));
            }
        };
        if self.picker.is_some() {
            push(Some("enter".to_owned()), "switch");
            push(self.key_of("history_rename"), "rename");
            push(self.key_of("history_delete"), "delete");
            push(self.key_of("history_open"), "open in Bcode");
            push(self.key_of("history_back"), "back");
            return hints;
        }
        match &self.mode {
            Mode::Rename(_) => {
                push(Some("enter".to_owned()), "rename");
                push(Some("esc".to_owned()), "cancel");
                return hints;
            }
            Mode::ConfirmDelete | Mode::ConfirmDeleteAll(..) => {
                push(Some("y".to_owned()), "delete");
                push(Some("n".to_owned()), "keep");
                return hints;
            }
            Mode::Chat => {}
        }
        if let Some(permission) = self.pending_permission() {
            let keys = self.agents.keys();
            let remember =
                permission.can_remember && !NEVER_REMEMBER.contains(&permission.tool.as_str());
            push(Some(keys.allow.clone()), "allow");
            if remember {
                push(Some(keys.always.clone()), "always");
            }
            push(Some(keys.decline.clone()), "decline");
            if remember {
                push(Some(keys.never.clone()), "never");
            }
            push(self.key_of("assistant_back"), "back");
            return hints;
        }
        push(Some("enter".to_owned()), "send");
        push(self.key_of("assistant_model"), "model");
        push(self.key_of("assistant_effort"), "effort");
        push(self.key_of("assistant_permissions"), "permissions");
        push(self.key_of("assistant_history"), "history");
        push(self.key_of("assistant_new"), "new");
        push(self.key_of("assistant_stop"), "stop");
        push(self.key_of("assistant_context"), "context");
        if self.session.is_some() {
            push(self.key_of("assistant_open"), "open in Bcode");
        }
        push(self.key_of("assistant_back"), "back");
        push(self.key_of("assistant_close"), "close");
        hints
    }

    /// The title row's text: `✦ Assistant · <title> · <tag>`.
    #[must_use]
    pub fn title_text(&self, theme: &ReviewTheme) -> String {
        let mut title = format!("{} Assistant", theme.icons.assistant);
        if let Some(name) = &self.title {
            title.push_str(theme.icons.separator);
            title.push_str(name);
        }
        if let Some(tag) = &self.tag {
            title.push_str(theme.icons.separator);
            title.push_str(&tag.label());
        }
        title
    }

    fn status_text(&self) -> String {
        let Some(state) = self.session.as_deref().and_then(|s| self.agents.state(s)) else {
            return String::new();
        };
        let mut parts = Vec::new();
        if state.needs_you() {
            parts.push("needs you".to_owned());
        } else if state.working {
            parts.push("working…".to_owned());
        }
        if state.feed.tokens > 0 {
            parts.push(usage_tokens(state.feed.tokens));
        }
        if let Some(cost) = &state.feed.cost {
            parts.push(cost.clone());
        }
        parts.join(" · ")
    }

    /// Paint the panel into its rectangle, docked as given, within the app's `area`.
    pub fn paint(
        &self,
        area: Rect,
        (rect, dock): (Rect, Dock),
        place: &Place,
        now_ms: u64,
        theme: &ReviewTheme,
        cx: &mut PaintCx<'_, '_>,
    ) {
        self.theme.set(*theme);
        let mut painted = Painted {
            panel: Some((rect, dock)),
            area,
            ..Painted::default()
        };
        cx.fill(
            LocalRect::new(rect.x.into(), rect.y.into(), rect.width, rect.height),
            " ",
            theme.canvas,
        );
        if rect.width == 0 || rect.height == 0 {
            *self.painted.borrow_mut() = painted;
            return;
        }
        let focused = self.is_focused();
        let title = self.title_text(theme);
        if rect.width < TITLE_ONLY_WIDTH || rect.height < 3 {
            let style = if focused {
                theme.pane_title_focused
            } else {
                theme.pane_title
            };
            let line = Line::from_spans(vec![Span::styled(title, style)])
                .truncate(usize::from(rect.width));
            cx.write_line(
                LocalRect::new(rect.x.into(), rect.y.into(), rect.width, 1),
                &line,
            );
            *self.painted.borrow_mut() = painted;
            return;
        }
        let compact = rect.width < COMPACT_WIDTH || rect.height < COMPACT_HEIGHT;
        let pane_rect = Rect::new(0, 0, rect.width, rect.height);
        let chrome = self.chrome_with_usage(theme, &title, focused, compact, pane_rect);
        painted.title = Some(Rect::new(rect.x, rect.y, rect.width, 1));
        let ids = self.title_actions();
        for (index, action_area) in chrome.title_layout(pane_rect).actions {
            let id = ids.get(index).copied().unwrap_or("");
            painted.actions.push((
                id.to_owned(),
                Rect::new(
                    rect.x + action_area.x,
                    rect.y + action_area.y,
                    action_area.width,
                    action_area.height,
                ),
            ));
        }
        let mut state = self.pane.get();
        state.interaction.focused = focused;
        self.pane.set(state);
        let content = self.content(place, now_ms, theme, compact, focused);
        let component = PaneComponent::new(PANEL_ID, chrome, &self.pane, content);
        // Lay out once to learn the conversation's width, prepare its rows, then paint.
        let layout = component.layout(Constraints::tight(rect.size()), &mut LayoutCx::new());
        if let Some(node) = layout.find(&LayoutId::new(FEED_ID)) {
            self.prepare_rows(
                u16::try_from(node.size.width).unwrap_or(u16::MAX),
                place,
                theme,
            );
        }
        cx.paint_component(&component, rect);
        let absolute = |id: &'static str| {
            layout.find_rect(&LayoutId::new(id)).map(|found| {
                Rect::new(
                    rect.x.saturating_add(found.x),
                    rect.y.saturating_add(found.y),
                    found.width,
                    found.height,
                )
            })
        };
        painted.feed = absolute(FEED_ID);
        painted.history = absolute(HISTORY_ID);
        painted.chips = absolute(super::chips::CHIPS_ID)
            .map(|chips| self.chip_regions(chips))
            .unwrap_or_default();
        painted.history_delete = painted
            .history
            .and_then(|area| self.history_delete_region(area));
        *self.painted.borrow_mut() = painted;
    }

    /// Each painted chip's cells, from the plan the chips row was laid out with at `chips`.
    fn chip_regions(&self, chips: Rect) -> Vec<(ChipId, Rect)> {
        self.chip_plan
            .borrow()
            .placed
            .iter()
            .filter(|(_, cell, _)| cell.y < chips.height)
            .map(|(id, cell, _)| {
                (
                    *id,
                    Rect::new(chips.x + cell.x, chips.y + cell.y, cell.width, 1),
                )
            })
            .collect()
    }

    /// The cells and session of the `✕` painted on a history row of the rows at `area`.
    fn history_delete_region(&self, area: Rect) -> Option<(Rect, String)> {
        let (row, column) = self.history_delete.get()?;
        let picker = self.picker.as_ref()?;
        let session = picker.matching().get(row)?.session_id.clone();
        let y = u16::try_from(row.checked_sub(picker.first.get())?).ok()?;
        (y < area.height && column < area.width).then(|| {
            (
                Rect::new(area.x + column, area.y + y, area.width - column, 1),
                session,
            )
        })
    }

    /// The chrome, with the title cut when it would leave the usage no room on the title row.
    fn chrome_with_usage(
        &self,
        theme: &ReviewTheme,
        title: &str,
        focused: bool,
        compact: bool,
        pane_rect: Rect,
    ) -> Pane<'static> {
        let chrome = self.chrome(theme, title, focused, compact);
        let status = self.status_text();
        let layout = chrome.title_layout(pane_rect);
        if compact || status.is_empty() || layout.right_title.is_some() {
            return chrome;
        }
        let actions_start = layout
            .actions
            .first()
            .map_or_else(|| pane_rect.width.saturating_sub(1), |(_, area)| area.x);
        let status_width = u16::try_from(theme.pane_status(&status).width()).unwrap_or(u16::MAX);
        // The corner, the title's own padding, and a cell on each side of the status.
        let room = actions_start.saturating_sub(status_width.saturating_add(5));
        if room < MIN_TITLE_CELLS {
            return chrome;
        }
        let cut = Line::from_spans(vec![Span::raw(title.to_owned())]).truncate(usize::from(room));
        let cut: String = cut.spans.iter().map(|span| span.content.as_str()).collect();
        self.chrome(theme, &cut, focused, compact)
    }

    /// The title row's buttons, in order: `+ new`, `⟲`, `↗` (only with a session) and `✕`.
    fn title_actions(&self) -> Vec<&'static str> {
        let mut ids = vec!["new", "history"];
        if self.session.is_some() {
            ids.push("open");
        }
        ids.push("close");
        ids
    }

    fn chrome(
        &self,
        theme: &ReviewTheme,
        title: &str,
        focused: bool,
        compact: bool,
    ) -> Pane<'static> {
        let mut chrome = Pane::new()
            .title(theme.pane_title(title, focused))
            .styles(PaneStyles {
                background: None,
                border: theme.border,
                focused_border: theme.border_focused,
            })
            .chrome(theme.component.chrome);
        if focused && !compact {
            let hints = self
                .hints()
                .into_iter()
                .map(|(key, label)| format!("{key} {label}"))
                .collect::<Vec<_>>()
                .join(" · ");
            chrome = chrome.footer(Line::from_spans(vec![Span::styled(
                format!(" {hints} "),
                theme.muted,
            )]));
        }
        if !compact {
            chrome = chrome.right_title(theme.pane_status(&self.status_text()));
            for id in self.title_actions() {
                let label = match id {
                    "new" => "+ new".to_owned(),
                    "history" => "⟲".to_owned(),
                    "open" => theme.icons.external.to_owned(),
                    _ => "✕".to_owned(),
                };
                chrome = chrome.action(PaneTitleAction::new(
                    id,
                    Line::from_spans(vec![Span::styled(format!(" {label} "), theme.accent)]),
                ));
            }
        }
        chrome
    }

    fn content<'a>(
        &'a self,
        place: &Place,
        now_ms: u64,
        theme: &'a ReviewTheme,
        compact: bool,
        focused: bool,
    ) -> Column<'a> {
        let mut content = Column::new().id("app.assistant.content");
        if let Some(picker) = &self.picker {
            return self.history_content(content, picker, now_ms, theme, focused);
        }
        if !compact {
            content = content.child(ChipRow::new(self.chips(place, theme), &self.chip_plan));
            if let Some((text, error)) = &self.notice {
                let style = if *error { theme.error } else { theme.muted };
                content = content.child(one_line(Line::from_spans(vec![Span::styled(
                    text.clone(),
                    style,
                )])));
            }
            content = content.flex(Flex::new(1, Feed { panel: self }));
        }
        match &self.mode {
            Mode::Rename(input) => {
                return content.child(
                    Row::new()
                        .id("app.assistant.rename")
                        .child(TextBlock::new(Text::from_lines(vec![Line::from_spans(
                            vec![Span::styled("rename: ", theme.muted)],
                        )])))
                        .flex(Flex::new(
                            1,
                            TextInputComponent::new(
                                "app.assistant.rename_input",
                                input,
                                &RENAME_POLICY,
                            )
                            .style(theme.input)
                            .focused(focused),
                        )),
                );
            }
            Mode::ConfirmDelete => {
                return content.child(one_line(Line::from_spans(vec![Span::styled(
                    "Delete this conversation from Bcode? y delete · n keep",
                    theme.pill_warning,
                )])));
            }
            Mode::ConfirmDeleteAll(sessions, more) => {
                let count = sessions.len();
                let plural = if count == 1 { "" } else { "s" };
                let question = if *more {
                    format!(
                        "Delete the newest {count} conversation{plural} the review app started from Bcode (more remain)? y delete · n keep"
                    )
                } else {
                    format!(
                        "Delete all {count} conversation{plural} the review app started from Bcode? y delete · n keep"
                    )
                };
                return content.child(one_line(Line::from_spans(vec![Span::styled(
                    question,
                    theme.pill_warning,
                )])));
            }
            Mode::Chat => {}
        }
        if self.prompt_pending() {
            let hint = self
                .hints()
                .into_iter()
                .map(|(key, label)| format!("{key} {label}"))
                .collect::<Vec<_>>()
                .join(" · ");
            return content.child(one_line(Line::from_spans(vec![Span::styled(
                hint,
                theme.pill_warning,
            )])));
        }
        let placeholder = match place.screen {
            Screen::Review => "Ask about this review…",
            Screen::Home => "Ask me to create or open a review…",
            Screen::Wizard | Screen::Opening => "Ask the assistant…",
        };
        content.child(
            Row::new()
                .id("app.assistant.input_row")
                .child(TextBlock::new(Text::from_lines(vec![Line::from_spans(
                    vec![
                        Span::styled(theme.icons.accent_bar, theme.accent),
                        Span::raw(" "),
                    ],
                )])))
                .flex(Flex::new(
                    1,
                    TextInputComponent::new(INPUT_ID, &self.input, &self.policy)
                        .style(theme.input)
                        .placeholder(placeholder, theme.placeholder)
                        .focused(focused),
                )),
        )
    }

    /// The history picker: its search input, a note while loading or degraded, and its rows.
    fn history_content<'a>(
        &'a self,
        mut content: Column<'a>,
        picker: &'a Picker,
        now_ms: u64,
        theme: &'a ReviewTheme,
        focused: bool,
    ) -> Column<'a> {
        let search = Row::new()
            .id("app.assistant.search")
            .child(TextBlock::new(Text::from_lines(vec![Line::from_spans(
                vec![Span::styled(
                    format!("{} ", theme.icons.filter),
                    theme.muted,
                )],
            )])))
            .flex(Flex::new(
                1,
                TextInputComponent::new(
                    "app.assistant.search_input",
                    picker.query_input(),
                    picker.policy(),
                )
                .style(theme.input)
                .placeholder("search conversations", theme.placeholder)
                .focused(focused),
            ));
        let rows = HistoryRows {
            panel: self,
            now_ms,
            theme,
        };
        let note = match &picker.load {
            None => Some(("loading conversations…".to_owned(), theme.muted)),
            Some(Err(error)) => Some((
                format!("could not list conversations: {error}"),
                theme.error,
            )),
            Some(Ok(load)) if load.rows.is_empty() => {
                Some(("no conversations yet".to_owned(), theme.muted))
            }
            Some(Ok(load)) if load.degraded => Some((
                "Bcode is still loading sessions; some may be missing".to_owned(),
                theme.notice,
            )),
            Some(Ok(_)) => None,
        };
        content = content.child(search);
        if let Some((text, style)) = note {
            content = content.child(one_line(Line::from_spans(vec![Span::styled(text, style)])));
        }
        content.flex(Flex::new(1, rows))
    }

    /// The chips row: the context a message carries, the model and effort it runs with, the
    /// permission categories and the tools list.
    fn chips(&self, place: &Place, theme: &ReviewTheme) -> Vec<Chip> {
        let text = |text: String, style| Line::from_spans(vec![Span::styled(text, style)]);
        let mut chips = Vec::new();
        let (context, context_style) = if self.context {
            (format!(" ⌖ {} ", place.chip()), theme.pill_accent)
        } else {
            ("context detached".to_owned(), theme.tiers.dim)
        };
        chips.push(Chip {
            id: ChipId::Context,
            short: text(
                if self.context {
                    " ⌖ ".to_owned()
                } else {
                    "⌖ off".to_owned()
                },
                context_style,
            ),
            full: text(context, context_style),
            keep: 40,
        });
        let session = self.session.as_deref();
        let model = self.models.model_label(session);
        let model_style = if matches!(self.models.catalog, Some(Err(_))) {
            theme.error
        } else {
            theme.accent
        };
        let glyph = if model.starts_with('⚠') { "" } else { "● " };
        chips.push(Chip {
            id: ChipId::Model,
            full: text(format!("{glyph}{model} ▾"), model_style),
            short: text(format!("{glyph}{model}"), model_style),
            keep: 50,
        });
        if let Some(effort) = self.models.effort_label(session) {
            chips.push(Chip {
                id: ChipId::Effort,
                full: text(format!("◆ effort {effort}"), theme.muted),
                short: text(format!("◆ {effort}"), theme.muted),
                keep: 30,
            });
        }
        for (index, category) in CHIP_CATEGORIES.into_iter().enumerate() {
            let style = if self.category_on(category) {
                theme.text
            } else {
                theme
                    .tiers
                    .dim
                    .add_modifier(bmux_tui::style::Modifier::CROSSED_OUT)
            };
            chips.push(Chip {
                id: ChipId::Category(category),
                full: text(
                    format!("{} {}", category_glyph(category), category_name(category)),
                    style,
                ),
                short: text(category_glyph(category).to_owned(), style),
                keep: 20 - u16::try_from(index).unwrap_or(0),
            });
        }
        chips.push(Chip {
            id: ChipId::Tools,
            full: text("⚒ tools".to_owned(), theme.muted),
            short: text("⚒".to_owned(), theme.muted),
            keep: 10,
        });
        chips
    }

    /// Lay out the conversation at `width` columns when it changed.
    fn prepare_rows(&self, width: u16, place: &Place, theme: &ReviewTheme) {
        let session = self.session.clone();
        let key = (
            session
                .as_deref()
                .map_or(0, |session| self.agents.session_revision(session)),
            self.folds.revision,
            self.revision,
            width,
            place.screen.name(),
        );
        if self
            .rows
            .borrow()
            .as_ref()
            .is_some_and(|rows| rows.key == key)
        {
            return;
        }
        let width = usize::from(width);
        let draft = place.screen == Screen::Review;
        let (mut lines, actions) = session.as_deref().map_or_else(
            || (self.intro(place.screen, width, theme), Vec::new()),
            |session| {
                crate::rows::dock_rows(session, &self.agents, &self.folds, theme, width, draft)
            },
        );
        if let Some(failure) = &self.failure {
            lines.push(Line::default());
            lines.extend(wrapped(
                &Line::from_spans(vec![
                    Span::styled("✗ ", theme.error),
                    Span::styled(failure.clone(), theme.error),
                ]),
                width,
            ));
        }
        *self.rows.borrow_mut() = Some(Rows {
            key,
            lines,
            actions,
        });
    }

    /// What an empty conversation shows on `screen`: what the assistant can do there, example
    /// requests, what the chips control, and why the models are unavailable when they are.
    fn intro(&self, screen: Screen, width: usize, theme: &ReviewTheme) -> Vec<Line> {
        if self.starting {
            return vec![
                Line::from_spans(vec![Span::styled("starting the assistant…", theme.muted)])
                    .truncate(width),
            ];
        }
        let (lead, examples): (&str, [&str; 3]) = match screen {
            Screen::Review => (
                "Ask about this review, or ask me to mark files viewed, change the comparison, comment or move you around.",
                [
                    "“What changed since I last looked?”",
                    "“Mark the generated files viewed”",
                    "“Show v2 of #102 against now”",
                ],
            ),
            Screen::Home | Screen::Opening => (
                "Ask me to create or open a review, find your pull requests, or tidy your reviews.",
                [
                    "“Create a review for my most recent PR stack”",
                    "“Review my uncommitted changes”",
                    "“Archive reviews I finished last week”",
                ],
            ),
            Screen::Wizard => (
                "Ask me to fill in the new review: which pull requests, branches or commits.",
                [
                    "“Review PR #1234 and its stack”",
                    "“Compare my branch with main”",
                    "“Only the last three commits”",
                ],
            ),
        };
        let mut lines = vec![Line::from_spans(vec![Span::styled(
            lead.to_owned(),
            theme.tiers.strong,
        )])];
        lines.extend(examples.iter().map(|example| {
            Line::from_spans(vec![
                Span::styled("  ", theme.muted),
                Span::styled((*example).to_owned(), theme.text),
            ])
        }));
        lines.push(Line::from_spans(vec![Span::styled(
            "The chips above choose the model (alt+m), its effort (shift+tab) and what the assistant may change (alt+p); changes ask for permission first.",
            theme.muted,
        )]));
        if let Some(Err(error)) = &self.models.catalog {
            lines.push(Line::from_spans(vec![Span::styled(
                format!("⚠ models unavailable: {error}"),
                theme.error,
            )]));
        }
        lines.iter().flat_map(|line| wrapped(line, width)).collect()
    }
}

/// The categories the chips row and the permissions overlay switch (reads have no chip).
pub const CHIP_CATEGORIES: [Category; 5] = [
    Category::ReviewWrites,
    Category::FileWrites,
    Category::Shell,
    Category::Publish,
    Category::Navigation,
];

/// A category's glyph on its chip.
#[must_use]
pub const fn category_glyph(category: Category) -> &'static str {
    match category {
        Category::ReviewWrites => "✎",
        Category::FileWrites => "▤",
        Category::Shell => "$",
        Category::Publish => "⇪",
        Category::Navigation => "→",
        Category::ReviewReads | Category::FileReads => "◇",
    }
}

/// A category's short name on its chip.
#[must_use]
pub const fn category_name(category: Category) -> &'static str {
    match category {
        Category::ReviewWrites => "review",
        Category::FileWrites => "files",
        Category::Shell => "shell",
        Category::Publish => "publish",
        Category::Navigation => "move",
        Category::ReviewReads => "review reads",
        Category::FileReads => "file reads",
    }
}

/// A model's details in the picker: context window, prices per million tokens and effort
/// range.
fn model_details(model: &crate::host::ModelOption) -> String {
    let mut parts = Vec::new();
    if let Some(window) = model.context_window {
        parts.push(if window >= 1_000_000 {
            format!("{}M ctx", window / 1_000_000)
        } else {
            format!("{}k ctx", window / 1_000)
        });
    }
    if let Some((input, output)) = &model.price {
        parts.push(format!("{input}/{output} per M"));
    }
    match (model.effort_values.first(), model.effort_values.last()) {
        (Some(first), Some(last)) if first != last => parts.push(format!("effort {first}–{last}")),
        (Some(only), _) => parts.push(format!("effort {only}")),
        _ => {}
    }
    parts.join(" · ")
}

/// `line` wrapped at word boundaries into rows of at most `width` cells.
fn wrapped(line: &Line, width: usize) -> Vec<Line> {
    bmux_tui::text::wrap_line_bounded(
        line,
        bmux_tui::text::TextWrapGeometry::uniform(width),
        TextWrap::Word,
    )
}

/// Tokens on the title row, as bdata's panel shows them: `850 tok`, `12.3k tok`, `1.2M tok`.
#[must_use]
pub fn usage_tokens(tokens: u64) -> String {
    match tokens {
        0..1_000 => format!("{tokens} tok"),
        1_000..1_000_000 => format!("{}.{}k tok", tokens / 1_000, tokens % 1_000 / 100),
        _ => format!(
            "{}.{}M tok",
            tokens / 1_000_000,
            tokens % 1_000_000 / 100_000
        ),
    }
}

/// What a refused message or start says in the conversation: what failed, why, and what to do
/// for the reasons the app knows.
fn failure_text(what: &str, code: Option<&str>, message: &str) -> String {
    let hint = match code {
        Some("agent_profile_unavailable") => Some(
            "the review app's tool connection is not registered with Bcode yet; send again in a moment",
        ),
        Some(
            "model_not_found"
            | "model_unavailable"
            | "no_model_configured"
            | "provider_unavailable",
        ) => Some("choose another model with the model chip or in bcode tui"),
        _ => None,
    };
    let mut text = format!("{what}: {message}");
    if let Some(code) = code.filter(|code| *code != crate::host::HOST_ERROR_FAILED) {
        use std::fmt::Write as _;
        let _ = write!(text, " ({code})");
    }
    if let Some(hint) = hint {
        text.push_str(" — ");
        text.push_str(hint);
    }
    text
}

/// Editing policy of the rename input.
static RENAME_POLICY: std::sync::LazyLock<TextInputPolicy> =
    std::sync::LazyLock::new(single_line_policy);

/// A one-row input: `enter` submits, nothing inserts a newline.
#[must_use]
pub const fn single_line_policy() -> TextInputPolicy {
    let mut policy = TextInputPolicy::chat_composer();
    policy.keyboard.shift_enter = None;
    policy.viewport.max_rows = Some(1);
    policy.edge = EdgePolicy::disabled();
    policy
}

fn empty_input() -> TextInputState {
    TextInputState::new(bmux_text_edit::TextEditBuffer::new())
}

fn push(inbox: &Arc<Mutex<Vec<Done>>>, done: Done) {
    if let Ok(mut queue) = inbox.lock() {
        queue.push(done);
    }
}

fn one_line(line: Line) -> TextBlock {
    TextBlock::new(Text::from_lines(vec![line])).wrap(TextWrap::None)
}

/// A new conversation's title: the first line of its first message, bounded.
fn conversation_title(text: &str) -> String {
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("Conversation");
    let first = first.trim();
    if first.chars().count() > MAX_TITLE_CHARS {
        let cut: String = first.chars().take(MAX_TITLE_CHARS - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        first.to_owned()
    }
}

/// How a session's binding changes.
#[derive(Debug)]
enum Attachment {
    /// Attached to the review on screen.
    Review(Box<ReviewAssistantBinding>),
    /// Detached from any review.
    Detach,
}

impl Attachment {
    fn of(binding: Option<ReviewAssistantBinding>) -> Self {
        binding.map_or(Self::Detach, |binding| Self::Review(Box::new(binding)))
    }
}

/// Attach `session` to a review, or detach its stored binding.
async fn attach(
    services: &ReviewServices,
    repo: &std::path::Path,
    session: &str,
    attachment: Attachment,
) -> Result<(), String> {
    let binding = match attachment {
        Attachment::Review(binding) => *binding,
        Attachment::Detach => {
            let stored = services
                .call(GetReviewAssistantRequest {
                    repo_path: repo.to_path_buf(),
                    lookup: ReviewAssistantLookup::Session {
                        session_id: session.to_owned(),
                    },
                })
                .await
                .map_err(|error| error.message)?
                .binding;
            let Some(mut stored) = stored.filter(|stored| !stored.is_detached()) else {
                return Ok(());
            };
            stored.detached_at_ms = Some(now_ms());
            stored
        }
    };
    services
        .call(BindReviewAssistantRequest {
            repo_path: repo.to_path_buf(),
            binding,
        })
        .await
        .map(drop)
        .map_err(|error| error.message)
}

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The conversation: the rows laid out by [`Assistant::prepare_rows`], scrolled to the newest
/// unless the user scrolled up.
struct Feed<'a> {
    panel: &'a Assistant,
}

impl Feed<'_> {
    fn window(&self, height: usize) -> (usize, usize) {
        let total = self
            .panel
            .rows
            .borrow()
            .as_ref()
            .map_or(0, |rows| rows.lines.len());
        let max = total.saturating_sub(height);
        let scroll = self.panel.scroll.get().min(max);
        self.panel.scroll.set(scroll);
        (max - scroll, total.min(height))
    }
}

impl Component for Feed<'_> {
    fn revision(&self) -> ComponentRevision {
        let key = self.panel.rows.borrow().as_ref().map_or(0, |rows| {
            rows.key.0 ^ rows.key.1.rotate_left(17) ^ rows.key.2.rotate_left(31)
        });
        ComponentRevision::new(key, key ^ self.panel.scroll.get() as u64)
    }

    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            LayoutId::new(FEED_ID),
            constraints.constrain(LogicalSize::new(
                constraints.max_width(),
                constraints.max_height().unwrap_or(0),
            )),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let height = usize::try_from(layout.size.height).unwrap_or(usize::MAX);
        let (first, count) = self.window(height);
        self.panel.shown.set((first, count));
        let lines: Vec<Line> = self
            .panel
            .rows
            .borrow()
            .as_ref()
            .map(|rows| rows.lines[first..first + count].to_vec())
            .unwrap_or_default();
        // Rows were laid out at this width; clipping keeps them (and their click cells) in place.
        let block = TextBlock::new(Text::from_lines(lines)).wrap(TextWrap::None);
        let size = Size::new(
            u16::try_from(layout.size.width).unwrap_or(u16::MAX),
            u16::try_from(layout.size.height).unwrap_or(u16::MAX),
        );
        let node = block.layout(Constraints::tight(size), &mut LayoutCx::new());
        block.paint(&node, cx);
    }
}

/// The history picker's rows.
struct HistoryRows<'a> {
    panel: &'a Assistant,
    now_ms: u64,
    theme: &'a ReviewTheme,
}

impl Component for HistoryRows<'_> {
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            LayoutId::new(HISTORY_ID),
            constraints.constrain(LogicalSize::new(
                constraints.max_width(),
                constraints.max_height().unwrap_or(0),
            )),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let Some(picker) = self.panel.picker.as_ref() else {
            return;
        };
        let height = usize::try_from(layout.size.height).unwrap_or(0);
        let width = usize::try_from(layout.size.width).unwrap_or(0);
        let rows = picker.matching();
        // Keep the selection in view.
        let mut first = picker.first.get().min(rows.len().saturating_sub(1));
        if picker.selected < first {
            first = picker.selected;
        } else if height > 0 && picker.selected >= first + height {
            first = picker.selected + 1 - height;
        }
        picker.first.set(first);
        // The `✕` sits on the hovered row, else the selected one, when the row has room for it.
        let target = self
            .panel
            .hover
            .filter(|hover| *hover >= first && *hover < first + height && *hover < rows.len())
            .unwrap_or(picker.selected);
        let delete_column = width
            .checked_sub(DELETE_CELLS)
            .filter(|column| *column >= 4);
        self.panel.history_delete.set(
            delete_column
                .filter(|_| target < rows.len() && target >= first && target < first + height)
                .and_then(|column| Some((target, u16::try_from(column).ok()?))),
        );
        let lines: Vec<Line> =
            rows.iter()
                .enumerate()
                .skip(first)
                .take(height)
                .map(|(index, row)| {
                    let marked = picker.marked.as_deref() == Some(row.session_id.as_str());
                    let row_width = match delete_column {
                        Some(column) if index == target => column,
                        _ => width,
                    };
                    let state =
                        self.panel.agents.state(&row.session_id).filter(|_| {
                            self.panel.session.as_deref() == Some(row.session_id.as_str())
                        });
                    let line = super::picker::row_line(
                        row,
                        RowState {
                            active: self.panel.session.as_deref() == Some(row.session_id.as_str()),
                            working: state.is_some_and(|state| state.working),
                            needs_you: picker.needs_you(&row.session_id)
                                || state.is_some_and(crate::agent::AgentState::needs_you),
                            selected: index == picker.selected,
                            marked,
                        },
                        picker.turns.get(&row.session_id).copied(),
                        self.now_ms,
                        row_width,
                        self.theme,
                    );
                    if row_width == width {
                        return line;
                    }
                    let mut spans = line.spans.clone();
                    let pad = row_width.saturating_sub(line.width());
                    if pad > 0 {
                        spans.push(Span::raw(" ".repeat(pad)));
                    }
                    spans.push(Span::styled(
                        " ✕",
                        if marked {
                            self.theme
                                .error
                                .add_modifier(bmux_tui::style::Modifier::BOLD)
                        } else {
                            self.theme.muted
                        },
                    ));
                    Line::from_spans(spans)
                })
                .collect();
        let block = TextBlock::new(Text::from_lines(lines)).wrap(TextWrap::None);
        let size = Size::new(
            u16::try_from(layout.size.width).unwrap_or(u16::MAX),
            u16::try_from(layout.size.height).unwrap_or(u16::MAX),
        );
        let node = block.layout(Constraints::tight(size), &mut LayoutCx::new());
        block.paint(&node, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_the_first_line_bounded() {
        assert_eq!(
            conversation_title("\n  Why did it move?\nmore"),
            "Why did it move?"
        );
        let long = "word ".repeat(40);
        let title = conversation_title(&long);
        assert!(title.chars().count() <= MAX_TITLE_CHARS, "{title}");
        assert!(title.ends_with('…'));
    }
}
