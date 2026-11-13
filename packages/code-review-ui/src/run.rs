//! Terminal ownership, the runtime loop, and handing the terminal to Bcode sessions.
//!
//! The app runs its own `bmux_tui_runtime` loop:
//!
//! * Terminal input goes straight to the app ([`View::handle_event`]); every queued event is
//!   handled before the next frame.
//! * Work the app spawns runs on the host; each completion (and every explicit redraw request)
//!   wakes the loop, which polls the app ([`View::poll`]) and presents a frame. Wakes coalesce
//!   while one is pending.
//! * After each presented frame the app is polled again, so data needed by the new viewport is
//!   requested at once; while the app keeps reporting changes (animations, spinners) frames
//!   continue at the runtime cadence, and the loop goes idle when nothing changes.
//! * With the standalone host the app owns its theme: the review's own saved appearance, else
//!   bcode's. An appearance watcher re-resolves it when configuration, the saved appearance or
//!   theme files change, or when the terminal reports its background, and offers it to the
//!   app's [`AppearanceControl`], which restyles the host unless the appearance picker is
//!   previewing.
//! * Opening a session in Bcode ends the runtime, runs the host's open command with the terminal
//!   handed over, and resumes the app where it was.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bcode_code_review::ReviewStore;
use bcode_session_models::SessionId;
use bcode_tui_theme::appearance::{AppearanceWatcher, VariantPreference};
use bcode_tui_theme::components::ComponentStyles;
use bmux_tui::capabilities::{TerminalCapabilities, TerminalColorDepth};
use bmux_tui::crossterm::{CrosstermTerminalGuard, terminal_size};
use bmux_tui::event::EventOutcome;
use bmux_tui::focus::FocusTrap;
use bmux_tui::geometry::Rect;
use bmux_tui::hit::HitMap;
use bmux_tui::paint::PaintCx;
#[cfg(unix)]
use bmux_tui::terminal::TtyOutput;
use bmux_tui::terminal::{ScrollRegionAcceleration, Terminal};
use bmux_tui_runtime::{Program, RuntimeEvent, RuntimeHandle, Update};

use crate::action::Action;
use crate::app::ReviewApp;
use crate::appearance::AppearanceControl;
use crate::config::LoadedConfig;
use crate::context::UiContext;
use crate::entry::Entry;
use crate::host::{BcodeAccess, ExternalCommand, ReviewHost, StandaloneHost};
use crate::services::ReviewServices;
use crate::theme::HostTheme;
use crate::view::View;

/// Terminal input events buffered before the reader waits for the app.
const INPUT_BUFFER: usize = 64;

/// How often the appearance watcher checks its files for changes.
const APPEARANCE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(750);

/// Inputs for [`run`] and [`build_app`].
pub struct RunOptions {
    /// Directory inside the repository to review.
    pub path: PathBuf,
    /// Loaded configuration.
    pub config: LoadedConfig,
    /// Host capabilities; `None` uses a [`StandaloneHost`] over the default Bcode daemon.
    pub host: Option<Arc<dyn ReviewHost>>,
    /// What the app opens first.
    pub entry: Entry,
    /// The Bcode executable the app runs inside (`bcode review`): the standalone host opens
    /// sessions with it and may start the daemon from it. `None` (the standalone `bcode-review`
    /// binary) uses the user's installed Bcode (`BCODE_BIN`, else `bcode` on the path), whose
    /// daemon it verifies and starts when needed, since this executable cannot serve as one.
    pub bcode_program: Option<PathBuf>,
}

