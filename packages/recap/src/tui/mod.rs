//! Recap's terminal shell. All effects enter the shared application adapter.
mod images;
mod settings;
#[cfg(test)]
mod tests;
mod view;

use crate::application::{self, Outcome, Request, Stage, TuiOptions};
use bcode_recap::{
    contracts::{Depth, RecapDocument},
    process::Cancellation,
};
use bmux_keyboard::KeyCode;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::{
    component::{Constraints, EventCx, LayoutCx, LayoutNode},
    crossterm::{CrosstermTerminalGuard, terminal_size},
    event::Event,
    geometry::{Rect, Size},
    interaction::InteractionRouter,
    terminal::Terminal,
};
use bmux_tui_components::{
    button::ButtonState,
    scroll_view::ScrollViewState,
    text_input::{TextInputControl, TextInputPolicy, TextInputState},
};
use bmux_tui_runtime::{
    ImageTerminalPresenter, Program, Runtime, RuntimeConfig, RuntimeEvent, Subscription,
    SubscriptionKey, Update,
};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    time::{Duration, Instant},
};

const FIELDS: [&str; 10] = [
    "source.input",
    "transcript.input",
    "focus.input",
    "config.input",
    "profile.input",
    "export-path.input",
    "settings.search.input",
    "settings.model.input",
    "settings.endpoint.input",
    "settings.auth.input",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Home,
    Reader,
    Diagnostics,
}

struct Active {
    id: u64,
    cancel: Cancellation,
    started: Instant,
    stage: Option<Stage>,
    cancelling: bool,
    task: tokio::task::JoinHandle<()>,
}

enum Message {
    Discovered(
        u64,
        Box<Result<(crate::connections::Discovery, crate::connections::Selection), String>>,
    ),
    Start,
    Tick,
    Progress(u64, Stage),
    Settled(u64, Box<Result<Outcome, String>>),
    Preview(u64, Box<Result<Option<images::PreviewImage>, String>>),
    InputFailed(String),
}

struct App {
    preview: Option<images::PreviewImage>,
    preview_generation: u64,
    image_capabilities: bmux_image::host_caps::HostImageCapabilities,
    preview_task: Option<tokio::task::JoinHandle<()>>,
    preview_sender: Option<tokio::sync::mpsc::Sender<Message>>,
    preview_permit: std::sync::Arc<tokio::sync::Semaphore>,
    settings_state: settings::Settings,
    options: TuiOptions,
    fields: Vec<RefCell<TextInputState>>,
    focused: usize,
    screen: Screen,
    document: Option<RecapDocument>,
    chapter: usize,
    library_offset: usize,
    chapter_offset: usize,
    lines: Vec<bmux_tui::text::Line>,
    scroll: Cell<ScrollViewState>,
    recent: Vec<application::CatalogEntry>,
    cursor: Option<application::CatalogCursor>,
    active: Option<Active>,
    sequence: u64,
    status: String,
    failure_lines: Vec<bmux_tui::text::Line>,
    export: Option<PathBuf>,
    quitting: bool,
    refine: bool,
    drawer: bool,
    depth: Depth,
    palette: view::Palette,
    buttons: Vec<Cell<ButtonState>>,
    router: InteractionRouter,
    layout: RefCell<Option<LayoutNode>>,
    size: Cell<(u16, u16)>,
}

