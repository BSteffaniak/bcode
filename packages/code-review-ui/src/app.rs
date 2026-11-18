//! The code review app: one screen with internal routes.
//!
//! * **Routes.** [`Route::Home`] (the review home), [`Route::Wizard`] (new review),
//!   [`Route::Opening`] (resolving an entry or creating a workspace), and [`Route::Review`] (the
//!   review screen, [`ReviewSurface`]). Moving between them only swaps the route value: the
//!   runtime and the terminal stay as they are, and home keeps its state (filter, selection,
//!   groups) while a review is open.
//! * **Transitions.** Opening a review from home, a suggestion, the wizard, or a direct entry
//!   shows the review route; leaving a review (its own exit, after it saved) returns to home and
//!   reloads it; `q` on home closes the app. Entries that need answers first (pull requests,
//!   stacks, resume, creating a workspace) show the opening route with a spinner, or the failure
//!   with a way back.
//! * **Theme.** Home, the wizard, and the opening route derive the same [`ReviewTheme`] from the
//!   host theme as the review does.
//! * **Assistant.** The review assistant panel belongs to the app, not to a route
//!   ([`crate::assistant`]): it stays open with the same conversation while the user moves
//!   between home, the wizard and reviews. The app splits its area between the route and the
//!   panel, gives the route the remaining rectangle, paints the Ask AI button at the end of the
//!   route's header row, and attaches the conversation's session to the review on screen.

use super::appearance::{AppearanceOutcome, AppearanceOverlay};
use super::config::ReviewViewConfig;
use super::entry::{self, Entry};
use super::home::{Home, HomeOutcome};
use super::theme::ReviewTheme;
use super::wizard::{Wizard, WizardOutcome};
use super::{ReviewSurface, entry::transient_workspace};
use crate::action::Action;
use crate::action::EventResult;
use crate::assistant::context::ViewContext;
use crate::assistant::{Assistant, Place, Screen};
use crate::context::UiContext;
use crate::host::ClientToolReply;
use crate::services::ReviewServices;
use crate::view::View;
use bcode_code_review::document::DocumentConfig;
use bcode_code_review_models::{
    CreateReviewWorkspaceRequest, CreateReviewWorkspaceResponse, ReviewSource, ReviewSourceKind,
    ReviewWorkspace,
};
use bcode_tui_theme::components::ComponentStyles;
use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::empty_state::{EmptyStateComponent, EmptyStatePolicy};
use bmux_tui_components::spinner::Spinner;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The route on screen.
pub enum Route {
    /// The review home.
    Home,
    /// The new-review wizard.
    Wizard(Box<Wizard>),
    /// Resolving an entry or creating a workspace.
    Opening(Opening),
    /// The review screen.
    Review(Box<ReviewSurface>),
}

/// Name of a route, for tests and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteName {
    /// [`Route::Home`].
    Home,
    /// [`Route::Wizard`].
    NewReview,
    /// [`Route::Opening`].
    Opening,
    /// [`Route::Review`].
    Review,
}

/// The opening route: what is being resolved, and its failure once known.
#[derive(Debug, Clone)]
pub struct Opening {
    label: String,
    started: Instant,
    error: Option<String>,
    seq: u64,
}

/// Results delivered to the app.
#[derive(Debug)]
struct Resolved {
    seq: u64,
    result: Result<ReviewWorkspace, String>,
}

/// The code review app.
pub struct ReviewApp {
    services: ReviewServices,
    repo: PathBuf,
    config: ReviewViewConfig,
    route: Route,
    home: Option<Box<Home>>,
    theme_input: Option<ComponentStyles>,
    theme: ReviewTheme,
    themed: bool,
    resolved: Arc<Mutex<Vec<Resolved>>>,
    seq: u64,
    pending: Option<Entry>,
    scroll_motion: Option<Duration>,
    document_config: Option<DocumentConfig>,
    /// Reviews opened so far (route changes do not reload anything; tests count them).
    opened_reviews: usize,
    /// The route changed since the routes last read the keymap.
    keymap_stale: bool,
    /// The review whose sources the wizard edits (reopened when the edit is canceled).
    editing: Option<Box<ReviewWorkspace>>,
    /// The appearance picker, over every route while open.
    appearance: Option<AppearanceOverlay>,
    /// The review assistant panel.
    assistant: Assistant,
    /// Where the Ask AI button was painted in the last frame.
    button: Option<Rect>,
    /// Results of the assistant's app tools that ran on a task.
    tool_work: Arc<Mutex<Vec<ToolWork>>>,
    /// An `open_review` waiting for its review to load before moving to a place in it.
    awaiting: Option<Awaiting>,
    /// A `guide_show` call waiting for the guide it names to be read.
    awaiting_guide: Option<(tokio::sync::oneshot::Sender<ClientToolReply>, Instant)>,
    /// An assistant move waiting for the review on screen to close.
    leaving: Option<Leaving>,
    /// Whether the last frame showed the screen (the zoomed assistant covers it otherwise).
    screen_shown: bool,
}

/// An assistant tool's work finished on a task.
enum ToolWork {
    /// `open_review new_review` turned its sources into review sources.
    Prefilled {
        result: Result<Next, String>,
        reply: tokio::sync::oneshot::Sender<ClientToolReply>,
    },
    /// `open_review` read the review to open.
    Opened {
        result: Result<Box<ReviewWorkspace>, String>,
        target: crate::surface::GoTo,
        reply: tokio::sync::oneshot::Sender<ClientToolReply>,
    },
}

/// Where an assistant tool moves the user once the review on screen closed.
enum Next {
    /// The reviews home.
    Home,
    /// The new-review wizard, filled in with a title and sources when given.
    Wizard {
        title: Option<String>,
        sources: Vec<(ReviewSourceKind, Option<String>)>,
    },
    /// Another review, at a place in it.
    Review {
        workspace: Box<ReviewWorkspace>,
        target: crate::surface::GoTo,
    },
}

/// A move waiting for the review on screen to finish closing.
struct Leaving {
    next: Next,
    reply: tokio::sync::oneshot::Sender<ClientToolReply>,
}

/// A review opened by the assistant, with the place to move to once it loaded.
struct Awaiting {
    target: crate::surface::GoTo,
    reply: tokio::sync::oneshot::Sender<ClientToolReply>,
    since: Instant,
}

/// Error code of an app tool called by a session that is not the panel's active conversation.
pub const NOT_ACTIVE_CONVERSATION: &str = "not_active_conversation";

/// How long `open_review` waits for a review to load before answering without the place.
const OPEN_WAIT: Duration = Duration::from_secs(20);

impl std::fmt::Debug for ReviewApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewApp")
            .field("repo", &self.repo)
            .field("route", &self.route_name())
            .finish_non_exhaustive()
    }
}

impl ReviewApp {
    /// An app for the repository at `repo` that starts at `entry`.
    #[must_use]
    pub fn new(
        services: ReviewServices,
        repo: PathBuf,
        config: ReviewViewConfig,
        entry: Entry,
    ) -> Self {
        let icons = config.icons.unwrap_or_default();
        let assistant = Assistant::new(services.clone(), repo.clone());
        let mut app = Self {
            services,
            repo,
            config,
            route: Route::Home,
            home: None,
            theme_input: None,
            theme: ReviewTheme::derive(None, icons),
            themed: false,
            resolved: Arc::default(),
            seq: 0,
            pending: None,
            scroll_motion: None,
            document_config: None,
            opened_reviews: 0,
            keymap_stale: true,
            editing: None,
            appearance: None,
            assistant,
            button: None,
            tool_work: Arc::default(),
            awaiting: None,
            awaiting_guide: None,
            leaving: None,
            screen_shown: true,
        };
        match entry {
            Entry::Home => app.show_home(false),
            Entry::Workspace { workspace } => app.open_review(*workspace, None),
            Entry::Sources { title, sources } => {
                let workspace = transient_workspace(app.repo.clone(), title, sources);
                app.open_review(workspace, None);
            }
            other => {
                app.route = Route::Opening(Opening {
                    label: other.opening_label(),
                    started: Instant::now(),
                    error: None,
                    seq: 0,
                });
                app.pending = Some(other);
            }
        }
        app
    }

