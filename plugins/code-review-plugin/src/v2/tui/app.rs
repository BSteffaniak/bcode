//! The V2 code review app (spec 7.1): one plugin surface with internal routes.
//!
//! * **Routes.** [`Route::Home`] (the review home), [`Route::Wizard`] (new review),
//!   [`Route::Opening`] (resolving an entry or creating a workspace), and [`Route::Review`] (the
//!   review screen, [`ReviewSurfaceV2`]). Moving between them only swaps the route value: the
//!   plugin runtime and the terminal stay as they are, and home keeps its state (filter,
//!   selection, groups) while a review is open.
//! * **Transitions.** Opening a review from home, a suggestion, the wizard, or a direct entry
//!   shows the review route; leaving a review (its own exit, after it saved) returns to home and
//!   reloads it; `q` on home closes the surface. Entries that need answers first (pull requests,
//!   stacks, resume, creating a workspace) show the opening route with a spinner, or the failure
//!   with a way back.
//! * **Theme.** Home, the wizard, and the opening route derive the same [`ReviewTheme`] from the
//!   host theme as the review does.

use super::backend::{SharedBackend, call_json};
use super::config::ReviewViewConfig;
use super::entry::{self, Entry};
use super::home::{Home, HomeOutcome};
use super::theme::{IconStyle, ReviewTheme};
use super::wizard::{Wizard, WizardOutcome};
use super::{ReviewSurfaceV2, entry::transient_workspace};
use crate::v2::document::DocumentConfig;
use bcode_code_review_models::{
    CreateReviewWorkspaceRequest, CreateReviewWorkspaceResponse, OP_REVIEW_WORKSPACE_CREATE,
    ReviewSource, ReviewSourceKind, ReviewWorkspace,
};
use bcode_plugin_sdk::tui::{
    PluginTuiAction, PluginTuiEventResult, PluginTuiHost, PluginTuiSurface, PluginTuiTheme,
};
use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::empty_state::{EmptyStateComponent, EmptyStatePolicy};
use bmux_tui_components::spinner::Spinner;
use std::path::PathBuf;
use std::pin::Pin;
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
    Review(Box<ReviewSurfaceV2>),
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

/// The V2 code review app surface.
pub struct ReviewAppV2 {
    backend: SharedBackend,
    repo: PathBuf,
    config: ReviewViewConfig,
    route: Route,
    home: Option<Box<Home>>,
    theme_input: Option<PluginTuiTheme>,
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
}

impl std::fmt::Debug for ReviewAppV2 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewAppV2")
            .field("repo", &self.repo)
            .field("route", &self.route_name())
            .finish_non_exhaustive()
    }
}