impl App {
    fn new(options: TuiOptions) -> Self {
        let values = [
            options.source.clone().unwrap_or_default(),
            String::new(),
            String::new(),
            options
                .model_config
                .as_ref()
                .map_or_else(String::new, |p| p.display().to_string()),
            options.profile.clone().unwrap_or_default(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ];
        Self {
            preview: None,
            preview_generation: 0,
            image_capabilities: bmux_image::host_caps::HostImageCapabilities::default(),
            preview_task: None,
            preview_sender: None,
            preview_permit: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            settings_state: settings::Settings::default(),
            options,
            fields: values
                .into_iter()
                .map(|text| RefCell::new(TextInputState::new(TextEditBuffer::from_text(text))))
                .collect(),
            focused: 0,
            screen: Screen::Home,
            document: None,
            chapter: 0,
            library_offset: 0,
            chapter_offset: 0,
            lines: Vec::new(),
            scroll: Cell::new(ScrollViewState::default()),
            recent: Vec::new(),
            cursor: None,
            active: None,
            sequence: 0,
            status: "Your next perspective starts here. No model runs until you choose Generate."
                .into(),
            failure_lines: Vec::new(),
            export: None,
            quitting: false,
            refine: false,
            drawer: false,
            depth: Depth::Standard,
            palette: if std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
                view::Palette::monochrome()
            } else {
                view::Palette::dark()
            },
            buttons: (0..80).map(|_| Cell::new(ButtonState::new())).collect(),
            router: InteractionRouter::new(),
            layout: RefCell::new(None),
            size: Cell::new((80, 24)),
        }
    }
    fn text(&self, index: usize) -> String {
        self.fields[index]
            .borrow()
            .buffer()
            .text()
            .trim()
            .to_owned()
    }
    fn admit(&mut self, request: Request) -> Update<Message> {
        if self.active.is_some() {
            return Update::none();
        }
        self.sequence = self.sequence.saturating_add(1);
        let id = self.sequence;
        let cancel = Cancellation::default();
        let self_cancel = cancel.clone();
        let options = self.options.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let task = tokio::spawn(async move {
            let progress = tx.clone();
            let result = application::execute(&options, request, &cancel, &mut |stage| {
                let _ = progress.try_send(Message::Progress(id, stage));
            })
            .await
            .map_err(|error| operation_error(&error));
            let _ = tx.send(Message::Settled(id, Box::new(result))).await;
        });
        self.active = Some(Active {
            id,
            cancel: self_cancel,
            started: Instant::now(),
            stage: None,
            cancelling: false,
            task,
        });
        let mut update = Update::redraw();
        update.subscriptions.push(Subscription::new(
            SubscriptionKey::new("operation"),
            move |sender| async move {
                while let Some(message) = rx.recv().await {
                    if sender.send(message).await.is_err() {
                        break;
                    }
                }
            },
        ));
        update
    }
    fn cancel(&mut self) {
        if let Some(active) = &mut self.active {
            active.cancel.cancel();
            active.cancelling = true;
            self.status =
                "Cancellation requested · waiting for the operation to settle safely".into();
        }
    }
    fn action(&mut self, action: &str) -> Update<Message> {
        if action == "quit" {
            self.quitting = true;
            self.cancel();
            return if self.active.is_none() {
                Update::exit()
            } else {
                Update::redraw()
            };
        }
        if action == "cancel" {
            self.cancel();
            return Update::redraw();
        }
        if self.active.is_some() {
            return Update::none();
        }
        if let Some(update) = self.settings_action(action) {
            return update;
        }
        match action {
            "library-next" => {
                self.library_offset =
                    (self.library_offset + 6).min(self.recent.len().saturating_sub(1));
            }
            "library-prev" => self.library_offset = self.library_offset.saturating_sub(6),
            "chapter-next" => {
                if let Some(document) = &self.document {
                    self.chapter_offset =
                        (self.chapter_offset + 8).min(document.sections.len().saturating_sub(1));
                }
            }
            "chapter-prev" => self.chapter_offset = self.chapter_offset.saturating_sub(8),
            "settings" | "settings-nav" => {
                self.settings_state.open = !self.settings_state.open;
                self.settings_state.dropdown_focus = 0;
                self.focused = if self.settings_state.open { 6 } else { 0 };
            }
            "diagnostics" if !self.failure_lines.is_empty() => {
                self.screen = Screen::Diagnostics;
                self.scroll.set(ScrollViewState::default());
            }
            "home" => {
                self.settings_state.open = false;
                self.screen = Screen::Home;
                self.refine = false;
                self.focused = 0;
            }
            "chapters" => self.drawer = !self.drawer,
            "options" => {
                self.refine = !self.refine;
                self.focused = usize::from(self.refine);
            }
            "refine" => {
                self.refine = !self.refine;
                self.focused = if self.screen == Screen::Home { 1 } else { 2 };
                if !self.refine {
                    self.focused = usize::MAX;
                }
                if let Some(id) = FIELDS.get(self.focused) {
                    let _ = self.router.set_focused(&bmux_tui::focus::FocusId::new(*id));
                }
            }
            "theme" => {
                self.palette = self.palette.next();
                self.rebuild_reader();
            }
            "depth" => {
                self.depth = match self.depth {
                    Depth::Brief => Depth::Standard,
                    Depth::Standard => Depth::Detailed,
                    Depth::Detailed => Depth::Brief,
                }
            }
            "generate" | "apply" | "export" | "open" => return self.effect_action(action),
            "more" => {
                let cursor = self.cursor.take();
                return self.admit(Request::List { cursor, limit: 24 });
            }
            _ => {
                if let Some(index) = action
                    .strip_prefix("recent-")
                    .and_then(|n| n.parse::<usize>().ok())
                    && let Some(entry) = self.recent.get(index)
                {
                    return self.admit(Request::Load {
                        id: entry.id.clone(),
                    });
                }
                if let Some(index) = action
                    .strip_prefix("chapter-")
                    .and_then(|n| n.parse::<usize>().ok())
                {
                    self.chapter = index;
                    self.drawer = false;
                    self.rebuild_reader();
                }
            }
        }
        Update::redraw()
    }
    fn effect_action(&mut self, action: &str) -> Update<Message> {
        match action {
            "generate" => {
                if self.text(0).is_empty() {
                    self.status = "Add a YouTube URL or a local video path first.".into();
                    return Update::redraw();
                }
                if self
                    .settings_state
                    .discovery
                    .as_ref()
                    .is_none_or(|discovery| {
                        discovery.selected.provider.is_none() || discovery.selected.model.is_none()
                    })
                {
                    self.status = format!(
                        "{} · Open Settings (F7) before creating",
                        self.settings_state.status
                    );
                    return Update::redraw();
                }
                let request = self.options.create_request(
                    self.text(0),
                    (!self.text(1).is_empty()).then(|| PathBuf::from(self.text(1))),
                    (!self.text(2).is_empty()).then(|| self.text(2)),
                    self.depth,
                );
                return self.admit(Request::CreateAndExport(Box::new(request)));
            }
            "apply" => {
                if let Some(document) = &self.document {
                    if self.text(2).is_empty() {
                        self.status = "Enter an editorial focus before refining.".into();
                    } else {
                        return self.admit(Request::RefineAndExport(Box::new(
                            self.options
                                .refine_request(document.id.clone(), self.text(2)),
                        )));
                    }
                }
            }
            "export" => {
                if let Some(document) = &self.document {
                    let mut options = self.options.output_options();
                    options.output =
                        (!self.text(5).is_empty()).then(|| PathBuf::from(self.text(5)));
                    return self.admit(Request::ExportCopy {
                        id: document.id.clone(),
                        options,
                    });
                }
            }
            "open" => {
                if let Some(path) = self.export.clone() {
                    return self.admit(Request::Open { path });
                }
                self.status = "Export this recap before opening its HTML artifact.".into();
            }
            _ => {}
        }
        Update::redraw()
    }
    fn global_terminal(&mut self, event: &Event) -> Option<Update<Message>> {
        // Quit belongs to the application, never to the focused editor or button.
        // Raw mode delivers Ctrl+C as input rather than generating SIGINT.
        if event.key().is_some_and(|key| {
            key.key == KeyCode::F(10)
                || (key.modifiers.ctrl
                    && !key.modifiers.alt
                    && matches!(key.key, KeyCode::Char('c' | 'C' | 'q' | 'Q')))
        }) {
            return Some(self.action("quit"));
        }
        if let Event::Resize(size) = event {
            self.size.set((size.width, size.height));
            self.layout.replace(None);
            return Some(Update::redraw());
        }
        None
    }
    fn terminal(&mut self, event: &Event) -> Update<Message> {
        if let Some(update) = self.global_terminal(event) {
            return update;
        }
        let focused = self.focused;
        let control_id = FIELDS
            .get(focused)
            .map(|id| bmux_tui::focus::FocusId::new(*id));
        if let Some(id) = control_id {
            let _ = self.router.set_focused(&id);
        }
        let route = self.router.route(event.clone());
        if self.settings_state.open {
            let target = route.target.as_ref().map(bmux_tui::hit::HitId::as_str);
            if let Some(update) = self.settings_event(event, target) {
                return update;
            }
        }
        for (index, id) in FIELDS.iter().enumerate() {
            if route.targets(&bmux_tui::hit::HitId::new(*id)) {
                self.focused = index;
                self.settings_state.dropdown_focus = 0;
            }
        }
        if let Some(focus) = route.focus_changed.as_ref() {
            self.settings_state.dropdown_focus = match focus.as_str() {
                "settings.connections" => 1,
                "settings.models" => 2,
                _ => 0,
            };
            self.focused = FIELDS
                .iter()
                .position(|id| *id == focus.as_str())
                .unwrap_or(usize::MAX);
        }
        if route.traversal_consumed {
            return Update::redraw();
        }
        if route.is_activation()
            && let Some(id) = route.target.as_ref()
            && !FIELDS.contains(&id.as_str())
        {
            return self.action(id.as_str());
        }
        if let Some(key) = event.key() {
            let action = match key.key {
                KeyCode::Escape => Some(if self.active.is_some() {
                    "cancel"
                } else if self.settings_state.open {
                    "settings"
                } else {
                    "home"
                }),
                KeyCode::F(7) => Some("settings"),
                KeyCode::Enter
                    if self.screen == Screen::Home
                        && !self.settings_state.open
                        && self.focused == 0 =>
                {
                    Some("generate")
                }
                KeyCode::F(2) if !self.settings_state.open => Some("generate"),
                KeyCode::F(3) => Some("chapters"),
                KeyCode::F(4) => Some("refine"),
                KeyCode::F(5) => Some("export"),
                KeyCode::F(6) => Some("open"),
                KeyCode::F(9) => Some("diagnostics"),
                KeyCode::F(8) => Some("theme"),
                KeyCode::F(10) => Some("quit"),
                _ => None,
            };
            if let Some(action) = action {
                return self.action(action);
            }
            if key.key == KeyCode::Tab {
                return Update::redraw();
            }
        }
        if self.active.is_none()
            && self.focused < FIELDS.len()
            && matches!(event, Event::Key(_) | Event::Paste(_))
        {
            let policy = TextInputPolicy::chat_composer();
            let _ = TextInputControl::new(&policy)
                .handle_event(&mut self.fields[self.focused].borrow_mut(), event);
            if self.settings_state.open && self.focused == 6 {
                self.rebuild_choices();
            }
            return Update::redraw();
        }
        if self.active.is_none()
            && let Some(layout) = self.layout.borrow().as_ref()
        {
            let mut cx = EventCx::new(layout);
            self.tree().event(event, layout, &mut cx);
        }

        Update::redraw()
    }
    fn install_discovery(
        &mut self,
        result: Result<(crate::connections::Discovery, crate::connections::Selection), String>,
    ) {
        match result {
            Ok((discovery, selection)) => {
                self.apply_selection(selection);
                self.settings_state.status = format!(
                    "{} connections discovered · Settings save automatically",
                    discovery.connections.len()
                );
                self.settings_state.discovery = Some(discovery);
                self.rebuild_choices();
            }
            Err(error) => {
                self.settings_state.discovery = None;
                self.settings_state.status = format!("{error} · Choose a config file in Settings");
                self.rebuild_choices();
            }
        }
    }
    fn install_outcome(&mut self, result: Result<Outcome, String>) {
        match result {
            Ok(Outcome::Checked) => {
                self.status = "Source acquisition checked".into();
            }
            Ok(Outcome::Ready { document, export }) => {
                self.document = Some(*document);
                self.focused = usize::MAX;
                self.screen = Screen::Reader;
                self.chapter = 0;
                self.refine = false;
                match export {
                    Ok(result) => {
                        self.export = Some(result.path);
                        self.status = "Recap ready · HTML exported · F6 opens your document".into();
                    }
                    Err(error) => {
                        self.export = None;
                        self.status = format!(
                            "Recap saved · HTML export needs attention: {error} · F5 retry"
                        );
                    }
                }
                self.rebuild_reader();
            }
            Ok(Outcome::Document(document)) => {
                self.document = Some(*document);
                self.focused = usize::MAX;
                self.screen = Screen::Reader;
                self.chapter = 0;
                self.refine = false;
                self.export = None;
                self.status = "Saved · source-grounded recap ready".into();
                self.rebuild_reader();
            }
            Ok(Outcome::Catalog(page)) => {
                self.recent = page.entries;
                self.cursor = page.next;
                self.library_offset = 0;
                self.status = format!(
                    "Local library · examined {} entries · browsing does not call a model",
                    page.examined
                );
            }
            Ok(Outcome::Exported(result)) => {
                if self.screen == Screen::Diagnostics && self.document.is_some() {
                    self.screen = Screen::Reader;
                }
                self.status = format!("Exported {} · HTML is ready to open", result.path.display());
                self.export = Some(result.path);
            }
            Ok(Outcome::Opened) => {
                self.status = "Opened exported HTML in the system viewer".into();
            }
            Err(error) => {
                self.failure_lines = error
                    .split(" · ")
                    .map(|part| {
                        bmux_tui::text::Line::from_spans(vec![bmux_tui::text::Span::raw(
                            part.to_owned(),
                        )])
                    })
                    .collect();
                self.status = "Operation failed · F9 details · F7 Settings".into();
                self.screen = Screen::Diagnostics;
                self.scroll.set(ScrollViewState::default());
            }
        }
    }
    fn rebuild_reader(&mut self) {
        self.preview_generation = self.preview_generation.saturating_add(1);
        self.preview = None;
        if let Some(task) = self.preview_task.take() {
            task.abort();
        }
        if self.image_capabilities.any_supported()
            && let Some(document) = self.document.clone()
            && let Some(sender) = self.preview_sender.clone()
        {
            let options = self.options.clone();
            let chapter = self.chapter;
            let generation = self.preview_generation;
            let permit = self.preview_permit.clone();
            self.preview_task = Some(tokio::spawn(async move {
                let Ok(permit) = permit.acquire_owned().await else {
                    return;
                };
                let result = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let store = bcode_recap::storage::DocumentStore::new(
                        &options.store,
                        options.max_document_bytes,
                    );
                    images::load_preview(&store, &document, chapter)
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
                let _ = sender
                    .send(Message::Preview(generation, Box::new(result)))
                    .await;
            }));
        }
        self.lines = view::reader_lines(self.document.as_ref(), self.chapter, &self.palette);
        self.scroll.set(ScrollViewState::default());
    }
}