    /// Animate review wheel scrolling over `duration` (zero scrolls at once).
    #[must_use]
    pub fn with_scroll_motion(mut self, duration: Duration) -> Self {
        self.scroll_motion = Some(duration);
        self.route = match self.route {
            Route::Review(review) => Route::Review(Box::new(review.with_scroll_motion(duration))),
            other => other,
        };
        self
    }

    /// Use `config` for review documents.
    #[must_use]
    pub fn with_document_config(mut self, config: DocumentConfig) -> Self {
        self.document_config = Some(config);
        self.route = match self.route {
            Route::Review(review) => Route::Review(Box::new(review.with_document_config(config))),
            other => other,
        };
        self
    }

    /// The route on screen.
    #[must_use]
    pub const fn route_name(&self) -> RouteName {
        match self.route {
            Route::Home => RouteName::Home,
            Route::Wizard(_) => RouteName::NewReview,
            Route::Opening(_) => RouteName::Opening,
            Route::Review(_) => RouteName::Review,
        }
    }

    /// Home, once shown.
    #[must_use]
    pub fn home(&self) -> Option<&Home> {
        self.home.as_deref()
    }

    /// The wizard, while shown.
    #[must_use]
    pub fn wizard(&self) -> Option<&Wizard> {
        match &self.route {
            Route::Wizard(wizard) => Some(wizard),
            _ => None,
        }
    }

    /// The review, while shown.
    #[must_use]
    pub fn review(&self) -> Option<&ReviewSurface> {
        match &self.route {
            Route::Review(review) => Some(review),
            _ => None,
        }
    }

    /// The review, while shown, to change.
    #[cfg(test)]
    pub fn review_mut(&mut self) -> Option<&mut ReviewSurface> {
        match &mut self.route {
            Route::Review(review) => Some(review),
            _ => None,
        }
    }

    /// The failure of the opening route, while shown.
    #[must_use]
    pub fn opening_error(&self) -> Option<&str> {
        match &self.route {
            Route::Opening(opening) => opening.error.as_deref(),
            _ => None,
        }
    }

    /// Reviews opened so far.
    #[must_use]
    pub const fn opened_reviews(&self) -> usize {
        self.opened_reviews
    }

    /// The appearance picker, while open.
    #[must_use]
    pub const fn appearance_picker(&self) -> Option<&AppearanceOverlay> {
        self.appearance.as_ref()
    }

    /// The review assistant panel.
    #[must_use]
    pub const fn assistant(&self) -> &Assistant {
        &self.assistant
    }

    /// Where the Ask AI button was painted in the last frame.
    #[must_use]
    pub const fn assistant_button(&self) -> Option<Rect> {
        self.button
    }

    /// What is on screen, for the assistant; with `full` the review's binding template and
    /// the whole view context.
    fn place(&mut self, full: bool) -> Place {
        let mut place = match &mut self.route {
            Route::Home => Place::screen(Screen::Home),
            Route::Wizard(_) => Place::screen(Screen::Wizard),
            Route::Opening(_) => Place::screen(Screen::Opening),
            Route::Review(review) => Place {
                screen: Screen::Review,
                review: Some(review.assistant_review_place(full)),
                context: None,
            },
        };
        if full {
            place.context = Some(Box::new(self.view_context()));
        }
        place
    }

    /// What the user is looking at, read from the route on screen now.
    #[must_use]
    pub fn view_context(&self) -> ViewContext {
        let mut context = ViewContext {
            repo: self.repo.display().to_string(),
            as_of_ms: crate::assistant::panel::now_ms(),
            ..ViewContext::default()
        };
        match &self.route {
            Route::Home => {
                Screen::Home.name().clone_into(&mut context.screen);
                context.home = self.home.as_ref().map(|home| home.view_context());
            }
            Route::Wizard(wizard) => {
                Screen::Wizard.name().clone_into(&mut context.screen);
                context.wizard = Some(wizard.view_context());
            }
            Route::Opening(_) => Screen::Opening.name().clone_into(&mut context.screen),
            Route::Review(review) => {
                Screen::Review.name().clone_into(&mut context.screen);
                context.review = Some(review.view_context());
            }
        }
        context.screen_generation = context.generation();
        context
    }

    /// Whether the route asked to open, focus or close the assistant.
    fn take_assistant_request(&mut self) -> bool {
        match &mut self.route {
            Route::Home => self
                .home
                .as_mut()
                .is_some_and(|home| home.take_assistant_request()),
            Route::Wizard(wizard) => wizard.take_assistant_request(),
            Route::Review(review) => review.take_assistant_request(),
            Route::Opening(_) => false,
        }
    }

    /// Whether the route on screen shows its header row in an area `height` rows high.
    const fn route_shows_header(&self, height: u16) -> bool {
        match &self.route {
            Route::Home => Home::shows_header(height),
            Route::Wizard(_) => Wizard::shows_header(height),
            Route::Review(_) => ReviewSurface::shows_header(height),
            Route::Opening(_) => false,
        }
    }

    /// Leave `cells` free at the end of the route's header row.
    fn set_header_reserve(&mut self, cells: u16) {
        match &mut self.route {
            Route::Home => {
                if let Some(home) = self.home.as_mut() {
                    home.set_header_reserve(cells);
                }
            }
            Route::Wizard(wizard) => wizard.set_header_reserve(cells),
            Route::Review(review) => review.set_header_reserve(cells),
            Route::Opening(_) => {}
        }
    }

