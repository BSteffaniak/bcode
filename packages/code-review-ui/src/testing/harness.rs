//! Deterministic multi-event scenario harness for the review screens.
//!
//! * [`ScriptedHost`] implements [`ReviewHost`] with captured effects: a clipboard, counted
//!   redraw requests, spawned tasks that run only when the harness asks
//!   ([`ScriptedHost::run_until_idle`]), recorded sessions and messages, followed sessions the
//!   test feeds ([`ScriptedHost::session_view`]), and optional fake provider and publisher
//!   plugins. It also carries the keymap (the review's declared defaults plus overrides) and
//!   the diff layout preference a scenario's [`UiContext`] uses.
//! * [`Scenario`] owns a screen, the host, and a tokio current-thread runtime. It routes input,
//!   settles async work, and renders frames into a BMUX test buffer at an explicit terminal
//!   size, with no presentation theme (the screen's own default styling) unless one is set.
//! * [`Screen`] is a rendered frame with row text, full text, cell styles, and text lookup.
//! * [`paint`] renders any closure painting into a [`PaintCx`] for component-level tests.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bcode_code_review::provider::ProviderCallError;
use bcode_code_review_models::REVIEW_SOURCE_ERROR_FAILED;
use bcode_keymap::KeymapRegistry;
use bcode_session_models::{ProjectionWindowRequest, SessionId};
use bcode_session_view::follow::SessionViewFollowUpdate;
use bcode_session_view_models::ReasoningPresentationPolicy;
use bcode_tui_theme::components::ComponentStyles;
use bmux_keyboard::parse_key_stroke;
use bmux_tui::buffer::{Buffer, Cell};
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect, Size};
use bmux_tui::paint::PaintCx;
use bmux_tui::style::Style;

use super::fake_provider::{FAKE_PLUGIN, FakeProvider};
use super::fake_publisher::{FAKE_PUBLISHER_PLUGIN, FakePublisher};
use crate::action::{Action, EventResult};
use crate::config::DiffLayout;
use crate::context::UiContext;
use crate::host::{
    BlockingTask, ExternalCommand, HostError, HostFuture, HostTask, ReviewHost, SessionFollow,
};
use crate::theme::HostTheme;
use crate::view::View;

/// Upper bound on settle rounds before the harness reports a livelock.
const MAX_SETTLE_ROUNDS: usize = 64;
/// Upper bound on one round of captured async work.
const TASK_ROUND_TIMEOUT: Duration = Duration::from_secs(10);
/// Updates buffered per followed session.
const FOLLOW_BUFFER: usize = 16;

/// A [`ReviewHost`] whose effects are captured for assertions.
pub struct ScriptedHost {
    keymap: Arc<KeymapRegistry>,
    diff_layout: Option<DiffLayout>,
    clipboard: Mutex<Vec<String>>,
    redraws: AtomicUsize,
    tasks: Mutex<VecDeque<HostTask>>,
    blocking: Mutex<VecDeque<BlockingTask>>,
    spawned: AtomicUsize,
    session_views: Mutex<BTreeMap<String, tokio::sync::mpsc::Sender<SessionViewFollowUpdate>>>,
    sessions: Mutex<Vec<(String, String)>>,
    messages: Mutex<Vec<(String, String)>>,
    session_failure: Mutex<Option<String>>,
    permissions: Mutex<Vec<(String, bool, bool)>>,
    cancels: Mutex<Vec<String>>,
    delayed_redraws: AtomicUsize,
    provider: OnceLock<FakeProvider>,
    publisher: OnceLock<FakePublisher>,
}

impl std::fmt::Debug for ScriptedHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScriptedHost")
            .field("redraws", &self.redraw_requests())
            .field("pending_tasks", &self.pending_tasks())
            .finish_non_exhaustive()
    }
}

impl Default for ScriptedHost {
    fn default() -> Self {
        Self::with_overrides(BTreeMap::new())
    }
}