impl Program for App {
    type Message = Message;
    type Error = String;
    fn update(&mut self, event: RuntimeEvent<Message>) -> Result<Update<Message>, String> {
        match event {
            RuntimeEvent::Message(Message::Preview(generation, result)) => {
                if generation == self.preview_generation {
                    match *result {
                        Ok(preview) => self.preview = preview,
                        Err(error) => self.status = format!("Source image unavailable · {error}"),
                    }
                }
            }
            RuntimeEvent::Message(Message::Discovered(id, result)) => {
                if id != self.settings_state.generation {
                    return Ok(Update::none());
                }
                self.install_discovery(*result);
            }
            RuntimeEvent::Message(Message::Start) => {
                let discovery = self.discover_connections(true);
                let mut update = if let Some(id) = self.options.id.clone() {
                    self.admit(Request::Load { id })
                } else {
                    self.admit(Request::List {
                        cursor: None,
                        limit: 24,
                    })
                };
                update.commands.extend(discovery.commands);
                update.subscriptions.extend(discovery.subscriptions);
                return Ok(update);
            }
            RuntimeEvent::Message(Message::Tick) => {
                return Ok(if self.active.is_some() {
                    Update::redraw()
                } else {
                    Update::none()
                });
            }
            RuntimeEvent::Message(Message::InputFailed(error)) => {
                self.status = error;
                self.quitting = true;
                self.cancel();
            }
            RuntimeEvent::Message(Message::Progress(id, stage)) => {
                if let Some(active) = &mut self.active
                    && active.id == id
                {
                    active.stage = Some(stage);
                }
            }
            RuntimeEvent::Message(Message::Settled(id, result)) => {
                if self.active.as_ref().is_none_or(|active| active.id != id) {
                    return Ok(Update::none());
                }
                self.active = None;
                self.install_outcome(*result);
            }
            RuntimeEvent::Terminal(event) => return Ok(self.terminal(&event)),
            RuntimeEvent::Timer(_) => {}
        }
        if self.quitting && self.active.is_none() {
            Ok(Update::exit())
        } else {
            Ok(Update::redraw())
        }
    }
}