    /// Route an event to the assistant panel when it takes it.
    fn assistant_event(&mut self, event: &Event, host: &UiContext) -> Option<Action> {
        if let Event::Mouse(mouse) = event {
            if matches!(
                mouse.kind,
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left)
            ) && self
                .button
                .is_some_and(|area| area.contains(mouse.position))
            {
                self.assistant.toggle();
                return Some(Action::Redraw);
            }
            let down = matches!(mouse.kind, bmux_tui::event::MouseEventKind::Down(_));
            // The whole place (binding and view) only for a press in the panel.
            let place = self.place(down && self.assistant.contains(mouse.position));
            if let Some(outcome) = self.assistant.mouse(event, &place, host) {
                if outcome == crate::assistant::Outcome::GuideMouse {
                    if let Route::Review(review) = &mut self.route {
                        review.guide_tab_mouse(event, host);
                    }
                    return Some(Action::Redraw);
                }
                return Some(self.assistant_outcome(outcome, host));
            }
            // A click on the screen gives it the keyboard back.
            if down && self.assistant.is_focused() {
                self.assistant.unfocus();
            }
            return None;
        }
        if !self.assistant.is_focused() {
            return None;
        }
        match event {
            Event::Key(stroke) => {
                let place = self.place(stroke.key == KeyCode::Enter);
                let outcome = self.assistant.key(*stroke, &place, host);
                if let crate::assistant::Outcome::GuideKey(stroke) = outcome {
                    return Some(match &mut self.route {
                        Route::Review(review) => review
                            .guide_tab_key(stroke, host)
                            .map_or(Action::None, |action| self.review_action(action, host)),
                        _ => Action::None,
                    });
                }
                Some(self.assistant_outcome(outcome, host))
            }
            Event::Paste(text) => {
                self.assistant.paste(text);
                Some(Action::Redraw)
            }
            _ => None,
        }
    }

    /// Collect the panel's work, attach its session to the place on screen, and let the
    /// review reflect what the session's review tools changed.
    fn poll_assistant(&mut self, host: &UiContext) -> bool {
        let mut changed = self.assistant.poll(host);
        let key = self.place(false).key();
        if self.assistant.needs_attach(&key) {
            let place = self.place(true);
            self.assistant.attach(&place, host);
            changed = true;
        }
        if self.assistant.wants_resend() {
            let place = self.place(true);
            self.assistant.resend(&place, host);
            changed = true;
        }
        for call in self.assistant.take_tool_calls() {
            self.answer_tool(call, host);
            changed = true;
        }
        changed |= self.finish_tool_work(host);
        for session in self.assistant.take_stopped() {
            if let Route::Review(review) = &mut self.route {
                review.guide_turn_stopped(&session, host);
            }
            changed = true;
        }
        let tools = self.assistant.take_finished_tools();
        if tools.is_empty() {
            return changed;
        }
        if tools
            .iter()
            .any(|(name, _)| crate::surface::REVIEW_LIST_TOOLS.contains(&name.as_str()))
            && let Some(home) = self.home.as_mut()
        {
            home.refresh();
        }
        if let Route::Review(review) = &mut self.route {
            review.apply_assistant_tools(&tools, host);
        }
        true
    }

    /// Paint the route in `screen` with the Ask AI button at the end of its header row.
    fn render_screen(
        &mut self,
        screen: Rect,
        panel_shown: bool,
        frame: &mut PaintCx<'_, '_>,
        styles: Option<&ComponentStyles>,
        host: &UiContext,
    ) {
        let plan = self
            .route_shows_header(screen.height)
            .then(|| {
                crate::assistant::button::plan(
                    screen.width,
                    self.assistant.is_open(),
                    &self.assistant.badge(panel_shown),
                    &self.theme,
                )
            })
            .flatten();
        self.set_header_reserve(plan.as_ref().map_or(0, |plan| plan.width));
        if let Route::Review(review) = &mut self.route {
            review.render(screen, frame, styles, host);
        } else {
            self.render_route(screen, frame);
        }
        self.button = plan.map(|plan| {
            plan.paint(
                Rect::new(screen.x, screen.y, screen.width, 1),
                &self.theme,
                frame,
            )
        });
    }

    /// Open the appearance picker when a route asked for it.
    fn open_requested_appearance(&mut self, host: &UiContext) -> bool {
        let Some(control) = host.appearance() else {
            return false;
        };
        if !control.take_open_request() || self.appearance.is_some() {
            return false;
        }
        self.appearance = Some(AppearanceOverlay::open(control, &self.theme));
        true
    }

    /// Route `event` to the open appearance picker, which captures all input.
    fn appearance_event(&mut self, event: &Event, host: &UiContext) -> Option<EventResult> {
        let overlay = self.appearance.as_mut()?;
        let Some(control) = host.appearance() else {
            self.appearance = None;
            return Some(EventResult::from_action(Action::Redraw));
        };
        let outcome = match event {
            Event::Key(stroke) => overlay.key(*stroke, control, &self.theme),
            Event::Mouse(_) => overlay.mouse(event, control, &self.theme),
            Event::Resize(_) => AppearanceOutcome::Redraw,
            _ => AppearanceOutcome::Ignored,
        };
        let action = match outcome {
            AppearanceOutcome::Close => {
                self.appearance = None;
                Action::Redraw
            }
            AppearanceOutcome::Redraw => Action::Redraw,
            AppearanceOutcome::Ignored => Action::None,
        };
        Some(EventResult::from_action(action))
    }

    // ----------------------------------------------------------------------------------------
    // Transitions
    // ----------------------------------------------------------------------------------------

    /// Let the routes read the keymap after a route change (hosts need not call
    /// `prepare_keymap` again on their own).
    fn refresh_keymap(&mut self, host: &UiContext) {
        if std::mem::take(&mut self.keymap_stale) {
            self.prepare_keymap(host);
        }
    }

    fn show_home(&mut self, refresh: bool) {
        let home = self
            .home
            .get_or_insert_with(|| Box::new(Home::new(self.services.clone(), self.repo.clone())));
        if refresh {
            home.refresh();
        }
        self.route = Route::Home;
        self.keymap_stale = true;
    }

    fn open_review(&mut self, workspace: ReviewWorkspace, host: Option<&UiContext>) {
        let mut review = ReviewSurface::new(
            self.services.clone(),
            self.repo.clone(),
            workspace.sources.clone(),
            workspace.title.clone(),
            self.config,
        )
        .with_workspace(workspace);
        if let Some(duration) = self.scroll_motion {
            review = review.with_scroll_motion(duration);
        }
        if let Some(config) = self.document_config {
            review = review.with_document_config(config);
        }
        if let Some(host) = host {
            review.prepare_keymap(host);
        }
        self.opened_reviews += 1;
        self.route = Route::Review(Box::new(review));
        self.keymap_stale = true;
    }

    /// Start resolving `entry` on a task (the opening route shows meanwhile).
    fn start(&mut self, entry: Entry, host: &UiContext) {
        match entry {
            Entry::Home => self.show_home(true),
            Entry::Workspace { workspace } => self.open_review(*workspace, Some(host)),
            Entry::Sources { title, sources } => {
                let workspace = transient_workspace(self.repo.clone(), title, sources);
                self.open_review(workspace, Some(host));
            }
            other => {
                let label = other.opening_label();
                let (services, repo) = (self.services.clone(), self.repo.clone());
                self.run_opening(label, host, async move {
                    entry::resolve(services, repo, other).await
                });
            }
        }
    }

    fn run_opening(
        &mut self,
        label: String,
        host: &UiContext,
        task: impl std::future::Future<Output = Result<ReviewWorkspace, String>> + Send + 'static,
    ) {
        self.seq += 1;
        let seq = self.seq;
        self.route = Route::Opening(Opening {
            label,
            started: Instant::now(),
            error: None,
            seq,
        });
        let resolved = Arc::clone(&self.resolved);
        host.spawn(Box::pin(async move {
            let result = task.await;
            if let Ok(mut queue) = resolved.lock() {
                queue.push(Resolved { seq, result });
            }
        }));
    }

    /// Create a durable workspace of `sources` and open it.
    fn create(
        &mut self,
        title: String,
        sources: Vec<ReviewSourceKind>,
        show_ignored: Option<bool>,
        whole_stack: bool,
        host: &UiContext,
    ) {
        let draft = transient_workspace(self.repo.clone(), Some(title), sources);
        let (services, repo) = (self.services.clone(), self.repo.clone());
        let label = format!("Creating “{}”…", draft.title);
        let presentation = (show_ignored.is_some() || whole_stack).then(|| {
            let mut state = bcode_code_review_models::ReviewPresentationState {
                show_ignored,
                ..bcode_code_review_models::ReviewPresentationState::default()
            };
            if whole_stack {
                state.set_stack_view(bcode_code_review_models::ReviewStackView::Whole);
            }
            state
        });
        self.run_opening(label, host, async move {
            create_workspace(services, repo, draft.title, draft.sources, presentation).await
        });
    }

    fn home_outcome(&mut self, outcome: HomeOutcome, host: &UiContext) -> Action {
        let outcome_files = outcome == HomeOutcome::NewFiles;
        match outcome {
            HomeOutcome::Ignored => Action::None,
            HomeOutcome::Redraw => Action::Redraw,
            HomeOutcome::Exit => Action::CLOSE,
            HomeOutcome::Open(workspace) => {
                self.open_review(*workspace, Some(host));
                Action::Redraw
            }
            HomeOutcome::Start(entry) => {
                self.start(entry, host);
                Action::Redraw
            }
            HomeOutcome::New | HomeOutcome::NewFiles => {
                let mut wizard = if outcome_files {
                    Wizard::files(self.services.clone(), self.repo.clone())
                } else {
                    Wizard::new(self.services.clone(), self.repo.clone())
                };
                wizard.prepare_keymap(host);
                wizard.set_show_ignored(self.config.files.show_ignored);
                self.route = Route::Wizard(Box::new(wizard));
                self.keymap_stale = true;
                Action::Redraw
            }
        }
    }

    fn wizard_outcome(&mut self, outcome: WizardOutcome, host: &UiContext) -> Action {
        match outcome {
            WizardOutcome::Ignored => Action::None,
            WizardOutcome::Redraw => Action::Redraw,
            WizardOutcome::Cancel => {
                // Leaving an edit returns to the review unchanged.
                match self.editing.take() {
                    Some(workspace) => self.open_review(*workspace, Some(host)),
                    None => self.show_home(false),
                }
                Action::Redraw
            }
            WizardOutcome::Update { workspace } => {
                let base = self.editing.take().map(|base| *base);
                self.update_sources(*workspace, base, host);
                Action::Redraw
            }
            WizardOutcome::Create {
                title,
                sources,
                show_ignored,
                whole_stack,
            } => {
                self.create(title, sources, show_ignored, whole_stack, host);
                Action::Redraw
            }
        }
    }

    /// A review's action, with its close turned into the way back to home (or into the
    /// source editor when the review asked for it).
    fn review_action(&mut self, action: Action, host: &UiContext) -> Action {
        let Action::Close { edit_sources } = action else {
            return action;
        };
        if let Some(leaving) = self.leaving.take() {
            self.go(leaving.next, leaving.reply, host);
            return Action::Redraw;
        }
        if edit_sources && let Route::Review(review) = &self.route {
            let workspace = review.edit_target();
            let mut wizard =
                Wizard::editing(self.services.clone(), self.repo.clone(), workspace.clone());
            wizard.prepare_keymap(host);
            wizard.set_show_ignored(self.config.files.show_ignored);
            self.editing = Some(Box::new(workspace));
            self.route = Route::Wizard(Box::new(wizard));
            self.keymap_stale = true;
            return Action::Redraw;
        }
        let deleted = match &self.route {
            Route::Review(review) if review.deleted() => Some(review.title().to_owned()),
            _ => None,
        };
        self.show_home(true);
        if let (Some(title), Some(home)) = (deleted, self.home.as_mut()) {
            home.notify_deleted(&title);
        }
        Action::Redraw
    }

    /// Save edited sources (durable reviews; what changed from `base`, the review as it was
    /// opened for editing) and open the review with them.
    fn update_sources(
        &mut self,
        workspace: ReviewWorkspace,
        base: Option<ReviewWorkspace>,
        host: &UiContext,
    ) {
        if workspace.id == entry::TRANSIENT_WORKSPACE_ID {
            self.open_review(workspace, Some(host));
            return;
        }
        let services = self.services.clone();
        let label = format!("Saving “{}”…", workspace.title);
        self.run_opening(label, host, async move {
            let repo_path = workspace.repo_root.clone();
            let response: bcode_code_review_models::UpdateReviewWorkspaceResponse = services
                .call(bcode_code_review_models::UpdateReviewWorkspaceRequest {
                    repo_path,
                    workspace,
                    base,
                })
                .await
                .map_err(|error| error.message)?;
            Ok(response.workspace)
        });
    }

    /// Route one event to the route on screen.
    fn route_event(&mut self, event: &Event, host: &UiContext) -> EventResult {
        let theme = self.theme;
        let action = match &mut self.route {
            Route::Review(review) => {
                let result = review.handle_event(event, host);
                if matches!(result.action, Action::Close { .. }) {
                    return EventResult::from_action(self.review_action(result.action, host));
                }
                if let Some(entry) = review.take_start_request() {
                    self.start(entry, host);
                    return EventResult::from_action(Action::Redraw);
                }
                return result;
            }
            Route::Wizard(wizard) => {
                let outcome = match event {
                    Event::Key(stroke) => wizard.key(*stroke, host, &theme),
                    Event::Paste(text) => wizard.paste(text, host, &theme),
                    Event::Mouse(_) => wizard.mouse(event, host, &theme),
                    Event::Resize(_) => WizardOutcome::Redraw,
                    _ => WizardOutcome::Ignored,
                };
                self.wizard_outcome(outcome, host)
            }
            Route::Opening(opening) => match event {
                // `esc` abandons a running resolution (its answer is ignored); after a failure,
                // `enter` and `q` also go home.
                Event::Key(stroke)
                    if stroke.key == KeyCode::Escape
                        || (opening.error.is_some()
                            && matches!(stroke.key, KeyCode::Enter | KeyCode::Char('q'))) =>
                {
                    self.pending = None;
                    self.show_home(true);
                    Action::Redraw
                }
                Event::Resize(_) => Action::Redraw,
                _ => Action::None,
            },
            Route::Home => {
                let Some(home) = self.home.as_mut() else {
                    return EventResult::from_action(Action::None);
                };
                let outcome = match event {
                    Event::Key(stroke) => home.key(*stroke, host, &theme),
                    Event::Paste(text) => home.paste(text, &theme),
                    Event::Mouse(_) => home.mouse(event, &theme),
                    Event::Resize(_) => HomeOutcome::Redraw,
                    Event::Focus(FocusEvent::Gained) => {
                        home.refresh();
                        HomeOutcome::Ignored
                    }
                    _ => HomeOutcome::Ignored,
                };
                self.home_outcome(outcome, host)
            }
        };
        EventResult::from_action(action)
    }

    /// Poll the route on screen (and resolve entries).
    fn poll_route(&mut self, host: &UiContext) -> Action {
        if let Some(entry) = self.pending.take() {
            self.start(entry, host);
            return Action::Redraw;
        }
        let resolved: Vec<Resolved> = self
            .resolved
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default();
        let mut changed = false;
        for Resolved { seq, result } in resolved {
            let Route::Opening(opening) = &mut self.route else {
                continue;
            };
            if opening.seq != seq {
                continue;
            }
            changed = true;
            match result {
                Ok(workspace) => self.open_review(workspace, Some(host)),
                Err(error) => opening.error = Some(error),
            }
        }
        let theme = self.theme;
        let action = match &mut self.route {
            Route::Review(review) => {
                let action = review.poll(host);
                self.review_action(action, host)
            }
            Route::Home => {
                let changed_home = self.home.as_mut().is_some_and(|home| home.poll(host));
                if changed_home {
                    Action::Redraw
                } else {
                    Action::None
                }
            }
            Route::Wizard(wizard) => {
                if wizard.poll(host, &theme) {
                    Action::Redraw
                } else {
                    Action::None
                }
            }
            // The spinner animates while resolving.
            Route::Opening(opening) if opening.error.is_none() => Action::Redraw,
            Route::Opening(_) => Action::None,
        };
        if changed && action == Action::None {
            Action::Redraw
        } else {
            action
        }
    }

    fn apply_theme(&mut self, theme: Option<&ComponentStyles>) {
        if self.themed && self.theme_input.as_ref() == theme {
            return;
        }
        self.themed = true;
        self.theme_input = theme.copied();
        self.theme = self.config.theme(theme);
    }

    fn render_route(&mut self, area: Rect, cx: &mut PaintCx<'_, '_>) {
        let theme = self.theme;
        match &mut self.route {
            Route::Home => {
                if let Some(home) = self.home.as_mut() {
                    home.render(area, cx, &theme);
                }
            }
            Route::Wizard(wizard) => wizard.render(area, cx, &theme),
            Route::Opening(opening) => paint_opening(opening, &theme, area, cx),
            Route::Review(review) => {
                review.render_with_theme(area, cx, self.theme_input.as_ref());
            }
        }
    }
}