impl ReviewAppV2 {
    /// An app for the repository at `repo` that starts at `entry`.
    #[must_use]
    pub fn new(
        backend: SharedBackend,
        repo: PathBuf,
        config: ReviewViewConfig,
        entry: Entry,
    ) -> Self {
        let icons = config.icons.unwrap_or_default();
        let mut app = Self {
            backend,
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
    pub fn review(&self) -> Option<&ReviewSurfaceV2> {
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

    // ----------------------------------------------------------------------------------------
    // Transitions
    // ----------------------------------------------------------------------------------------

    /// Let the routes read the keymap after a route change (hosts need not call
    /// `prepare_keymap` again on their own).
    fn refresh_keymap(&mut self, host: &dyn PluginTuiHost) {
        if std::mem::take(&mut self.keymap_stale) {
            self.prepare_keymap(host);
        }
    }

    fn show_home(&mut self, refresh: bool) {
        let home = self.home.get_or_insert_with(|| {
            Box::new(Home::new(Arc::clone(&self.backend), self.repo.clone()))
        });
        if refresh {
            home.refresh();
        }
        self.route = Route::Home;
        self.keymap_stale = true;
    }

    fn open_review(&mut self, workspace: ReviewWorkspace, host: Option<&dyn PluginTuiHost>) {
        let mut review = ReviewSurfaceV2::new(
            Arc::clone(&self.backend),
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
    fn start(&mut self, entry: Entry, host: &dyn PluginTuiHost) {
        match entry {
            Entry::Home => self.show_home(true),
            Entry::Workspace { workspace } => self.open_review(*workspace, Some(host)),
            Entry::Sources { title, sources } => {
                let workspace = transient_workspace(self.repo.clone(), title, sources);
                self.open_review(workspace, Some(host));
            }
            other => {
                let label = other.opening_label();
                let (backend, repo) = (Arc::clone(&self.backend), self.repo.clone());
                self.run_opening(label, host, async move {
                    entry::resolve(backend, repo, other).await
                });
            }
        }
    }

    fn run_opening(
        &mut self,
        label: String,
        host: &dyn PluginTuiHost,
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
    fn create(&mut self, title: String, sources: Vec<ReviewSourceKind>, host: &dyn PluginTuiHost) {
        let draft = transient_workspace(self.repo.clone(), Some(title), sources);
        let (backend, repo) = (Arc::clone(&self.backend), self.repo.clone());
        let label = format!("Creating “{}”…", draft.title);
        self.run_opening(label, host, async move {
            create_workspace(backend, repo, draft.title, draft.sources).await
        });
    }

    fn home_outcome(&mut self, outcome: HomeOutcome, host: &dyn PluginTuiHost) -> PluginTuiAction {
        match outcome {
            HomeOutcome::Ignored => PluginTuiAction::None,
            HomeOutcome::Redraw => PluginTuiAction::Redraw,
            HomeOutcome::Exit => PluginTuiAction::Close { outcome: None },
            HomeOutcome::Open(workspace) => {
                self.open_review(*workspace, Some(host));
                PluginTuiAction::Redraw
            }
            HomeOutcome::Start(entry) => {
                self.start(entry, host);
                PluginTuiAction::Redraw
            }
            HomeOutcome::New => {
                let mut wizard = Wizard::new(Arc::clone(&self.backend), self.repo.clone());
                wizard.prepare_keymap(host);
                self.route = Route::Wizard(Box::new(wizard));
                self.keymap_stale = true;
                PluginTuiAction::Redraw
            }
        }
    }

    fn wizard_outcome(
        &mut self,
        outcome: WizardOutcome,
        host: &dyn PluginTuiHost,
    ) -> PluginTuiAction {
        match outcome {
            WizardOutcome::Ignored => PluginTuiAction::None,
            WizardOutcome::Redraw => PluginTuiAction::Redraw,
            WizardOutcome::Cancel => {
                // Leaving an edit returns to the review unchanged.
                match self.editing.take() {
                    Some(workspace) => self.open_review(*workspace, Some(host)),
                    None => self.show_home(false),
                }
                PluginTuiAction::Redraw
            }
            WizardOutcome::Update { workspace } => {
                self.editing = None;
                self.update_sources(*workspace, host);
                PluginTuiAction::Redraw
            }
            WizardOutcome::Create { title, sources } => {
                self.create(title, sources, host);
                PluginTuiAction::Redraw
            }
        }
    }

    /// A review's action, with its close turned into the way back to home (or into the
    /// source editor when the review asked for it).
    fn review_action(
        &mut self,
        action: PluginTuiAction,
        host: &dyn PluginTuiHost,
    ) -> PluginTuiAction {
        let PluginTuiAction::Close { outcome } = &action else {
            return action;
        };
        let edit = outcome
            .as_ref()
            .and_then(|outcome| outcome.get(super::surface::EDIT_SOURCES_OUTCOME))
            .is_some_and(|value| value.as_bool() == Some(true));
        if edit && let Route::Review(review) = &self.route {
            let workspace = review.edit_target();
            let mut wizard = Wizard::editing(
                Arc::clone(&self.backend),
                self.repo.clone(),
                workspace.clone(),
            );
            wizard.prepare_keymap(host);
            self.editing = Some(Box::new(workspace));
            self.route = Route::Wizard(Box::new(wizard));
            self.keymap_stale = true;
            return PluginTuiAction::Redraw;
        }
        self.show_home(true);
        PluginTuiAction::Redraw
    }

    /// Save edited sources (durable reviews) and open the review with them.
    fn update_sources(&mut self, workspace: ReviewWorkspace, host: &dyn PluginTuiHost) {
        if workspace.id == entry::TRANSIENT_WORKSPACE_ID {
            self.open_review(workspace, Some(host));
            return;
        }
        let backend = Arc::clone(&self.backend);
        let label = format!("Saving “{}”…", workspace.title);
        self.run_opening(label, host, async move {
            let repo_path = workspace.repo_root.clone();
            let response: bcode_code_review_models::UpdateReviewWorkspaceResponse = call_json(
                backend.as_ref(),
                bcode_code_review_models::OP_REVIEW_WORKSPACE_UPDATE,
                &bcode_code_review_models::UpdateReviewWorkspaceRequest {
                    repo_path,
                    workspace,
                },
            )
            .await
            .map_err(|error| error.message)?;
            Ok(response.workspace)
        });
    }

    /// Route one event to the route on screen.
    fn route_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiEventResult {
        let theme = self.theme;
        let action = match &mut self.route {
            Route::Review(review) => {
                let result = review.handle_event_result(event, host);
                if matches!(result.action, PluginTuiAction::Close { .. }) {
                    return PluginTuiEventResult::from_legacy(
                        self.review_action(result.action, host),
                    );
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
                    PluginTuiAction::Redraw
                }
                Event::Resize(_) => PluginTuiAction::Redraw,
                _ => PluginTuiAction::None,
            },
            Route::Home => {
                let Some(home) = self.home.as_mut() else {
                    return PluginTuiEventResult::from_legacy(PluginTuiAction::None);
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
        PluginTuiEventResult::from_legacy(action)
    }

    /// Poll the route on screen (and resolve entries).
    fn poll_route(&mut self, host: &dyn PluginTuiHost) -> PluginTuiAction {
        if let Some(entry) = self.pending.take() {
            self.start(entry, host);
            return PluginTuiAction::Redraw;
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
                    PluginTuiAction::Redraw
                } else {
                    PluginTuiAction::None
                }
            }
            Route::Wizard(wizard) => {
                if wizard.poll(host, &theme) {
                    PluginTuiAction::Redraw
                } else {
                    PluginTuiAction::None
                }
            }
            // The spinner animates while resolving.
            Route::Opening(opening) if opening.error.is_none() => PluginTuiAction::Redraw,
            Route::Opening(_) => PluginTuiAction::None,
        };
        if changed && action == PluginTuiAction::None {
            PluginTuiAction::Redraw
        } else {
            action
        }
    }

    fn apply_theme(&mut self, theme: Option<&PluginTuiTheme>) {
        if self.themed && self.theme_input.as_ref() == theme {
            return;
        }
        self.themed = true;
        self.theme_input = theme.copied();
        self.theme = ReviewTheme::derive(theme, self.config.icons.unwrap_or(IconStyle::Unicode));
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
            Route::Review(review) => review.render_with_theme(area, cx, self.theme_input.as_ref()),
        }
    }
}

/// Create a durable workspace.
async fn create_workspace(
    backend: SharedBackend,
    repo: PathBuf,
    title: String,
    sources: Vec<ReviewSource>,
) -> Result<ReviewWorkspace, String> {
    let response: CreateReviewWorkspaceResponse = call_json(
        backend.as_ref(),
        OP_REVIEW_WORKSPACE_CREATE,
        &CreateReviewWorkspaceRequest {
            repo_path: repo,
            title: Some(title),
            sources,
        },
    )
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

impl PluginTuiSurface for ReviewAppV2 {
    fn id(&self) -> &'static str {
        super::CODE_REVIEW_SURFACE_KIND
    }

    fn title(&self) -> &'static str {
        "Code Review"
    }

    fn prepare_keymap(&mut self, host: &dyn PluginTuiHost) {
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

    fn render(&mut self, area: Rect, frame: &mut PaintCx<'_, '_>) {
        self.apply_theme(None);
        self.render_route(area, frame);
    }

    fn render_with_theme(
        &mut self,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        theme: Option<&PluginTuiTheme>,
    ) {
        self.apply_theme(theme);
        self.render_route(area, frame);
    }

    fn render_with_host(
        &mut self,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        theme: Option<&PluginTuiTheme>,
        host: &dyn PluginTuiHost,
    ) {
        self.apply_theme(theme);
        if let Route::Review(review) = &mut self.route {
            review.render_with_host(area, frame, theme, host);
            return;
        }
        self.render_route(area, frame);
    }

    fn handle_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction {
        self.handle_event_result(event, host).action
    }

    fn handle_event_result(
        &mut self,
        event: &Event,
        host: &dyn PluginTuiHost,
    ) -> PluginTuiEventResult {
        let result = self.route_event(event, host);
        self.refresh_keymap(host);
        result
    }

    fn focused_text_field_event(
        &mut self,
        event: &Event,
        host: &dyn PluginTuiHost,
    ) -> PluginTuiEventResult {
        match &mut self.route {
            Route::Review(review) => review.focused_text_field_event(event, host),
            _ => PluginTuiEventResult::from_legacy(PluginTuiAction::None),
        }
    }

    fn focused_text_field(&self) -> Option<bcode_plugin_sdk::tui::FocusedTextFieldSnapshot> {
        match &self.route {
            Route::Review(review) => review.focused_text_field(),
            _ => None,
        }
    }

    fn apply_focused_text_field_edit_result(
        &mut self,
        edit: &bcode_plugin_sdk::tui::FocusedTextFieldEdit,
        host: &dyn PluginTuiHost,
    ) -> bcode_plugin_sdk::tui::PluginTuiFocusedTextFieldEditResult {
        match &mut self.route {
            Route::Review(review) => review.apply_focused_text_field_edit_result(edit, host),
            _ => bcode_plugin_sdk::tui::PluginTuiFocusedTextFieldEditResult::rejected(
                bcode_plugin_sdk::composer::ComposerValidationError::Stale,
            ),
        }
    }

    fn session_navigation_finished(
        &mut self,
        session_id: bcode_session_models::SessionId,
        result: Result<(), String>,
    ) {
        if let Route::Review(review) = &mut self.route {
            review.session_navigation_finished(session_id, result);
        }
    }

    fn poll(&mut self, host: &dyn PluginTuiHost) -> PluginTuiAction {
        let action = self.poll_route(host);
        self.refresh_keymap(host);
        action
    }

    fn drain_effects<'a>(
        &'a mut self,
        host: &'a dyn PluginTuiHost,
    ) -> Pin<Box<dyn std::future::Future<Output = PluginTuiAction> + Send + 'a>> {
        match &mut self.route {
            Route::Review(review) => review.drain_effects(host),
            _ => Box::pin(async { PluginTuiAction::None }),
        }
    }
}