/// Failure starting or running the app.
#[derive(Debug)]
pub enum RunError {
    /// Invalid keybinding configuration.
    Keymap(String),
    /// Terminal or runtime failure.
    Terminal(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keymap(message) | Self::Terminal(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for RunError {}

/// Messages of the run loop.
#[derive(Debug)]
pub enum Msg {
    /// The runtime started or resumed.
    Start,
    /// The host finished work or asked for a frame.
    Wake,
    /// Terminal input stopped.
    InputFailed(String),
}

/// The app as the runtime drives it.
pub struct App {
    color_depth: TerminalColorDepth,
    review: ReviewApp,
    ui: UiContext,
    styles: Option<(u64, ComponentStyles)>,
    wake_pending: Arc<AtomicBool>,
    open_session: Option<SessionId>,
    quit: bool,
    diagnostics: Vec<String>,
    /// Keeps the standalone host's theme current; `None` when the host owns its theme.
    appearance: Option<AppearanceWatcher>,
    /// Whether to ask the terminal for its colors, which select the light or dark variant.
    query_terminal_colors: bool,
}

impl std::fmt::Debug for App {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("App")
            .field("review", &self.review)
            .field("quit", &self.quit)
            .finish_non_exhaustive()
    }
}

impl App {
    /// The review app.
    #[must_use]
    pub const fn review(&self) -> &ReviewApp {
        &self.review
    }

    /// Problems found while starting (configuration, keybindings, theme).
    #[must_use]
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// Paint the app into `cx`, which covers the whole terminal.
    pub fn paint(&mut self, cx: &mut PaintCx<'_, '_>) {
        let generation = self.ui.host().theme_generation();
        if self
            .styles
            .as_ref()
            .is_none_or(|(cached, _)| *cached != generation)
        {
            self.styles = Some((generation, self.ui.host().theme().styles));
        }
        let styles = self.styles.as_ref().map(|(_, styles)| styles);
        let area = cx.clip();
        self.review.render(area, cx, styles, &self.ui);
    }

    /// The runtime update for a screen action.
    fn act(&mut self, action: &Action, outcome: EventOutcome) -> Update<Msg> {
        match action {
            Action::Close { .. } => {
                self.quit = true;
                Update::exit()
            }
            Action::OpenSession { session_id } => {
                self.open_session = Some(*session_id);
                Update::exit()
            }
            Action::None if outcome == EventOutcome::Ignored => Update::none(),
            // The app opens comparisons itself; one reaching here only needs drawing.
            Action::Redraw | Action::None | Action::OpenReview { .. } => Update::redraw(),
        }
    }

    fn poll(&mut self) -> Update<Msg> {
        let action = self.review.poll(&self.ui);
        self.act(&action, EventOutcome::Ignored)
    }
}

impl Program for App {
    type Message = Msg;
    type Error = String;

    fn presentation_committed(&mut self, _report: bmux_tui_runtime::PresentReport) -> Update<Msg> {
        self.poll()
    }

    fn update(&mut self, event: RuntimeEvent<Msg>) -> Result<Update<Msg>, String> {
        Ok(match event {
            RuntimeEvent::Terminal(event) => {
                let result = self.review.handle_event(&event, &self.ui);
                self.act(&result.action, result.outcome)
            }
            RuntimeEvent::Message(Msg::Start) => Update::redraw().merge(self.poll()),
            RuntimeEvent::Message(Msg::Wake) => {
                self.wake_pending.store(false, Ordering::Release);
                Update::redraw().merge(self.poll())
            }
            RuntimeEvent::Message(Msg::InputFailed(message)) => return Err(message),
            RuntimeEvent::Timer(_) => Update::none(),
        })
    }
}

/// Build application state without touching the terminal, with the theme and host the options
/// select. The host's redraws reach no runtime yet; [`run`] connects them.
///
/// # Errors
///
/// * [`RunError::Keymap`] when keybinding overrides are invalid.
///
/// # Panics
///
/// Panics when called outside a Tokio runtime without a host in `options`.
pub fn build_app(options: RunOptions) -> Result<App, RunError> {
    let repo = bcode_code_review::scope::working_tree_root(&options.path);
    let mut loaded = options.config;
    let color_depth = loaded
        .color_depth
        .resolve(TerminalCapabilities::detect().color_depth);
    loaded.view.color_depth = Some(color_depth);
    let keymap = crate::keymap::registry(loaded.keybindings.clone())
        .map_err(|error| RunError::Keymap(format!("invalid review keybindings: {error}")))?;
    let mut diagnostics = loaded.diagnostics.clone();
    let mut appearance = None;
    let mut control_for_ui = None;
    let host = if let Some(host) = options.host {
        host
    } else {
        let sources = crate::config::appearance_sources(&loaded, &repo);
        let background = TerminalCapabilities::detect().background;
        let resolution = sources.resolve(background);
        let (theme, theme_diagnostics) = HostTheme::from_resolution(&resolution);
        diagnostics.extend(theme_diagnostics);
        let access = options
            .bcode_program
            .map_or(BcodeAccess::Installed, |program| BcodeAccess::Running {
                client: Box::new(
                    bcode_client::BcodeClient::default_endpoint()
                        .with_daemon_availability(bcode_client::DaemonAvailability::AutoStart),
                ),
                program,
            });
        let standalone = Arc::new(StandaloneHost::new(access, theme));
        let control = Arc::new(AppearanceControl::new(
            resolution,
            {
                let host = Arc::clone(&standalone);
                Box::new(move |resolution| {
                    let (theme, _) = HostTheme::from_resolution(resolution);
                    if host.theme() != theme {
                        host.set_theme(theme);
                    }
                })
            },
            loaded.state_file.clone(),
        ));
        appearance = Some(watch_appearance(
            sources,
            background,
            repo.clone(),
            Arc::clone(&control),
        ));
        control_for_ui = Some(control);
        standalone as Arc<dyn ReviewHost>
    };
    let query_terminal_colors =
        appearance.is_some() && loaded.theme.variant == VariantPreference::Auto;
    let services = ReviewServices::new(ReviewStore::new(loaded.store), Arc::clone(&host))
        .with_stack_limit(loaded.stack_limit)
        // Disposable: a review opens from what an earlier run computed and learned, then confirms
        // it (`BCODE_CODE_REVIEW_CACHE=off` disables it).
        .with_cache(Arc::new(bcode_code_review::cache::ReviewCache::for_user()))
        // Built-in providers run in-process; a running Bcode's plugins can add more.
        .with_source(Arc::new(bcode_code_review_github::GitHubReviewSource))
        .with_contributor(Arc::new(bcode_code_review::discovery::git::GitDiscovery))
        .with_contributor(Arc::new(bcode_code_review_graphite::GraphiteDiscovery));
    let mut ui = UiContext::new(host, Arc::new(keymap), loaded.diff_layout);
    if let Some(control) = control_for_ui {
        ui = ui.with_appearance(control);
    }
    let mut review = ReviewApp::new(services, repo, loaded.view, options.entry);
    review.prepare_keymap(&ui);
    Ok(App {
        color_depth,
        review,
        ui,
        styles: None,
        wake_pending: Arc::new(AtomicBool::new(false)),
        open_session: None,
        quit: false,
        diagnostics,
        appearance,
        query_terminal_colors,
    })
}

/// Offer `control` every new appearance of `repo`: the review's own choice or bcode's, as
/// configuration, user state and theme files change.
fn watch_appearance(
    sources: bcode_tui_theme::appearance::AppearanceSources,
    background: bmux_tui::capabilities::TerminalBackground,
    repo: PathBuf,
    control: Arc<AppearanceControl>,
) -> AppearanceWatcher {
    AppearanceWatcher::spawn(
        sources,
        background,
        APPEARANCE_INTERVAL,
        move || crate::config::appearance_sources(&crate::config::load(&repo), &repo),
        move |resolution| control.offer(resolution),
    )
}

/// Render the current state into plain text rows, without a terminal. Intended for tests and
/// previews; styles are discarded.
#[must_use]
pub fn render_text(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, width, height));
    {
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
        let mut cx = PaintCx::new(&mut frame);
        app.paint(&mut cx);
    }
    (0..height)
        .map(|y| {
            buffer
                .row_symbols(y)
                .unwrap_or_default()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn paint(app: &mut App, cx: &mut PaintCx<'_, '_>) {
    app.paint(cx);
}

/// The app paints from its own state and routes input with its own hit testing, so committed
/// interaction scenes need no observer.
const fn committed(_app: &mut App, _hits: &HitMap, _focus: &FocusTrap) {}

type Render = fn(&mut App, &mut PaintCx<'_, '_>);
type Commit = fn(&mut App, &HitMap, &FocusTrap);

/// Presents frames to the terminal: through an output thread over a separate nonblocking
/// descriptor of the terminal where the platform allows it, so a slow terminal never blocks
/// input handling (the runtime draws the latest state once the previous frame is delivered,
/// so frames superseded meanwhile are never written), else directly to stdout.
///
/// Both move rows with terminal scroll regions when the review list scrolls, with left/right
/// margins on terminals known to support them, so a scroll writes only the rows it reveals.
enum AppPresenter {
    #[cfg(unix)]
    Async(Box<bmux_tui_runtime::AsyncTerminalPresenter<TtyOutput, Render, Commit>>),
    Blocking(Box<bmux_tui_runtime::TerminalPresenter<std::io::Stdout, Render>>),
}

impl AppPresenter {
    fn new(area: Rect, depth: TerminalColorDepth) -> Self {
        let acceleration = if TerminalCapabilities::left_right_margins() {
            ScrollRegionAcceleration::AutoWithMargins
        } else {
            ScrollRegionAcceleration::Auto
        };
        #[cfg(unix)]
        if let Ok(output) = TtyOutput::open() {
            let mut terminal = Terminal::new(Vec::new(), area);
            terminal.set_scroll_region_acceleration(acceleration);
            terminal.set_color_depth(depth);
            return Self::Async(Box::new(bmux_tui_runtime::AsyncTerminalPresenter::new(
                terminal,
                output,
                paint as Render,
                committed as Commit,
            )));
        }
        let mut terminal = Terminal::new(std::io::stdout(), area);
        terminal.set_scroll_region_acceleration(acceleration);
        terminal.set_color_depth(depth);
        Self::Blocking(Box::new(bmux_tui_runtime::TerminalPresenter::new(
            terminal,
            paint as Render,
        )))
    }
}

/// Forward a presenter call to whichever presenter is active.
macro_rules! forward {
    ($self:ident, $presenter:ident => $call:expr) => {
        match $self {
            #[cfg(unix)]
            Self::Async($presenter) => $call,
            Self::Blocking($presenter) => $call,
        }
    };
}

impl bmux_tui_runtime::Presenter<App> for AppPresenter {
    type Error = std::io::Error;

    fn resize(&mut self, size: bmux_tui::geometry::Size) {
        forward!(self, presenter => bmux_tui_runtime::Presenter::<App>::resize(presenter.as_mut(), size));
    }

    fn reset(&mut self, reason: bmux_tui_runtime::ResetReason) {
        forward!(self, presenter => bmux_tui_runtime::Presenter::<App>::reset(presenter.as_mut(), reason));
    }

    fn present(&mut self, program: &mut App) -> std::io::Result<bmux_tui_runtime::PresentReport> {
        forward!(self, presenter => presenter.present(program))
    }

    fn start_present(
        &mut self,
        program: &mut App,
    ) -> std::io::Result<Option<bmux_tui_runtime::PresentReport>> {
        forward!(self, presenter => presenter.start_present(program))
    }

    fn poll_present(
        &mut self,
        cx: &mut std::task::Context<'_>,
        program: &mut App,
    ) -> std::task::Poll<std::io::Result<bmux_tui_runtime::PresentReport>> {
        forward!(self, presenter => presenter.poll_present(cx, program))
    }

    fn cancel_present(&mut self) {
        forward!(self, presenter => bmux_tui_runtime::Presenter::<App>::cancel_present(presenter.as_mut()));
    }

    fn observe_terminal_event(&mut self, event: &bmux_tui::event::Event) -> bool {
        forward!(self, presenter => bmux_tui_runtime::Presenter::<App>::observe_terminal_event(presenter.as_mut(), event))
    }

    fn set_pointer_context(&mut self, context: bmux_tui::terminal::PointerContext) {
        forward!(self, presenter => bmux_tui_runtime::Presenter::<App>::set_pointer_context(presenter.as_mut(), context));
    }

    fn set_presentation_clock(&mut self, clock: bmux_tui::clock::PresentationClock) {
        forward!(self, presenter => bmux_tui_runtime::Presenter::<App>::set_presentation_clock(presenter.as_mut(), clock));
    }

    async fn restore(&mut self) -> std::io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Async(presenter) => {
                bmux_tui_runtime::Presenter::<App>::restore(presenter.as_mut()).await
            }
            Self::Blocking(presenter) => {
                bmux_tui_runtime::Presenter::<App>::restore(presenter.as_mut()).await
            }
        }
    }
}

/// The runtime of the current terminal session, which host redraw requests wake.
type WakeTarget = Arc<Mutex<Option<RuntimeHandle<Msg>>>>;

/// Run the app until the user quits.
///
/// # Errors
///
/// * [`RunError`] for keybinding, terminal, or runtime failures.
pub async fn run(options: RunOptions) -> Result<(), RunError> {
    let mut app = build_app(options)?;
    let target = WakeTarget::default();
    app.ui
        .host()
        .connect_redraw(wake(Arc::clone(&target), Arc::clone(&app.wake_pending)));
    loop {
        app = Box::pin(session(app, &target)).await?;
        let Some(session_id) = app.open_session.take() else {
            break;
        };
        if app.quit {
            break;
        }
        let result = app
            .ui
            .host()
            .open_session_command(&session_id.to_string())
            .map_or_else(
                || Err("this host cannot open sessions".to_owned()),
                |command| run_external(&command),
            );
        app.review.session_navigation_finished(session_id, result);
    }
    Ok(())
}

/// One terminal session; returns when the user quits or a session is opened.
async fn session(app: App, target: &WakeTarget) -> Result<App, RunError> {
    let terminal_error = |error: std::io::Error| RunError::Terminal(error.to_string());
    let guard = CrosstermTerminalGuard::enter(std::io::stdout()).map_err(terminal_error)?;
    let size = terminal_size().map_err(terminal_error)?;
    let presenter = AppPresenter::new(Rect::new(0, 0, size.width, size.height), app.color_depth);
    app.wake_pending.store(false, Ordering::Release);
    let reporter = app
        .appearance
        .as_ref()
        .map(AppearanceWatcher::background_reporter);
    if app.query_terminal_colors {
        // Replies arrive as terminal input; terminals that ignore the query never answer and
        // nothing waits on them.
        let mut stdout = std::io::stdout();
        let _ = std::io::Write::write_all(
            &mut stdout,
            bmux_tui::capabilities::TerminalColorReport::QUERY,
        )
        .and_then(|()| std::io::Write::flush(&mut stdout));
    }
    let (runtime, handle) =
        bmux_tui_runtime::Runtime::new(app, presenter, bmux_tui_runtime::RuntimeConfig::default());
    set_target(target, Some(handle.clone()));

    let input_handle = handle.clone();
    let input = tokio::spawn(async move {
        let mut input = bmux_tui_runtime::ManagedTerminalInput::start(INPUT_BUFFER);
        loop {
            match input.recv().await {
                Some(Ok(Some(event))) => {
                    if let (Some(reporter), bmux_tui::event::Event::User(tag)) = (&reporter, &event)
                        && tag == bmux_tui_runtime::TERMINAL_COLORS_EVENT
                    {
                        let background = input.terminal_colors().background_appearance();
                        if background != bmux_tui::capabilities::TerminalBackground::Unknown {
                            reporter.report(background);
                        }
                    }
                    if input_handle.send_terminal(event).await.is_err() {
                        break;
                    }
                }
                Some(Ok(None)) => {}
                Some(Err(error)) => {
                    let _ = input_handle
                        .send(Msg::InputFailed(format!("input: {error}")))
                        .await;
                    break;
                }
                None => break,
            }
        }
        input.shutdown().await;
    });
    let _ = handle.send(Msg::Start).await;

    let result = Box::pin(runtime.run()).await;
    set_target(target, None);
    input.abort();
    let _ = input.await;
    drop(guard);
    match result {
        Ok(output) => Ok(output.program),
        Err(bmux_tui_runtime::RuntimeError::Program { error, .. }) => {
            Err(RunError::Terminal(error))
        }
        Err(bmux_tui_runtime::RuntimeError::Presenter { error, .. }) => {
            Err(RunError::Terminal(error.to_string()))
        }
    }
}

fn set_target(target: &WakeTarget, handle: Option<RuntimeHandle<Msg>>) {
    if let Ok(mut current) = target.lock() {
        *current = handle;
    }
}

/// The host's redraw callback: a coalesced [`Msg::Wake`] to the current terminal session.
fn wake(target: WakeTarget, pending: Arc<AtomicBool>) -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(move || {
        if pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let sent = target
            .lock()
            .ok()
            .and_then(|handle| handle.as_ref().map(|handle| handle.try_send(Msg::Wake)))
            .is_some_and(|result| result.is_ok());
        if !sent {
            pending.store(false, Ordering::Release);
        }
    })
}

