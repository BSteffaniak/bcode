//! Deterministic multi-event scenario harness for plugin TUI surfaces.
//!
//! * [`ScriptedHost`] implements [`PluginTuiHost`] with the plugin's own manifest keymap
//!   defaults, a captured clipboard, counted redraw requests, and captured spawned tasks that
//!   run only when the harness asks ([`ScriptedHost::run_until_idle`]).
//! * [`Scenario`] owns a surface, the host, and a tokio current-thread runtime. It routes input
//!   the way the Bcode host does (focused editor first, then the surface), settles async work,
//!   and renders frames into a BMUX test buffer at an explicit terminal size, with no
//!   presentation theme (the surface's own default styling) unless one is set.
//! * [`Screen`] is a rendered frame with row text, full text, cell styles, and text lookup.
//! * [`paint`] renders any closure painting into a [`PaintCx`] for component-level tests.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bcode_plugin_sdk::tui::keymap::KeymapRegistry;
use bcode_plugin_sdk::tui::keymap::controls::resolve_registry;
use bcode_plugin_sdk::tui::{
    PluginTask, PluginTuiAction, PluginTuiEventResult, PluginTuiHost, PluginTuiHostError,
    PluginTuiSurface, PluginTuiTheme,
};
use bmux_keyboard::{KeyStroke, parse_key_stroke};
use bmux_tui::buffer::{Buffer, Cell};
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect, Size};
use bmux_tui::paint::PaintCx;
use bmux_tui::style::Style;

/// Upper bound on settle rounds before the harness reports a livelock.
const MAX_SETTLE_ROUNDS: usize = 64;
/// Upper bound on one round of captured async work.
const TASK_ROUND_TIMEOUT: Duration = Duration::from_secs(10);

type BlockingTask = Box<dyn FnOnce() + Send + 'static>;

/// A [`PluginTuiHost`] whose effects are captured for assertions.
pub struct ScriptedHost {
    keymap: KeymapRegistry,
    clipboard: Mutex<Vec<String>>,
    redraws: AtomicUsize,
    tasks: Mutex<VecDeque<PluginTask>>,
    blocking: Mutex<VecDeque<BlockingTask>>,
    spawned: AtomicUsize,
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
    /// Host using the code review manifest keymap defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Host using the manifest keymap defaults plus `scope -> (stroke -> action)` overrides.
    /// An empty action string unbinds the stroke.
    ///
    /// # Panics
    ///
    /// Panics when the manifest declarations or overrides are invalid.
    #[must_use]
    pub fn with_overrides(overrides: BTreeMap<String, BTreeMap<String, String>>) -> Self {
        Self {
            keymap: resolve_registry(crate::keybindings::actions(), overrides)
                .expect("manifest keymap resolves"),
            clipboard: Mutex::new(Vec::new()),
            redraws: AtomicUsize::new(0),
            tasks: Mutex::new(VecDeque::new()),
            blocking: Mutex::new(VecDeque::new()),
            spawned: AtomicUsize::new(0),
        }
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

    /// Total redraw requests received through [`PluginTuiHost::request_redraw`].
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
            if !tasks.is_empty() {
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
        }
        panic!("scripted tasks kept spawning for {MAX_SETTLE_ROUNDS} rounds");
    }
}