/// An app tool's arguments with blank optional values taken as not given (see
/// [`bcode_tool_models::arguments::without_blank_values`]). Empty arguments are an empty object;
/// arguments that are not a JSON object are returned unchanged, to be refused by the tool's own
/// parsing.
pub(crate) fn without_blank_arguments(arguments: &str) -> String {
    if arguments.trim().is_empty() {
        return "{}".to_owned();
    }
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(value @ serde_json::Value::Object(_)) => {
            bcode_tool_models::arguments::without_blank_values(value, &[]).to_string()
        }
        _ => arguments.to_owned(),
    }
}

/// Tool arguments as `T`.
fn parse<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|error| format!("invalid arguments: {error}"))
}

impl ReviewApp {
    /// What is on screen after an app tool did `done`, as its answer.
    fn screen_answer(&self, done: &str) -> ClientToolReply {
        let screen = self
            .view_context()
            .tool_answer(r#"{"sections":["screen","review","stack","comparison","file","home"]}"#)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .unwrap_or_default();
        ClientToolReply::Completed(serde_json::json!({"done": done, "screen": screen}).to_string())
    }

    fn refused(code: &str, message: impl Into<String>) -> ClientToolReply {
        ClientToolReply::Failed {
            code: code.to_owned(),
            message: message.into(),
        }
    }

    /// Answer one of the app's tools.
    fn answer_tool(&mut self, call: crate::host::ClientToolCall, host: &UiContext) {
        use crate::assistant::actions;
        let crate::host::ClientToolCall {
            session_id,
            tool,
            arguments,
            reply,
        } = call;
        // Only the conversation open in the panel sees and moves the screen; a session that
        // still holds the tools from before (a conversation switched away from, a turn still
        // running) is refused.
        if self.assistant.session() != Some(session_id.as_str()) {
            let _ = reply.send(Self::refused(
                NOT_ACTIVE_CONVERSATION,
                "this conversation is not the one open in the review app's assistant panel, so \
                 it cannot read or change the screen; the user can switch to it from the \
                 panel's history",
            ));
            return;
        }
        let arguments = without_blank_arguments(&arguments);
        let answer = match tool.as_str() {
            crate::assistant::profile::CURRENT_VIEW => {
                match self.view_context().tool_answer(&arguments) {
                    Ok(answer) => ClientToolReply::Completed(answer),
                    Err(message) => Self::refused("invalid_arguments", message),
                }
            }
            actions::FIND_PULL_REQUESTS => {
                let (services, repo) = (self.services.clone(), self.repo.clone());
                host.spawn(Box::pin(async move {
                    let answer = match actions::find(services, repo, &arguments).await {
                        Ok(found) => ClientToolReply::Completed(found),
                        Err(message) => Self::refused("find_failed", message),
                    };
                    let _ = reply.send(answer);
                }));
                return;
            }
            actions::OPEN_REVIEW => {
                self.open_for_tool(&arguments, reply, host);
                return;
            }
            actions::GUIDE_SHOW => {
                self.guide_show_for_tool(&arguments, reply, host);
                return;
            }
            actions::REFRESH => match &mut self.route {
                Route::Home => {
                    if let Some(home) = self.home.as_mut() {
                        home.refresh();
                    }
                    self.screen_answer("reloading the reviews")
                }
                Route::Review(review) => {
                    match review.assistant_command(crate::surface::ReviewCommand::Refresh, host) {
                        Ok(done) => self.screen_answer(&done),
                        Err(message) => Self::refused("not_ready", message),
                    }
                }
                _ => Self::refused("not_available", "nothing to refresh on this screen"),
            },
            actions::GO_TO
            | actions::SET_COMPARISON
            | actions::SET_VIEW
            | actions::COMPLETE_PASS
            | actions::PULL_REQUEST_REVIEW
            | actions::PUBLISH => self.review_tool(&tool, &arguments, host),
            other => Self::refused(
                "unknown_tool",
                format!("the review app has no tool {other}"),
            ),
        };
        let _ = reply.send(answer);
    }

    /// Answer a tool that acts on the review on screen.
    fn review_tool(&mut self, tool: &str, arguments: &str, host: &UiContext) -> ClientToolReply {
        use crate::assistant::actions;
        let Route::Review(review) = &mut self.route else {
            return Self::refused(
                "no_review",
                "no review is on screen; open one with bcode.code-review.open_review",
            );
        };
        let arguments = if arguments.trim().is_empty() {
            "{}"
        } else {
            arguments
        };
        let result = match tool {
            actions::GO_TO => {
                parse(arguments).and_then(|target| review.assistant_go_to(&target, host))
            }
            actions::SET_COMPARISON => {
                parse(arguments).and_then(|change| review.assistant_set_comparison(&change, host))
            }
            actions::SET_VIEW => {
                parse(arguments).and_then(|change| review.assistant_set_view(&change, host))
            }
            actions::COMPLETE_PASS => {
                review.assistant_command(crate::surface::ReviewCommand::CompletePass, host)
            }
            actions::PUBLISH => {
                review.assistant_command(crate::surface::ReviewCommand::Publish, host)
            }
            _ => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Verdict {
                    action: String,
                }
                parse::<Verdict>(arguments).and_then(|verdict| {
                    let command = match verdict.action.as_str() {
                        "approve" => crate::surface::ReviewCommand::Approve,
                        "request_changes" => crate::surface::ReviewCommand::RequestChanges,
                        "comment" => crate::surface::ReviewCommand::Comment,
                        "approve_viewed_layers" => {
                            crate::surface::ReviewCommand::ApproveViewedLayers
                        }
                        other => return Err(format!("unknown action {other:?}")),
                    };
                    review.assistant_command(command, host)
                })
            }
        };
        match result {
            Ok(done) => self.screen_answer(&done),
            Err(message) => Self::refused("refused", message),
        }
    }