impl ScriptedHost {
    /// Host using the review's declared keymap defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Host using the declared keymap defaults plus `scope -> (stroke -> action)` overrides.
    /// An empty action string unbinds the stroke.
    ///
    /// # Panics
    ///
    /// Panics when the declarations or overrides are invalid.
    #[must_use]
    pub fn with_overrides(overrides: BTreeMap<String, BTreeMap<String, String>>) -> Self {
        Self {
            keymap: Arc::new(crate::keymap::registry(overrides).expect("declared keymap resolves")),
            diff_layout: None,
            clipboard: Mutex::new(Vec::new()),
            redraws: AtomicUsize::new(0),
            tasks: Mutex::new(VecDeque::new()),
            blocking: Mutex::new(VecDeque::new()),
            spawned: AtomicUsize::new(0),
            session_views: Mutex::new(BTreeMap::new()),
            sessions: Mutex::default(),
            messages: Mutex::default(),
            session_failure: Mutex::default(),
            permissions: Mutex::default(),
            cancels: Mutex::default(),
            delayed_redraws: AtomicUsize::new(0),
            provider: OnceLock::new(),
            publisher: OnceLock::new(),
        }
    }

    /// Fail creating sessions with `message` from now on (`None` succeeds again).
    ///
    /// # Panics
    ///
    /// Panics when the failure lock is poisoned.
    pub fn fail_sessions(&self, message: Option<&str>) {
        *self.session_failure.lock().expect("failure lock") = message.map(str::to_owned);
    }

    /// Offer `layout` as bcode's diff layout preference.
    #[must_use]
    pub const fn with_diff_layout(mut self, layout: DiffLayout) -> Self {
        self.diff_layout = Some(layout);
        self
    }

    /// Serve `provider` as the only review-source provider plugin.
    ///
    /// # Panics
    ///
    /// Panics when a provider is already set.
    pub fn set_provider(&self, provider: FakeProvider) {
        assert!(self.provider.set(provider).is_ok(), "provider already set");
    }

    /// Serve `publisher` as the only external review publisher and importer plugin.
    ///
    /// # Panics
    ///
    /// Panics when a publisher is already set.
    pub fn set_publisher(&self, publisher: FakePublisher) {
        assert!(
            self.publisher.set(publisher).is_ok(),
            "publisher already set"
        );
    }

    /// The fake provider, when set.
    #[must_use]
    pub fn provider(&self) -> Option<&FakeProvider> {
        self.provider.get()
    }

    /// The fake publisher, when set.
    #[must_use]
    pub fn publisher(&self) -> Option<&FakePublisher> {
        self.publisher.get()
    }

    /// The UI context screens use with this host.
    #[must_use]
    pub fn context(self: &Arc<Self>) -> UiContext {
        UiContext::new(
            Arc::clone(self) as Arc<dyn ReviewHost>,
            Arc::clone(&self.keymap),
            self.diff_layout,
        )
    }

    /// Text written to the clipboard, oldest first.
    ///
    /// # Panics
    ///
    /// Panics when the clipboard lock is poisoned.
    #[must_use]
    pub fn clipboard(&self) -> Vec<String> {
        self.clipboard.lock().expect("clipboard lock").clone()
    }

    /// Sessions created so far: `(id, title)`.
    ///
    /// # Panics
    ///
    /// Panics when the lock is poisoned.
    #[must_use]
    pub fn sessions(&self) -> Vec<(String, String)> {
        self.sessions.lock().expect("sessions lock").clone()
    }

    /// Messages sent so far: `(session id, text)`.
    ///
    /// # Panics
    ///
    /// Panics when the lock is poisoned.
    #[must_use]
    pub fn messages(&self) -> Vec<(String, String)> {
        self.messages.lock().expect("messages lock").clone()
    }

    /// Permission answers sent so far: `(permission id, approved, remember)`.
    ///
    /// # Panics
    ///
    /// Panics when the lock is poisoned.
    #[must_use]
    pub fn permission_answers(&self) -> Vec<(String, bool, bool)> {
        self.permissions.lock().expect("permissions lock").clone()
    }

    /// Sessions whose running turn was stopped, in order.
    ///
    /// # Panics
    ///
    /// Panics when the lock is poisoned.
    #[must_use]
    pub fn cancelled_turns(&self) -> Vec<String> {
        self.cancels.lock().expect("cancels lock").clone()
    }

    /// Frames requested for later through [`ReviewHost::request_redraw_after`] (never
    /// delivered: scenarios render explicitly).
    #[must_use]
    pub fn delayed_redraw_requests(&self) -> usize {
        self.delayed_redraws.load(Ordering::SeqCst)
    }