fn operation_error(error: &application::Error) -> String {
    use bcode_recap::{composition::CompositionError, engine::EngineError, model::ModelError};
    if let application::Error::Engine(EngineError::Composition(CompositionError::Model(model))) =
        error
    {
        return match model {
            ModelError::ExecutionFailed=>"Model request failed · check the selected profile's access, provider availability and request timeout. Rate limits or transport failures are possible, not confirmed. No automatic retry; review the operation before retrying explicitly.".into(),
            ModelError::Unavailable=>"Model unavailable · check the explicit config, profile and multimodal model selection, then Generate again.".into(),
            ModelError::InvalidResponse=>"Model returned an invalid structured recap · no fabricated fallback was saved. Review model compatibility before retrying explicitly.".into(),
            ModelError::Cancelled=>"Model operation cancelled · no successful recap is implied.".into(),
        };
    }
    error.to_string()
}

type RecapPresenter = ImageTerminalPresenter<
    std::io::Stdout,
    fn(&mut App, &mut bmux_tui::paint::PaintCx<'_, '_>),
    fn(&mut App, &bmux_tui::hit::HitMap, &bmux_tui::focus::FocusTrap),
>;
fn paint_app(app: &mut App, cx: &mut bmux_tui::paint::PaintCx<'_, '_>) {
    let area = cx.area();
    app.size.set((area.width, area.height));
    let tree = app.tree();
    let layout = tree.layout(
        Constraints::tight(Size::new(area.width, area.height)),
        &mut LayoutCx::default(),
    );
    tree.paint(&layout, cx);
    app.layout.replace(Some(layout));
}
fn commit_app(app: &mut App, hits: &bmux_tui::hit::HitMap, _: &bmux_tui::focus::FocusTrap) {
    app.router.commit_scene(hits.clone(), None);
    if app.focused < FIELDS.len() {
        let _ = app
            .router
            .set_focused(&bmux_tui::focus::FocusId::new(FIELDS[app.focused]));
    }
    for region in hits.regions() {
        if let Some(index) = FIELDS.iter().position(|id| *id == region.id.as_str())
            && app
                .router
                .focused()
                .is_some_and(|id| id.as_str() == FIELDS[index])
        {
            app.focused = index;
        }
    }
}
fn presenter(
    terminal: Terminal<std::io::Stdout>,
    capabilities: bmux_image::host_caps::HostImageCapabilities,
) -> RecapPresenter {
    ImageTerminalPresenter::with_commit(
        terminal,
        paint_app,
        commit_app,
        capabilities,
        bmux_image::ImageConfig {
            max_images_per_pane: 1,
            max_image_bytes: 4 * 1024 * 1024,
            ..bmux_image::ImageConfig::default()
        },
    )
}