    /// Run `guide_show`: answered at once, or once the guide it names was read (a guide just
    /// started may not be yet), saying when the review has no such guide.
    fn guide_show_for_tool(
        &mut self,
        arguments: &str,
        reply: tokio::sync::oneshot::Sender<ClientToolReply>,
        host: &UiContext,
    ) {
        let Route::Review(review) = &mut self.route else {
            let _ = reply.send(Self::refused(
                "no_review",
                "no review is on screen; open one with bcode.code-review.open_review",
            ));
            return;
        };
        let arguments = if arguments.trim().is_empty() {
            "{}"
        } else {
            arguments
        };
        let outcome = parse::<crate::assistant::actions::GuideShowArguments>(arguments)
            .and_then(|show| review.assistant_guide_show(&show, host));
        match outcome {
            Ok(crate::surface::GuideShowOutcome::Done(done)) => {
                let _ = reply.send(self.screen_answer(&done));
            }
            Ok(crate::surface::GuideShowOutcome::Waiting) => {
                if let Some((earlier, _)) = self.awaiting_guide.replace((reply, Instant::now())) {
                    let _ = earlier.send(Self::refused(
                        "replaced",
                        "a later guide_show replaced this one before its guide was read",
                    ));
                }
            }
            Err(message) => {
                let _ = reply.send(Self::refused("refused", message));
            }
        }
    }