    /// Sender of view updates for a session a screen follows.
    ///
    /// # Panics
    ///
    /// Panics when the lock is poisoned.
    #[must_use]
    pub fn session_view(
        &self,
        session: &str,
    ) -> Option<tokio::sync::mpsc::Sender<SessionViewFollowUpdate>> {
        self.session_views
            .lock()
            .expect("session views lock")
            .get(session)
            .cloned()
    }

    /// Total redraw requests received through [`ReviewHost::request_redraw`].
    #[must_use]
    pub fn redraw_requests(&self) -> usize {
        self.redraws.load(Ordering::SeqCst)
    }

    /// Total tasks spawned so far (async and blocking).
    #[must_use]
    pub fn spawned_tasks(&self) -> usize {
        self.spawned.load(Ordering::SeqCst)
    }

    /// Tasks captured but not yet executed.
    ///
    /// # Panics
    ///
    /// Panics when a task queue lock is poisoned.
    #[must_use]
    pub fn pending_tasks(&self) -> usize {
        self.tasks.lock().expect("task lock").len()
            + self.blocking.lock().expect("blocking lock").len()
    }

    /// Execute one round of the captured async tasks and leave blocking jobs queued, as if work
    /// on the host's blocking pool (such as syntax highlighting) had not finished yet. Returns
    /// the number of executed tasks.
    ///
    /// # Panics
    ///
    /// Panics when a task panics or the round exceeds the harness timeout.
    pub fn run_async_round(&self, runtime: &tokio::runtime::Runtime) -> usize {
        let tasks: Vec<_> = self.tasks.lock().expect("task lock").drain(..).collect();
        let executed = tasks.len();
        Self::await_tasks(runtime, tasks);
        executed
    }

    fn await_tasks(runtime: &tokio::runtime::Runtime, tasks: Vec<HostTask>) {
        if tasks.is_empty() {
            return;
        }
        runtime.block_on(async {
            let handles: Vec<_> = tasks.into_iter().map(tokio::spawn).collect();
            tokio::time::timeout(TASK_ROUND_TIMEOUT, async {
                for handle in handles {
                    handle.await.expect("scripted task panicked");
                }
            })
            .await
            .expect("scripted tasks did not finish within the harness timeout");
        });
    }

    /// Execute captured work until no task remains, including tasks spawned by tasks.
    ///
    /// Blocking tasks run inline in spawn order. Async tasks captured in one round are spawned
    /// in order onto `runtime` and awaited together. Returns the number of executed tasks.
    ///
    /// # Panics
    ///
    /// Panics when a task panics, a round exceeds the harness timeout, or work keeps spawning
    /// beyond the harness round bound.
    pub fn run_until_idle(&self, runtime: &tokio::runtime::Runtime) -> usize {
        let mut executed = 0;
        for _ in 0..MAX_SETTLE_ROUNDS {
            let blocking: Vec<_> = self
                .blocking
                .lock()
                .expect("blocking lock")
                .drain(..)
                .collect();
            let tasks: Vec<_> = self.tasks.lock().expect("task lock").drain(..).collect();
            if blocking.is_empty() && tasks.is_empty() {
                return executed;
            }
            executed += blocking.len() + tasks.len();
            for job in blocking {
                job();
            }
            Self::await_tasks(runtime, tasks);
        }
        panic!("scripted tasks kept spawning for {MAX_SETTLE_ROUNDS} rounds");
    }
}

fn plugin_error(message: String) -> HostError {
    HostError {
        code: REVIEW_SOURCE_ERROR_FAILED.to_owned(),
        message,
    }
}

impl ReviewHost for ScriptedHost {
    fn theme(&self) -> HostTheme {
        HostTheme::fallback()
    }

    fn theme_generation(&self) -> u64 {
        0
    }

    fn create_session(&self, title: String, _working_directory: PathBuf) -> HostFuture<String> {
        let failure = self.session_failure.lock().expect("failure lock").clone();
        if let Some(message) = failure {
            return Box::pin(async move { Err(HostError::failed(message)) });
        }
        let id = SessionId::new().to_string();
        self.sessions
            .lock()
            .expect("sessions lock")
            .push((id.clone(), title));
        Box::pin(async move { Ok(id) })
    }

    fn send_message(&self, session_id: String, text: String) -> HostFuture<()> {
        self.messages
            .lock()
            .expect("messages lock")
            .push((session_id, text));
        Box::pin(async { Ok(()) })
    }