/// Enter Recap's BMUX runtime. No inference occurs on launch or browse.
///
/// # Errors
/// Returns terminal setup, input, runtime or restoration errors.
pub async fn run(options: TuiOptions) -> Result<(), String> {
    let guard = CrosstermTerminalGuard::enter(std::io::stdout()).map_err(|e| e.to_string())?;
    let size = terminal_size().map_err(|e| e.to_string())?;
    let mut app = App::new(options);
    let capabilities = bmux_image::host_caps::detect_from_env();
    app.image_capabilities = capabilities.clone();
    let (preview_sender, mut preview_receiver) = tokio::sync::mpsc::channel(2);
    app.preview_sender = Some(preview_sender);
    app.size.set((size.width, size.height));
    let terminal = Terminal::new(std::io::stdout(), Rect::new(0, 0, size.width, size.height));
    let presenter = presenter(terminal, capabilities);
    let (runtime, handle) = Runtime::new(app, presenter, RuntimeConfig::default());
    let preview_handle = handle.clone();
    let previews = tokio::spawn(async move {
        while let Some(message) = preview_receiver.recv().await {
            if preview_handle.send(message).await.is_err() {
                break;
            }
        }
    });
    let input_handle = handle.clone();
    let input = tokio::spawn(async move {
        let mut input = bmux_tui_runtime::ManagedTerminalInput::start(32);
        loop {
            match input.recv().await {
                Some(Ok(Some(event))) => {
                    if input_handle.send_terminal(event).await.is_err() {
                        break;
                    }
                }
                Some(Ok(None)) => {}
                Some(Err(error)) => {
                    let _ = input_handle
                        .send(Message::InputFailed(error.to_string()))
                        .await;
                    break;
                }
                None => break,
            }
        }
        input.shutdown().await;
    });
    let tick_handle = handle.clone();
    let tick = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if tick_handle.send(Message::Tick).await.is_err() {
                break;
            }
        }
    });
    handle
        .send(Message::Start)
        .await
        .map_err(|e| format!("runtime admission failed: {e:?}"))?;
    let result = runtime.run().await;
    input.abort();
    tick.abort();
    previews.abort();
    let (mut app, error) = match result {
        Ok(output) => (output.program, None),
        Err(bmux_tui_runtime::RuntimeError::Program { error, output, .. }) => {
            (output.program, Some(error))
        }
        Err(bmux_tui_runtime::RuntimeError::Presenter { error, output, .. }) => {
            (output.program, Some(error.to_string()))
        }
    };
    if let Some(task) = app.preview_task.take() {
        task.abort();
    }
    if let Some(active) = app.active.take() {
        active.cancel.cancel();
        let _ = active.task.await;
    }
    drop(guard);
    error.map_or(Ok(()), Err)
}