    /// Answer a waiting `guide_show` once the review has its answer, or once it waited too long
    /// or the review was left. Returns whether it answered.
    fn finish_guide_show(&mut self) -> bool {
        let Some((reply, since)) = self.awaiting_guide.take() else {
            return false;
        };
        let answer = match &mut self.route {
            Route::Review(review) => review.take_guide_show_answer(),
            _ => Some(Err(
                "the review was left before its guide was read".to_owned()
            )),
        };
        let answer = match answer {
            Some(answer) => answer,
            None if since.elapsed() > OPEN_WAIT => {
                Err("the guide was not read in time; try again".to_owned())
            }
            None => {
                self.awaiting_guide = Some((reply, since));
                return false;
            }
        };
        let answer = match answer {
            Ok(done) => self.screen_answer(&done),
            Err(message) => Self::refused("refused", message),
        };
        let _ = reply.send(answer);
        true
    }

    /// Run `open_review`: answered at once, or once the review was read (and loaded, for a
    /// place in it).
    fn open_for_tool(
        &mut self,
        arguments: &str,
        reply: tokio::sync::oneshot::Sender<ClientToolReply>,
        host: &UiContext,
    ) {
        #[derive(serde::Deserialize, Default)]
        #[serde(default)]
        struct Open {
            screen: Option<String>,
            review: Option<String>,
            #[serde(flatten)]
            rest: serde_json::Map<String, serde_json::Value>,
        }
        let open: Open = match serde_json::from_str(if arguments.trim().is_empty() {
            "{}"
        } else {
            arguments
        }) {
            Ok(open) => open,
            Err(error) => {
                let _ = reply.send(Self::refused("invalid_arguments", error.to_string()));
                return;
            }
        };
        match open.screen.as_deref().unwrap_or("review") {
            "home" => self.leave_then(Next::Home, reply, host),
            "new_review" => self.prefill_for_tool(open.rest, reply, host),
            "review" => {
                let Some(id) = open.review.filter(|id| !id.trim().is_empty()) else {
                    let _ = reply.send(Self::refused(
                        "invalid_arguments",
                        "pass `review` with a saved review's id (code_review.list_reviews)",
                    ));
                    return;
                };
                let mut place = open.rest;
                place.insert("review".to_owned(), serde_json::Value::String(id.clone()));
                let target: crate::surface::GoTo =
                    match serde_json::from_value(serde_json::Value::Object(place)) {
                        Ok(target) => target,
                        Err(error) => {
                            let _ =
                                reply.send(Self::refused("invalid_arguments", error.to_string()));
                            return;
                        }
                    };
                let (services, repo) = (self.services.clone(), self.repo.clone());
                let work = Arc::clone(&self.tool_work);
                host.spawn(Box::pin(async move {
                    let response: Result<bcode_code_review_models::GetReviewWorkspaceResponse, _> =
                        services
                            .call(bcode_code_review_models::GetReviewWorkspaceRequest {
                                repo_path: repo.clone(),
                                workspace_id: id.clone(),
                            })
                            .await;
                    let result = match response {
                        Ok(response) => response.workspace.map(Box::new).ok_or_else(|| {
                            format!(
                                "no saved review {id} in {} (code_review.list_reviews lists them; a review of another repository opens with `bcode review` there)",
                                repo.display()
                            )
                        }),
                        Err(error) => Err(error.message),
                    };
                    if let Ok(mut queue) = work.lock() {
                        queue.push(ToolWork::Opened {
                            result,
                            target,
                            reply,
                        });
                    }
                }));
            }
            other => {
                let _ = reply.send(Self::refused(
                    "invalid_arguments",
                    format!("unknown screen {other:?} (review, home or new_review)"),
                ));
            }
        }
    }

    /// Run `open_review new_review`: the wizard, filled in with `title` and `sources` (as
    /// `code_review.create_review` takes them) when given.
    fn prefill_for_tool(
        &mut self,
        rest: serde_json::Map<String, serde_json::Value>,
        reply: tokio::sync::oneshot::Sender<ClientToolReply>,
        host: &UiContext,
    ) {
        #[derive(serde::Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Prefill {
            title: Option<String>,
            sources: Vec<bcode_code_review::assistant::create::SourceArgument>,
        }
        let prefill: Prefill = match serde_json::from_value(serde_json::Value::Object(rest)) {
            Ok(prefill) => prefill,
            Err(error) => {
                let _ = reply.send(Self::refused(
                    "invalid_arguments",
                    format!("new_review takes `title` and `sources`: {error}"),
                ));
                return;
            }
        };
        if prefill.sources.is_empty() {
            self.leave_then(
                Next::Wizard {
                    title: prefill.title,
                    sources: Vec::new(),
                },
                reply,
                host,
            );
            return;
        }
        let repo = self.repo.clone();
        let work = Arc::clone(&self.tool_work);
        host.spawn(Box::pin(async move {
            // Naming sources reads git (the origin remote, the repository root).
            let planned = tokio::task::spawn_blocking(move || {
                bcode_code_review::assistant::create::plan(&repo, None, prefill.sources)
                    .map_err(|error| error.to_string())
                    .and_then(|plan| {
                        let same = plan.repo_root.canonicalize().ok() == repo.canonicalize().ok();
                        if same {
                            Ok(plan.sources)
                        } else {
                            Err(format!(
                                "those sources are in {}, not this app's repository",
                                plan.repo_root.display()
                            ))
                        }
                    })
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|planned| planned);
            let result = planned.map(|sources| Next::Wizard {
                title: prefill.title,
                sources,
            });
            if let Ok(mut queue) = work.lock() {
                queue.push(ToolWork::Prefilled { result, reply });
            }
        }));
    }

    /// Move the user to `next` for an assistant tool, leaving the review on screen through its
    /// close path first (saves finish; an unsaved review with annotations is not left).
    fn leave_then(
        &mut self,
        next: Next,
        reply: tokio::sync::oneshot::Sender<ClientToolReply>,
        host: &UiContext,
    ) {
        let Route::Review(review) = &mut self.route else {
            self.go(next, reply, host);
            return;
        };
        match review.close_for_assistant(host) {
            Err(message) => {
                let _ = reply.send(Self::refused("unsaved_review", message));
            }
            Ok(action) => {
                if let Some(earlier) = self.leaving.replace(Leaving { next, reply }) {
                    let _ = earlier.reply.send(Self::refused(
                        "replaced",
                        "another move replaced this one before the review closed",
                    ));
                }
                if matches!(action, Action::Close { .. }) {
                    let _ = self.review_action(action, host);
                }
            }
        }
    }

    /// Show `next` (the review on screen has closed, or there was none) and answer.
    fn go(
        &mut self,
        next: Next,
        reply: tokio::sync::oneshot::Sender<ClientToolReply>,
        host: &UiContext,
    ) {
        match next {
            Next::Home => {
                self.show_home(true);
                let _ = reply.send(self.screen_answer("showing the reviews home"));
            }
            Next::Wizard { title, sources } => {
                let filled = !sources.is_empty();
                let mut wizard = if filled {
                    Wizard::prefilled(self.services.clone(), self.repo.clone(), title, sources)
                } else {
                    Wizard::new(self.services.clone(), self.repo.clone())
                };
                wizard.prepare_keymap(host);
                wizard.set_show_ignored(self.config.files.show_ignored);
                self.route = Route::Wizard(Box::new(wizard));
                self.keymap_stale = true;
                let _ = reply.send(self.screen_answer(if filled {
                    "showing the new-review wizard filled in, for the user to confirm"
                } else {
                    "showing the new-review wizard"
                }));
            }
            Next::Review { workspace, target } => {
                self.open_review(*workspace, Some(host));
                self.await_place(target, reply);
            }
        }
    }

