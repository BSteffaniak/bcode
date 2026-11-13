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

use super::appearance::{AppearanceOutcome, AppearanceOverlay};
use super::config::ReviewViewConfig;
use super::entry::{self, Entry};
use super::home::{Home, HomeOutcome};
use super::theme::ReviewTheme;
use super::wizard::{Wizard, WizardOutcome};
use super::{ReviewSurface, entry::transient_workspace};
use crate::action::Action;
use crate::action::EventResult;
use crate::context::UiContext;
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
    /// The reviews a comparison was opened from (the stack history), most recent last: closing
    /// a review returns to the one before it instead of home.
    return_to: Vec<ReviewWorkspace>,
    /// The appearance picker, over every route while open.
    appearance: Option<AppearanceOverlay>,
}

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
            return_to: Vec::new(),
            appearance: None,
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
    fn create(&mut self, title: String, sources: Vec<ReviewSourceKind>, host: &UiContext) {
        let draft = transient_workspace(self.repo.clone(), Some(title), sources);
        let (services, repo) = (self.services.clone(), self.repo.clone());
        let label = format!("Creating “{}”…", draft.title);
        self.run_opening(label, host, async move {
            create_workspace(services, repo, draft.title, draft.sources).await
        });
    }

    fn home_outcome(&mut self, outcome: HomeOutcome, host: &UiContext) -> Action {
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
            HomeOutcome::New => {
                let mut wizard = Wizard::new(self.services.clone(), self.repo.clone());
                wizard.prepare_keymap(host);
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
                self.editing = None;
                self.update_sources(*workspace, host);
                Action::Redraw
            }
            WizardOutcome::Create { title, sources } => {
                self.create(title, sources, host);
                Action::Redraw
            }
        }
    }

    /// A review's action, with its close turned into the way back to home (or into the
    /// source editor when the review asked for it).
    fn review_action(&mut self, action: Action, host: &UiContext) -> Action {
        if let Action::OpenReview { title, sources } = action {
            // A comparison opens as its own review; closing it comes back here.
            if let Route::Review(review) = &self.route {
                self.return_to.push(review.edit_target());
            }
            let workspace = transient_workspace(self.repo.clone(), Some(title), sources);
            self.open_review(workspace, Some(host));
            return Action::Redraw;
        }
        let Action::Close { edit_sources } = action else {
            return action;
        };
        if edit_sources && let Route::Review(review) = &self.route {
            let workspace = review.edit_target();
            let mut wizard =
                Wizard::editing(self.services.clone(), self.repo.clone(), workspace.clone());
            wizard.prepare_keymap(host);
            self.editing = Some(Box::new(workspace));
            self.route = Route::Wizard(Box::new(wizard));
            self.keymap_stale = true;
            return Action::Redraw;
        }
        if let Some(previous) = self.return_to.pop() {
            self.open_review(previous, Some(host));
            return Action::Redraw;
        }
        self.show_home(true);
        Action::Redraw
    }

    /// Save edited sources (durable reviews) and open the review with them.
    fn update_sources(&mut self, workspace: ReviewWorkspace, host: &UiContext) {
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
                    Event::Paste(text) => home.paste(text),
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

/// Create a durable workspace.
async fn create_workspace(
    services: ReviewServices,
    repo: PathBuf,
    title: String,
    sources: Vec<ReviewSource>,
) -> Result<ReviewWorkspace, String> {
    let response: CreateReviewWorkspaceResponse = services
        .call(CreateReviewWorkspaceRequest {
            repo_path: repo,
            title: Some(title),
            sources,
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
            if let Route::Review(review) = &mut self.route {
                review.render(area, frame, styles, host);
            } else {
                self.render_route(area, frame);
            }
            if let Some(overlay) = self.appearance.as_mut() {
                overlay.paint(&self.theme, area, frame);
            }
        });
    }

    fn handle_event(&mut self, event: &Event, host: &UiContext) -> EventResult {
        if let Some(result) = self.appearance_event(event, host) {
            return result;
        }
        let mut result = self.route_event(event, host);
        if self.open_requested_appearance(host) && result.action == Action::None {
            result = EventResult::from_action(Action::Redraw);
        }
        self.refresh_keymap(host);
        result
    }

    fn poll(&mut self, host: &UiContext) -> Action {
        let mut action = self.poll_route(host);
        if self.open_requested_appearance(host) && action == Action::None {
            action = Action::Redraw;
        }
        self.refresh_keymap(host);
        action
    }
}