    fn resolve_permission(
        &self,
        permission_id: String,
        approved: bool,
        remember: bool,
    ) -> HostFuture<()> {
        self.permissions.lock().expect("permissions lock").push((
            permission_id,
            approved,
            remember,
        ));
        Box::pin(async { Ok(()) })
    }

    fn cancel_turn(&self, session_id: String) -> HostFuture<()> {
        self.cancels.lock().expect("cancels lock").push(session_id);
        Box::pin(async { Ok(()) })
    }

    fn follow_session(
        &self,
        session_id: SessionId,
        _projection: ProjectionWindowRequest,
        _reasoning_policy: ReasoningPresentationPolicy,
    ) -> Result<SessionFollow, HostError> {
        let (sender, updates) = tokio::sync::mpsc::channel(FOLLOW_BUFFER);
        self.session_views
            .lock()
            .expect("session views lock")
            .insert(session_id.to_string(), sender);
        Ok(SessionFollow { updates })
    }

    fn open_session_command(&self, session_id: &str) -> Option<ExternalCommand> {
        Some(ExternalCommand {
            program: "bcode".to_owned(),
            args: vec!["tui".to_owned(), session_id.to_owned()],
        })
    }

    fn plugins_serving(&self, interface_id: String) -> HostFuture<Vec<String>> {
        let plugins = if self.provider.get().is_some()
            && interface_id == bcode_code_review_models::REVIEW_SOURCE_INTERFACE_ID
        {
            vec![FAKE_PLUGIN.to_owned()]
        } else if self.publisher.get().is_some()
            && (interface_id == bcode_code_review_models::REVIEW_PUBLISHER_INTERFACE_ID
                || interface_id == bcode_code_review_models::REVIEW_IMPORTER_INTERFACE_ID)
        {
            vec![FAKE_PUBLISHER_PLUGIN.to_owned()]
        } else {
            Vec::new()
        };
        Box::pin(async move { Ok(plugins) })
    }

    fn invoke_plugin(
        &self,
        plugin_id: String,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> HostFuture<Vec<u8>> {
        let answer = self
            .provider
            .get()
            .filter(|_| plugin_id == FAKE_PLUGIN)
            .map(|provider| provider.answer(&operation, &payload))
            .or_else(|| {
                self.publisher
                    .get()
                    .filter(|_| plugin_id == FAKE_PUBLISHER_PLUGIN)
                    .map(|publisher| publisher.answer(&interface_id, &operation, &payload))
            });
        Box::pin(async move {
            answer
                .unwrap_or_else(|| {
                    Err(ProviderCallError {
                        code: REVIEW_SOURCE_ERROR_FAILED.to_owned(),
                        message: format!("no provider {plugin_id} for {operation}"),
                    })
                })
                .map_err(|error| HostError {
                    code: error.code,
                    message: error.message,
                })
        })
    }

    fn copy_to_clipboard(&self, text: &str) -> Result<(), HostError> {
        self.clipboard
            .lock()
            .expect("clipboard lock")
            .push(text.to_owned());
        Ok(())
    }

    fn open_external(&self, url: &str) -> Result<(), HostError> {
        Err(plugin_error(format!("no opener for {url}")))
    }

    fn spawn(&self, task: HostTask) {
        self.spawned.fetch_add(1, Ordering::SeqCst);
        self.tasks.lock().expect("task lock").push_back(task);
    }

    fn spawn_blocking(&self, task: BlockingTask) {
        self.spawned.fetch_add(1, Ordering::SeqCst);
        self.blocking.lock().expect("blocking lock").push_back(task);
    }

    fn request_redraw(&self) {
        self.redraws.fetch_add(1, Ordering::SeqCst);
    }

    fn request_redraw_after(&self, _delay: Duration) {
        self.delayed_redraws.fetch_add(1, Ordering::SeqCst);
    }
}

/// One rendered terminal frame.
#[derive(Debug, Clone)]
pub struct Screen {
    buffer: Buffer,
    hits: Vec<bmux_tui::hit::HitRegion>,
    selection: bmux_tui::selection::SelectionScene,
}

impl Screen {
    /// Selection scopes and text fragments registered while painting this frame.
    #[must_use]
    pub const fn selection(&self) -> &bmux_tui::selection::SelectionScene {
        &self.selection
    }