impl PluginTuiHost for ScriptedHost {
    fn spawn(&self, task: PluginTask) {
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

    fn copy_text(&self, text: String) -> Result<(), PluginTuiHostError> {
        self.clipboard.lock().expect("clipboard lock").push(text);
        Ok(())
    }

    fn keymap_action(&self, scope: &str, stroke: KeyStroke) -> Option<String> {
        self.keymap.action_for_key(scope, stroke).map(str::to_owned)
    }

    fn keymap_is_overridden(&self, scope: &str, stroke: KeyStroke) -> bool {
        self.keymap.is_overridden_key(scope, stroke)
    }

    fn keymap_keys(&self, scope: &str, action: &str) -> Vec<String> {
        self.keymap
            .keys_for_action(scope, action)
            .into_iter()
            .map(str::to_owned)
            .collect()
    }
}

/// One rendered terminal frame.
#[derive(Debug, Clone)]
pub struct Screen {
    buffer: Buffer,
}

impl Screen {
    /// Wrap a painted buffer.
    #[must_use]
    pub const fn new(buffer: Buffer) -> Self {
        Self { buffer }
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
    {
        let mut frame = Frame::new(&mut buffer);
        let mut cx = PaintCx::new(&mut frame);
        draw(area, &mut cx);
    }
    Screen::new(buffer)
}

/// A surface driven through scripted events and frames.
pub struct Scenario<S: PluginTuiSurface> {
    runtime: tokio::runtime::Runtime,
    host: ScriptedHost,
    surface: S,
    size: Size,
    theme: Option<PluginTuiTheme>,
    actions: Vec<PluginTuiAction>,
}

impl<S: PluginTuiSurface> Scenario<S> {
    /// Drive `surface` at the given terminal size with the default [`ScriptedHost`].
    #[must_use]
    pub fn new(surface: S, width: u16, height: u16) -> Self {
        Self::with_host(surface, ScriptedHost::new(), width, height)
    }

    /// Drive `surface` with an explicit host.
    ///
    /// # Panics
    ///
    /// Panics when the tokio runtime cannot be built.
    #[must_use]
    pub fn with_host(surface: S, host: ScriptedHost, width: u16, height: u16) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        Self {
            runtime,
            host,
            surface,
            size: Size::new(width, height),
            theme: None,
            actions: Vec::new(),
        }
    }

    /// The captured host.
    #[must_use]
    pub const fn host(&self) -> &ScriptedHost {
        &self.host
    }

    /// The driven surface.
    #[must_use]
    pub const fn surface(&self) -> &S {
        &self.surface
    }

    /// The driven surface, mutably.
    pub const fn surface_mut(&mut self) -> &mut S {
        &mut self.surface
    }

    /// Current terminal size.
    #[must_use]
    pub const fn size(&self) -> Size {
        self.size
    }

    /// Presentation theme passed to the surface on later frames; `None` renders unthemed.
    pub fn set_theme(&mut self, theme: Option<&PluginTuiTheme>) {
        self.theme = theme.copied();
    }

    /// Non-`None` actions returned by the surface so far, oldest first.
    #[must_use]
    pub fn actions(&self) -> &[PluginTuiAction] {
        &self.actions
    }