/// Run `command` attached to the terminal.
fn run_external(command: &ExternalCommand) -> Result<(), String> {
    let status = std::process::Command::new(&command.program)
        .args(&command.args)
        .status()
        .map_err(|error| format!("{}: {error}", command.program))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} exited with {status}", command.program))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::harness::ScriptedHost;
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicUsize;

    fn options(
        path: PathBuf,
        config: LoadedConfig,
        host: Option<Arc<dyn ReviewHost>>,
    ) -> RunOptions {
        RunOptions {
            path,
            config,
            host,
            entry: Entry::Home,
            bcode_program: None,
        }
    }

    #[test]
    fn invalid_keybinding_overrides_fail_before_the_terminal_is_used() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = LoadedConfig {
            keybindings: BTreeMap::from([(
                crate::keymap::HOME_SCOPE.to_owned(),
                BTreeMap::from([(
                    "not a key stroke".to_owned(),
                    "bcode.code_review.home_down".to_owned(),
                )]),
            )]),
            ..LoadedConfig::default()
        };
        let host: Arc<dyn ReviewHost> = Arc::new(ScriptedHost::new());
        let result = build_app(options(dir.path().to_path_buf(), config, Some(host)));
        assert!(matches!(result, Err(RunError::Keymap(_))));
    }

    #[test]
    fn a_built_app_paints_home_with_the_given_host_and_keeps_config_diagnostics() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = LoadedConfig {
            diagnostics: vec!["tui.diff_viewer: unknown layout".to_owned()],
            ..LoadedConfig::default()
        };
        let host = Arc::new(ScriptedHost::new());
        let mut app = build_app(options(
            dir.path().to_path_buf(),
            config,
            Some(Arc::clone(&host) as Arc<dyn ReviewHost>),
        ))
        .expect("app builds");
        assert_eq!(app.diagnostics(), ["tui.diff_viewer: unknown layout"]);
        assert_eq!(app.review().route_name(), crate::RouteName::Home);
        let rows = render_text(&mut app, 60, 12);
        assert_eq!(rows.len(), 12);
        assert!(rows.iter().any(|row| !row.is_empty()), "{rows:#?}");
        // Starting the loop starts loading home through the given host.
        let _ = app.poll();
        assert!(host.spawned_tasks() > 0);
    }

    #[test]
    fn standalone_host_requests_a_frame_from_the_connected_loop_when_work_finishes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = runtime.enter();
        let host = StandaloneHost::new(BcodeAccess::Installed, HostTheme::fallback());
        let frames = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&frames);
        host.connect_redraw(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        let (done, finished) = tokio::sync::oneshot::channel();
        host.spawn(Box::pin(async move {
            let _ = done.send(());
        }));
        runtime.block_on(async {
            finished.await.expect("task ran");
            tokio::task::yield_now().await;
        });
        assert_eq!(frames.load(Ordering::SeqCst), 1);
        host.request_redraw();
        assert_eq!(frames.load(Ordering::SeqCst), 2);
    }
}