    /// Interaction regions registered while painting this frame.
    #[must_use]
    pub fn hits(&self) -> &[bmux_tui::hit::HitRegion] {
        &self.hits
    }

    /// Frame width in cells.
    #[must_use]
    pub const fn width(&self) -> u16 {
        self.buffer.area().width
    }

    /// Frame height in rows.
    #[must_use]
    pub const fn height(&self) -> u16 {
        self.buffer.area().height
    }

    /// Symbols of row `y` with trailing whitespace removed; empty when out of range.
    #[must_use]
    pub fn row_text(&self, y: u16) -> String {
        self.buffer
            .row_symbols(y)
            .map(|row| row.trim_end().to_owned())
            .unwrap_or_default()
    }

    /// Symbols of the cells of row `y` inside `area`, trailing whitespace removed; empty when
    /// the row is outside `area`.
    #[must_use]
    pub fn text_in(&self, area: Rect, y: u16) -> String {
        if !(area.y..area.bottom()).contains(&y) {
            return String::new();
        }
        (area.x..area.right())
            .filter_map(|x| self.cell(x, y).map(|cell| cell.symbol.as_str()))
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    /// Cell column where `needle` starts in row `y`, comparing cell symbols (so wide graphemes
    /// count as the cells they cover).
    #[must_use]
    pub fn column_of(&self, y: u16, needle: &str) -> Option<u16> {
        let mut row = String::new();
        let mut starts = Vec::new();
        for x in 0..self.width() {
            starts.push((row.len(), x));
            if let Some(cell) = self.cell(x, y) {
                row.push_str(&cell.symbol);
            }
        }
        let byte = row.find(needle)?;
        starts
            .iter()
            .rev()
            .find(|(start, _)| *start <= byte)
            .map(|(_, x)| *x)
    }

    /// All rows joined with newlines, trailing whitespace removed per row.
    #[must_use]
    pub fn text(&self) -> String {
        (0..self.height())
            .map(|y| self.row_text(y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether any row contains `needle`.
    #[must_use]
    pub fn contains(&self, needle: &str) -> bool {
        (0..self.height()).any(|y| self.row_text(y).contains(needle))
    }

    /// Cell at `(x, y)`.
    #[must_use]
    pub fn cell(&self, x: u16, y: u16) -> Option<&Cell> {
        self.buffer.get(Point::new(x, y))
    }

    /// Style of the cell at `(x, y)`.
    ///
    /// # Panics
    ///
    /// Panics when the position is outside the frame.
    #[must_use]
    pub fn style_at(&self, x: u16, y: u16) -> Style {
        self.cell(x, y)
            .unwrap_or_else(|| panic!("cell ({x}, {y}) outside {:?}", self.buffer.area()))
            .style
    }

    /// First cell position whose row text at that column starts with `needle`, scanning rows
    /// top to bottom. Columns are terminal cells, so wide graphemes are handled.
    #[must_use]
    pub fn find(&self, needle: &str) -> Option<Point> {
        let area = self.buffer.area();
        for y in area.y..area.bottom() {
            let mut row = String::new();
            let mut starts = Vec::with_capacity(usize::from(area.width));
            for x in area.x..area.right() {
                starts.push((row.len(), x));
                if let Some(cell) = self.buffer.get(Point::new(x, y)) {
                    row.push_str(&cell.symbol);
                }
            }
            if let Some(byte) = row.find(needle) {
                let x = starts
                    .iter()
                    .rev()
                    .find(|(start, _)| *start <= byte)
                    .map_or(area.x, |(_, x)| *x);
                return Some(Point::new(x, y));
            }
        }
        None
    }
}

/// Paint a closure into a fresh buffer of the given size.
pub fn paint(width: u16, height: u16, draw: impl FnOnce(Rect, &mut PaintCx<'_, '_>)) -> Screen {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    let (hits, selection) = {
        let mut frame = Frame::new(&mut buffer);
        let mut cx = PaintCx::new(&mut frame);
        draw(area, &mut cx);
        (frame.hits().regions().to_vec(), frame.selection().clone())
    };
    Screen {
        buffer,
        hits,
        selection,
    }
}

/// A screen driven through scripted events and frames.
pub struct Scenario<S: View> {
    runtime: tokio::runtime::Runtime,
    host: Arc<ScriptedHost>,
    ui: UiContext,
    surface: S,
    size: Size,
    theme: Option<ComponentStyles>,
    actions: Vec<Action>,
}

impl<S: View> Scenario<S> {
    /// Drive `surface` at the given terminal size with the default [`ScriptedHost`].
    #[must_use]
    pub fn new(surface: S, width: u16, height: u16) -> Self {
        Self::with_host(surface, ScriptedHost::new(), width, height)
    }

    /// Drive `surface` with an explicit host.
    #[must_use]
    pub fn with_host(surface: S, host: ScriptedHost, width: u16, height: u16) -> Self {
        Self::with_shared_host(surface, Arc::new(host), width, height)
    }

    /// Drive `surface` with a host shared with its data services.
    ///
    /// # Panics
    ///
    /// Panics when the tokio runtime cannot be built.
    #[must_use]
    pub fn with_shared_host(surface: S, host: Arc<ScriptedHost>, width: u16, height: u16) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        let ui = host.context();
        Self {
            runtime,
            host,
            ui,
            surface,
            size: Size::new(width, height),
            theme: None,
            actions: Vec::new(),
        }
    }

    /// The captured host.
    #[must_use]
    pub fn host(&self) -> &ScriptedHost {
        &self.host
    }

    /// The driven screen.
    #[must_use]
    pub const fn surface(&self) -> &S {
        &self.surface
    }

    /// The driven screen, mutably.
    pub const fn surface_mut(&mut self) -> &mut S {
        &mut self.surface
    }

    /// Current terminal size.
    #[must_use]
    pub const fn size(&self) -> Size {
        self.size
    }

    /// Component styles passed to the screen on later frames; `None` renders unthemed.
    pub fn set_theme(&mut self, theme: Option<&ComponentStyles>) {
        self.theme = theme.copied();
    }

    /// Non-`None` actions returned by the screen so far, oldest first.
    #[must_use]
    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    /// Route one event to the screen. Resize events also update the frame size.
    pub fn send(&mut self, event: &Event) -> EventResult {
        if let Some(size) = event.resize_size() {
            self.size = size;
        }
        let result = self.surface.handle_event(event, &self.ui);
        self.record(result.action.clone());
        result
    }

    /// Route several events without rendering in between.
    pub fn send_all<'a>(&mut self, events: impl IntoIterator<Item = &'a Event>) {
        for event in events {
            self.send(event);
        }
    }

    /// Send one key stroke written in keymap syntax, such as `"n"` or `"ctrl+d"`.
    ///
    /// # Panics
    ///
    /// Panics when the stroke does not parse.
    pub fn key(&mut self, stroke: &str) -> EventResult {
        let stroke = parse_key_stroke(stroke).unwrap_or_else(|error| panic!("{stroke}: {error}"));
        self.send(&Event::Key(stroke))
    }

    /// Send several key strokes without rendering in between.
    pub fn keys(&mut self, strokes: &[&str]) {
        for stroke in strokes {
            self.key(stroke);
        }
    }

    /// Send a bracketed paste.
    pub fn paste(&mut self, text: &str) -> EventResult {
        self.send(&Event::Paste(text.to_owned()))
    }

    /// Send a left-button press and release at `(x, y)`.
    pub fn click(&mut self, x: u16, y: u16) {
        let position = Point::new(x, y);
        self.send(&Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            position,
        )));
        self.send(&Event::Mouse(MouseEvent::new(
            MouseEventKind::Up(MouseButton::Left),
            position,
        )));
    }