    /// Route one event as the Bcode host does: the focused editor first, then the surface when
    /// the editor ignores it. Resize events also update the frame size.
    pub fn send(&mut self, event: &Event) -> PluginTuiEventResult {
        if let Some(size) = event.resize_size() {
            self.size = size;
        }
        let editor = self.surface.focused_text_field_event(event, &self.host);
        let result = if editor.outcome == EventOutcome::Ignored {
            self.surface.handle_event_result(event, &self.host)
        } else {
            editor
        };
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
    pub fn key(&mut self, stroke: &str) -> PluginTuiEventResult {
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
    pub fn paste(&mut self, text: &str) -> PluginTuiEventResult {
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
    pub fn mouse(&mut self, kind: MouseEventKind, x: u16, y: u16) -> PluginTuiEventResult {
        self.send(&Event::Mouse(MouseEvent::new(kind, Point::new(x, y))))
    }

    /// Resize the terminal; the next frame renders at the new size.
    pub fn resize(&mut self, width: u16, height: u16) -> PluginTuiEventResult {
        self.send(&Event::Resize(Size::new(width, height)))
    }

    /// Run captured tasks and poll the surface until neither produces more work.
    ///
    /// # Panics
    ///
    /// Panics when work does not settle within the harness round bound.
    pub fn settle(&mut self) {
        for _ in 0..MAX_SETTLE_ROUNDS {
            self.host.run_until_idle(&self.runtime);
            let action = self.surface.poll(&self.host);
            let idle = action == PluginTuiAction::None && self.host.pending_tasks() == 0;
            self.record(action);
            if idle {
                return;
            }
        }
        panic!("surface did not settle within {MAX_SETTLE_ROUNDS} rounds");
    }

    /// Await the surface's queued effect drain once (may contact services the surface uses).
    pub fn drain_effects(&mut self) {
        let action = self
            .runtime
            .block_on(self.surface.drain_effects(&self.host));
        self.record(action);
    }

    /// Render one frame at the current size without settling async work.
    pub fn render(&mut self) -> Screen {
        let area = Rect::new(0, 0, self.size.width, self.size.height);
        self.surface.prepare_keymap(&self.host);
        let surface = &mut self.surface;
        let host = &self.host;
        let theme = self.theme.as_ref();
        paint(area.width, area.height, |area, cx| {
            surface.render_with_host(area, cx, theme, host);
        })
    }

    /// Settle async work, then render one frame.
    pub fn frame(&mut self) -> Screen {
        self.settle();
        self.render()
    }

    fn record(&mut self, action: PluginTuiAction) {
        if action != PluginTuiAction::None {
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
        use std::sync::Arc;
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
        host.copy_text("copied".into()).unwrap();
        assert_eq!(host.redraw_requests(), 1);
        assert_eq!(host.clipboard(), vec!["copied".to_owned()]);
    }

    #[test]
    fn scripted_host_resolves_manifest_defaults_and_overrides() {
        let scope = crate::keybindings::REVIEW;
        let next = parse_key_stroke("n").unwrap();
        let host = ScriptedHost::new();
        assert_eq!(
            host.keymap_action(scope, next).as_deref(),
            Some("bcode.code_review.next_file")
        );
        let host = ScriptedHost::with_overrides(BTreeMap::from([(
            scope.to_owned(),
            BTreeMap::from([("n".to_owned(), String::new())]),
        )]));
        assert_eq!(host.keymap_action(scope, next), None);
        assert!(host.keymap_is_overridden(scope, next));
    }

    /// Minimal surface that logs routed input and paints the log, one entry per row.
    #[derive(Default)]
    struct EchoSurface {
        log: Vec<String>,
        task_done: std::sync::Arc<std::sync::atomic::AtomicBool>,
        task_reported: bool,
        drained: bool,
    }

    impl PluginTuiSurface for EchoSurface {
        fn id(&self) -> &'static str {
            "test.echo"
        }

        fn title(&self) -> &'static str {
            "Echo"
        }

        fn render(&mut self, area: Rect, frame: &mut PaintCx<'_, '_>) {
            let skip = self.log.len().saturating_sub(usize::from(area.height));
            for (row, entry) in (0_i64..).zip(self.log.iter().skip(skip)) {
                frame.write_line(
                    LocalRect::new(0, row, area.width, 1),
                    &Line::raw(entry.as_str()),
                );
            }
        }

        fn handle_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction {
            let entry = match event {
                Event::Key(stroke) if *stroke == parse_key_stroke("q").unwrap() => {
                    return PluginTuiAction::Close { outcome: None };
                }
                Event::Key(stroke) if *stroke == parse_key_stroke("c").unwrap() => {
                    host.copy_text("copied".into()).unwrap();
                    "copy".to_owned()
                }
                Event::Key(stroke) if *stroke == parse_key_stroke("s").unwrap() => {
                    let done = std::sync::Arc::clone(&self.task_done);
                    host.spawn(Box::pin(async move {
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
                _ => return PluginTuiAction::None,
            };
            self.log.push(entry);
            PluginTuiAction::Redraw
        }

        fn poll(&mut self, _host: &dyn PluginTuiHost) -> PluginTuiAction {
            if self.task_done.load(Ordering::SeqCst) && !self.task_reported {
                self.task_reported = true;
                self.log.push("task done".to_owned());
                return PluginTuiAction::Redraw;
            }
            PluginTuiAction::None
        }

        fn drain_effects<'a>(
            &'a mut self,
            _host: &'a dyn PluginTuiHost,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PluginTuiAction> + Send + 'a>>
        {
            Box::pin(async move {
                self.drained = true;
                PluginTuiAction::None
            })
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

        scenario.drain_effects();
        assert!(scenario.surface().drained);
        scenario.key("q");
        assert_eq!(
            scenario.actions().last(),
            Some(&PluginTuiAction::Close { outcome: None })
        );
        scenario.surface_mut().log.clear();
        assert_eq!(scenario.render().text(), "\n\n");
    }
}