    /// Answer an `open_review` of the review now on screen: at once, or once it loaded when the
    /// call named a place in it.
    fn await_place(
        &mut self,
        target: crate::surface::GoTo,
        reply: tokio::sync::oneshot::Sender<ClientToolReply>,
    ) {
        if !target.names_a_place() {
            let _ = reply.send(self.screen_answer("opened the review"));
            return;
        }
        if let Some(earlier) = self.awaiting.take() {
            let _ = earlier
                .reply
                .send(self.screen_answer("opened; another move replaced this one"));
        }
        self.awaiting = Some(Awaiting {
            target,
            reply,
            since: Instant::now(),
        });
    }

    /// Finish app tool work that ran on a task, and moves waiting for a review to load.
    fn finish_tool_work(&mut self, host: &UiContext) -> bool {
        let work: Vec<ToolWork> = self
            .tool_work
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default();
        let mut changed = self.finish_guide_show();
        for item in work {
            changed = true;
            let (result, target, reply) = match item {
                ToolWork::Prefilled { result, reply } => {
                    match result {
                        Ok(next) => self.leave_then(next, reply, host),
                        Err(message) => {
                            let _ = reply.send(Self::refused("invalid_arguments", message));
                        }
                    }
                    continue;
                }
                ToolWork::Opened {
                    result,
                    target,
                    reply,
                } => (result, target, reply),
            };
            match result {
                Err(message) => {
                    let _ = reply.send(Self::refused("not_found", message));
                }
                Ok(workspace) => {
                    let same = self
                        .review()
                        .and_then(ReviewSurface::workspace)
                        .is_some_and(|open| open.id == workspace.id);
                    if same {
                        self.await_place(target, reply);
                    } else {
                        self.leave_then(Next::Review { workspace, target }, reply, host);
                    }
                }
            }
        }
        let loaded = self
            .review()
            .is_some_and(|review| review.document().is_some());
        if let Some(awaiting) = self.awaiting.take() {
            if loaded {
                let result = match &mut self.route {
                    Route::Review(review) => review.assistant_go_to(&awaiting.target, host),
                    _ => Err("the review was left before it loaded".to_owned()),
                };
                let answer = match result {
                    Ok(done) => self.screen_answer(&format!("opened the review; {done}")),
                    Err(message) => self.screen_answer(&format!("opened the review; {message}")),
                };
                let _ = awaiting.reply.send(answer);
                changed = true;
            } else if awaiting.since.elapsed() > OPEN_WAIT
                || !matches!(self.route, Route::Review(_) | Route::Opening(_))
            {
                let _ = awaiting
                    .reply
                    .send(self.screen_answer("opened the review; it is still loading"));
            } else {
                self.awaiting = Some(awaiting);
            }
        }
        changed
    }
}

impl ReviewApp {
    /// The run loop's action for what the assistant panel did; a Draft comment drafts on the
    /// review on screen.
    fn assistant_outcome(
        &mut self,
        outcome: crate::assistant::Outcome,
        host: &UiContext,
    ) -> Action {
        match outcome {
            crate::assistant::Outcome::Ignored
            | crate::assistant::Outcome::GuideKey(_)
            | crate::assistant::Outcome::GuideMouse => Action::None,
            crate::assistant::Outcome::GuideSection(entry) => {
                if let Route::Review(review) = &mut self.route {
                    review.guide_card_show(entry, host);
                }
                Action::Redraw
            }
            crate::assistant::Outcome::GuideOpen => {
                let docked = self.screen_shown;
                match &mut self.route {
                    Route::Review(review) if docked && review.guide_docks() => {
                        review.open_guide_panel();
                        self.assistant.unfocus();
                    }
                    Route::Review(_) => self.assistant.show_guide_tab(),
                    _ => {}
                }
                Action::Redraw
            }
            crate::assistant::Outcome::Redraw => Action::Redraw,
            crate::assistant::Outcome::OpenSession(session_id) => {
                Action::OpenSession { session_id }
            }
            crate::assistant::Outcome::DraftComment(body) => {
                let result = match &mut self.route {
                    Route::Review(review) => review.draft_comment_at_cursor(body),
                    _ => Err("open a review to draft a comment".to_owned()),
                };
                match result {
                    Ok(done) => self.assistant.set_notice(done, false),
                    Err(error) => self.assistant.set_notice(error, true),
                }
                Action::Redraw
            }
        }
    }

    /// What the route asked of the assistant since the last call.
    fn take_assistant_requests(&mut self) -> Vec<crate::assistant::LinkRequest> {
        match &mut self.route {
            Route::Home => self
                .home
                .as_mut()
                .map(|home| home.take_assistant_requests())
                .unwrap_or_default(),
            Route::Wizard(wizard) => wizard.take_assistant_requests(),
            Route::Review(review) => review.take_assistant_requests(),
            Route::Opening(_) => Vec::new(),
        }
    }

    /// Run what the route asked of the assistant: a panel action from its palette (opening
    /// the panel for those shown in it), or a model or effort choice for a question or card.
    fn run_assistant_requests(&mut self, host: &UiContext) -> Option<Action> {
        let requests = self.take_assistant_requests();
        if requests.is_empty() {
            return None;
        }
        let mut action = Action::Redraw;
        for request in requests {
            match request {
                crate::assistant::LinkRequest::Action(name) => {
                    let overlay = matches!(
                        name.as_str(),
                        "assistant_model" | "assistant_permissions" | "assistant_tools"
                    );
                    if !overlay {
                        self.assistant.focus();
                    }
                    let place = self.place(true);
                    let outcome = self.assistant.run_action(&name, &place, host);
                    let result = self.assistant_outcome(outcome, host);
                    if result != Action::Redraw && result != Action::None {
                        action = result;
                    }
                }
                crate::assistant::LinkRequest::Model(target) => {
                    self.assistant.open_model_picker(target, host);
                }
                crate::assistant::LinkRequest::Effort(target) => {
                    self.assistant.cycle_effort(&target, host);
                }
                crate::assistant::LinkRequest::OpenGuide => {
                    let docked = self.screen_shown;
                    if let Route::Review(review) = &mut self.route {
                        if docked && review.guide_docks() {
                            review.open_guide_panel();
                        } else {
                            self.assistant.focus();
                            self.share_guide(false);
                            self.assistant.show_guide_tab();
                        }
                    }
                }
                crate::assistant::LinkRequest::Message { text, send } => {
                    self.assistant.focus();
                    if send {
                        let place = self.place(true);
                        let outcome = self.assistant.send_message(&text, &place, host);
                        let result = self.assistant_outcome(outcome, host);
                        if result != Action::Redraw && result != Action::None {
                            action = result;
                        }
                    } else {
                        self.assistant.prefill(&text);
                    }
                }
                crate::assistant::LinkRequest::GuideMessage {
                    text,
                    grant,
                    context,
                } => {
                    self.assistant.focus();
                    let place = self.place(true);
                    let outcome = self
                        .assistant
                        .send_guide_message(&text, grant, context, &place, host);
                    let result = self.assistant_outcome(outcome, host);
                    if result != Action::Redraw && result != Action::None {
                        action = result;
                    }
                }
            }
        }
        Some(action)
    }