    /// Send one mouse event of `kind` at `(x, y)`.
    pub fn mouse(&mut self, kind: MouseEventKind, x: u16, y: u16) -> EventResult {
        self.send(&Event::Mouse(MouseEvent::new(kind, Point::new(x, y))))
    }

    /// Resize the terminal; the next frame renders at the new size.
    pub fn resize(&mut self, width: u16, height: u16) -> EventResult {
        self.send(&Event::Resize(Size::new(width, height)))
    }

    /// Run captured tasks and poll the screen until neither produces more work.
    ///
    /// # Panics
    ///
    /// Panics when work does not settle within the harness round bound.
    pub fn settle(&mut self) {
        for _ in 0..MAX_SETTLE_ROUNDS {
            self.host.run_until_idle(&self.runtime);
            let action = self.surface.poll(&self.ui);
            let idle = action == Action::None && self.host.pending_tasks() == 0;
            self.record(action);
            if idle {
                return;
            }
        }
        panic!("screen did not settle within {MAX_SETTLE_ROUNDS} rounds");
    }

    /// Run one round of async tasks (blocking jobs stay queued, as if still running on the
    /// host's blocking pool) and poll the screen once.
    pub fn step_async(&mut self) {
        self.host.run_async_round(&self.runtime);
        let action = self.surface.poll(&self.ui);
        self.record(action);
    }

    /// Render one frame at the current size without settling async work.
    pub fn render(&mut self) -> Screen {
        let area = Rect::new(0, 0, self.size.width, self.size.height);
        self.surface.prepare_keymap(&self.ui);
        let surface = &mut self.surface;
        let ui = &self.ui;
        let theme = self.theme.as_ref();
        paint(area.width, area.height, |area, cx| {
            surface.render(area, cx, theme, ui);
        })
    }

    /// Paint one frame at the terminal's size into a prepared presentation with full damage,
    /// without encoding it. Pass the result to
    /// [`bmux_tui::terminal::Terminal::draw_prepared_with_output_hooks`] to diff-encode it
    /// against the terminal's retained frame.
    pub fn prepare<W: std::io::Write>(
        &mut self,
        terminal: &bmux_tui::terminal::Terminal<W>,
    ) -> Option<bmux_tui::presentation::PreparedPresentation> {
        let area = terminal.area();
        self.size = Size::new(area.width, area.height);
        self.surface.prepare_keymap(&self.ui);
        let surface = &mut self.surface;
        let ui = &self.ui;
        let theme = self.theme.as_ref();
        terminal.prepare_frame(bmux_tui::damage::Damage::Full, |cx| {
            surface.render(area, cx, theme, ui);
        })
    }

    /// Settle async work, then render one frame.
    pub fn frame(&mut self) -> Screen {
        self.settle();
        self.render()
    }