    /// Give the assistant the review's guide: its card, and the Chat and Guide tabs when the
    /// screen (`screen_shown`) has no room to dock the guide panel. A guide jump, or a guide just
    /// started, shows the Guide tab when the tabs show; the Chat tab keeps its dot while the
    /// conversation needs the reviewer.
    fn share_guide(&mut self, screen_shown: bool) {
        let Route::Review(review) = &mut self.route else {
            self.assistant.set_guide(None, false);
            return;
        };
        let tabs = review.has_guide() && (!screen_shown || !review.guide_docks());
        self.assistant.set_guide(review.guide_card(), tabs);
        let stepped = review.take_guide_stepped();
        let revealed = review.take_guide_reveal();
        if (stepped || revealed) && tabs {
            self.assistant.show_guide_tab();
        }
    }

    /// Give the review the assistant's `model · effort` labels for its question composer and
    /// cards, asking for the cards' sessions' models not known yet.
    fn share_model_labels(&mut self, host: &UiContext) {
        let Route::Review(review) = &mut self.route else {
            return;
        };
        let mut sessions = review.card_sessions(MAX_CARD_MODELS);
        if let Some(session) = review.composer_session()
            && !sessions.contains(&session)
        {
            sessions.insert(0, session);
        }
        self.assistant.learn_models(&sessions, host);
        let labels = sessions
            .iter()
            .filter(|session| self.assistant.models().has_label(session))
            .map(|session| {
                (
                    session.clone(),
                    self.assistant.model_effort_label(Some(session)),
                )
            })
            .collect();
        review.set_model_labels(self.assistant.model_effort_label(None), labels);
    }
}

/// Ask AI cards whose session's model the review shows at most.
const MAX_CARD_MODELS: usize = 16;

/// Create a durable workspace.
async fn create_workspace(
    services: ReviewServices,
    repo: PathBuf,
    title: String,
    sources: Vec<ReviewSource>,
    presentation: Option<bcode_code_review_models::ReviewPresentationState>,
) -> Result<ReviewWorkspace, String> {
    let response: CreateReviewWorkspaceResponse = services
        .call(CreateReviewWorkspaceRequest {
            repo_path: repo,
            title: Some(title),
            sources,
            presentation,
        })
        .await
        .map_err(|error| error.message)?;
    Ok(response.workspace)
}

fn paint_opening(opening: &Opening, theme: &ReviewTheme, area: Rect, cx: &mut PaintCx<'_, '_>) {
    cx.fill(
        LocalRect::new(area.x.into(), area.y.into(), area.width, area.height),
        " ",
        theme.canvas,
    );
    let spinner = if theme.icons.ellipsis.is_ascii() {
        Spinner::ascii()
    } else {
        Spinner::braille()
    };
    let (title, icon, body) = opening.error.as_ref().map_or_else(
        || {
            (
                opening.label.as_str(),
                spinner.frame(opening.started.elapsed()).to_owned(),
                vec![Line::from_spans(vec![Span::styled(
                    "esc: back to reviews",
                    theme.muted,
                )])],
            )
        },
        |error| {
            (
                "Could not open the review",
                "!".to_owned(),
                vec![
                    Line::from_spans(vec![Span::styled(error.clone(), theme.error)]),
                    Line::from(""),
                    Line::from_spans(vec![Span::styled(
                        "esc or enter: back to reviews",
                        theme.muted,
                    )]),
                ],
            )
        },
    );
    let state = EmptyStateComponent::new("app.opening", title)
        .icon(&icon)
        .body(&body)
        .policy(EmptyStatePolicy::centered())
        .styles(theme.component.empty_state_styles());
    cx.paint_component(&state, area);
}

impl ReviewApp {
    /// The linked session opened from the review returned (or failed to open).
    pub fn session_navigation_finished(
        &mut self,
        session_id: bcode_session_models::SessionId,
        result: Result<(), String>,
    ) {
        if let Route::Review(review) = &mut self.route {
            review.session_navigation_finished(session_id, result);
        }
    }
}

impl View for ReviewApp {
    fn prepare_keymap(&mut self, host: &UiContext) {
        self.keymap_stale = false;
        self.assistant.prepare_keymap(host);
        if let Some(home) = self.home.as_mut() {
            home.prepare_keymap(host);
        }
        match &mut self.route {
            Route::Review(review) => review.prepare_keymap(host),
            Route::Wizard(wizard) => wizard.prepare_keymap(host),
            Route::Home | Route::Opening(_) => {}
        }
    }

    fn render(
        &mut self,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        styles: Option<&ComponentStyles>,
        host: &UiContext,
    ) {
        self.apply_theme(styles);
        // Every cell inherits the canvas, so text without a background of its own sits on the
        // theme's canvas rather than the terminal's default background.
        frame.with_style(self.theme.canvas, |frame| {
            let split = self.assistant.split(area);
            match split.screen {
                Some(screen) => {
                    self.render_screen(screen, split.panel.is_some(), frame, styles, host);
                }
                None => self.button = None,
            }
            self.screen_shown = split.screen.is_some();
            self.share_guide(self.screen_shown);
            if let Some((rect, dock)) = split.panel {
                let place = self.place(false);
                self.assistant.paint(
                    area,
                    (rect, dock),
                    &place,
                    crate::assistant::panel::now_ms(),
                    &self.theme,
                    frame,
                );
                if let Route::Review(review) = &mut self.route {
                    match self.assistant.guide_area() {
                        Some(guide) => review.paint_guide_tab(guide, frame),
                        None => review.guide_tab_hidden(),
                    }
                }
            } else {
                // Closed, or open but hidden: nothing of the panel takes pointer input.
                self.assistant.not_painted();
            }
            self.assistant.paint_overlay(area, &self.theme, frame);
            if let Some(overlay) = self.appearance.as_mut() {
                overlay.paint(&self.theme, area, frame);
            }
        });
    }

    fn handle_event(&mut self, event: &Event, host: &UiContext) -> EventResult {
        if let Some(result) = self.appearance_event(event, host) {
            return result;
        }
        if let Some(outcome) = self.assistant.overlay_event(event, host) {
            return EventResult::from_action(self.assistant_outcome(outcome, host));
        }
        if let Some(action) = self.assistant_event(event, host) {
            self.refresh_keymap(host);
            return EventResult::from_action(action);
        }
        let mut result = self.route_event(event, host);
        if self.take_assistant_request() {
            self.assistant.toggle();
            if result.action == Action::None {
                result = EventResult::from_action(Action::Redraw);
            }
        }
        if let Some(action) = self.run_assistant_requests(host)
            && matches!(result.action, Action::None | Action::Redraw)
        {
            result = EventResult::from_action(action);
        }
        self.share_model_labels(host);
        if self.open_requested_appearance(host) && result.action == Action::None {
            result = EventResult::from_action(Action::Redraw);
        }
        self.refresh_keymap(host);
        result
    }

    fn poll(&mut self, host: &UiContext) -> Action {
        let mut action = self.poll_route(host);
        if self.take_assistant_request() {
            self.assistant.toggle();
        }
        if let Some(requested) = self.run_assistant_requests(host)
            && matches!(action, Action::None | Action::Redraw)
        {
            action = requested;
        }
        self.share_model_labels(host);
        if self.poll_assistant(host) && action == Action::None {
            action = Action::Redraw;
        }
        if self.open_requested_appearance(host) && action == Action::None {
            action = Action::Redraw;
        }
        self.refresh_keymap(host);
        action
    }
}