    fn record(&mut self, action: Action) {
        if action != Action::None {
            self.actions.push(action);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::paint::LocalRect;
    use bmux_tui::style::Color;
    use bmux_tui::text::{Line, Span};

    #[test]
    fn paint_exposes_rows_styles_and_cell_positions_for_wide_text() {
        let style = Style::new().fg(Color::Red);
        let screen = paint(12, 2, |area, cx| {
            cx.write_line(
                LocalRect::new(0, 1, area.width, 1),
                &Line::from_spans(vec![Span::styled("ab漢字cd", style)]),
            );
        });
        assert_eq!(screen.row_text(0), "");
        assert_eq!(screen.row_text(1), "ab漢字cd");
        assert_eq!(screen.find("cd"), Some(Point::new(6, 1)));
        assert_eq!(screen.style_at(6, 1), style);
        assert_eq!(screen.text(), "\nab漢字cd");
    }

    #[test]
    fn scripted_host_runs_nested_tasks_deterministically() {
        let host = Arc::new(ScriptedHost::new());
        let order = Arc::new(Mutex::new(Vec::new()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for index in 0..3 {
            let order = Arc::clone(&order);
            let inner_host = Arc::clone(&host);
            host.spawn(Box::pin(async move {
                order.lock().unwrap().push(index);
                if index == 0 {
                    let order = Arc::clone(&order);
                    inner_host.spawn(Box::pin(async move {
                        order.lock().unwrap().push(10);
                    }));
                }
            }));
        }
        let blocking_order = Arc::clone(&order);
        host.spawn_blocking(Box::new(move || blocking_order.lock().unwrap().push(99)));
        assert_eq!(host.pending_tasks(), 4);
        assert_eq!(host.run_until_idle(&runtime), 5);
        assert_eq!(*order.lock().unwrap(), vec![99, 0, 1, 2, 10]);
        assert_eq!(host.pending_tasks(), 0);
        host.request_redraw();
        host.copy_to_clipboard("copied").unwrap();
        assert_eq!(host.redraw_requests(), 1);
        assert_eq!(host.clipboard(), vec!["copied".to_owned()]);
    }

    #[test]
    fn scripted_host_resolves_declared_defaults_and_overrides() {
        let scope = crate::SCOPE;
        let next = parse_key_stroke("n").unwrap();
        let ui = Arc::new(ScriptedHost::new()).context();
        assert_eq!(
            ui.keymap_action(scope, next).as_deref(),
            Some("bcode.code_review.next_file")
        );
        let ui = Arc::new(ScriptedHost::with_overrides(BTreeMap::from([(
            scope.to_owned(),
            BTreeMap::from([("n".to_owned(), String::new())]),
        )])))
        .context();
        assert_eq!(ui.keymap_action(scope, next), None);
        assert!(ui.keymap_is_overridden(scope, next));
    }

    /// Minimal surface that logs routed input and paints the log, one entry per row.
    #[derive(Default)]
    struct EchoSurface {
        log: Vec<String>,
        task_done: Arc<std::sync::atomic::AtomicBool>,
        task_reported: bool,
    }

    impl View for EchoSurface {
        fn prepare_keymap(&mut self, _ui: &UiContext) {}

        fn render(
            &mut self,
            area: Rect,
            frame: &mut PaintCx<'_, '_>,
            _styles: Option<&ComponentStyles>,
            _ui: &UiContext,
        ) {
            let skip = self.log.len().saturating_sub(usize::from(area.height));
            for (row, entry) in (0_i64..).zip(self.log.iter().skip(skip)) {
                frame.write_line(
                    LocalRect::new(0, row, area.width, 1),
                    &Line::raw(entry.as_str()),
                );
            }
        }

        fn handle_event(&mut self, event: &Event, ui: &UiContext) -> EventResult {
            let entry = match event {
                Event::Key(stroke) if *stroke == parse_key_stroke("q").unwrap() => {
                    return EventResult::from_action(Action::CLOSE);
                }
                Event::Key(stroke) if *stroke == parse_key_stroke("c").unwrap() => {
                    ui.copy_text("copied").unwrap();
                    "copy".to_owned()
                }
                Event::Key(stroke) if *stroke == parse_key_stroke("s").unwrap() => {
                    let done = Arc::clone(&self.task_done);
                    ui.spawn(Box::pin(async move {
                        done.store(true, Ordering::SeqCst);
                    }));
                    "spawn".to_owned()
                }
                Event::Key(stroke) => format!("key {stroke:?}"),
                Event::Paste(text) => format!("paste {text}"),
                Event::Mouse(mouse) => format!(
                    "mouse {:?} {},{}",
                    mouse.kind, mouse.position.x, mouse.position.y
                ),
                Event::Resize(size) => format!("resize {}x{}", size.width, size.height),
                _ => return EventResult::from_action(Action::None),
            };
            self.log.push(entry);
            EventResult::from_action(Action::Redraw)
        }

        fn poll(&mut self, _ui: &UiContext) -> Action {
            if self.task_done.load(Ordering::SeqCst) && !self.task_reported {
                self.task_reported = true;
                self.log.push("task done".to_owned());
                return Action::Redraw;
            }
            Action::None
        }
    }

    #[test]
    fn scenario_routes_event_sequences_settles_tasks_and_resizes() {
        let mut scenario = Scenario::new(EchoSurface::default(), 40, 8);
        let a = Event::Key(parse_key_stroke("a").unwrap());
        scenario.send_all([&a, &a]);
        scenario.paste("hi");
        scenario.click(3, 2);
        scenario.key("c");
        scenario.key("s");
        assert_eq!(scenario.host().pending_tasks(), 1);
        assert_eq!(scenario.host().spawned_tasks(), 1);
        assert!(scenario.render().find("task done").is_none());

        let screen = scenario.frame();
        assert_eq!(scenario.host().pending_tasks(), 0);
        assert_eq!(screen.row_text(2), "paste hi");
        assert!(screen.contains("mouse Down(Left) 3,2"));
        assert!(screen.contains("mouse Up(Left) 3,2"));
        assert_eq!(screen.find("task done"), Some(Point::new(0, 7)));
        assert_eq!(
            screen.cell(0, 7).map(|cell| cell.symbol.as_str()),
            Some("t")
        );
        assert_eq!(scenario.host().clipboard(), vec!["copied".to_owned()]);

        scenario.resize(20, 3);
        assert_eq!(scenario.size(), Size::new(20, 3));
        let narrow = scenario.frame();
        assert_eq!((narrow.width(), narrow.height()), (20, 3));
        assert_eq!(narrow.row_text(2), "resize 20x3");

        scenario.key("q");
        assert_eq!(scenario.actions().last(), Some(&Action::CLOSE));
        scenario.surface_mut().log.clear();
        assert_eq!(scenario.render().text(), "\n\n");
    }
}
