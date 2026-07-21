//! Native Tokio-backed TUI surface host APIs for plugins.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use crate::interaction::{InteractionInput, InteractionOutput, PluginInteraction};
use bcode_session_models::{
    ProjectionWindowRequest, SessionEvent, SessionId, SessionLiveEvent, SessionSummary,
};
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::Line;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Boxed error returned by native TUI plugin surface factories.
pub type PluginTuiError = Box<dyn Error + Send + Sync>;

/// Boxed native TUI plugin surface.
pub type BoxedPluginTuiSurface = Box<dyn PluginTuiSurface>;

/// Boxed native TUI plugin surface future.
pub type PluginTuiSurfaceFuture =
    Pin<Box<dyn Future<Output = Result<BoxedPluginTuiSurface, PluginTuiError>> + Send + 'static>>;

/// Boxed asynchronous task accepted by a plugin host.
pub type PluginTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Errors returned by TUI host capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginTuiHostError {
    /// This host does not support the requested capability.
    Unsupported(String),
    /// The plugin is not permitted to use the requested capability.
    PermissionDenied(String),
    /// The plugin requested an invalid host operation.
    InvalidRequest(String),
    /// The host failed while preparing the operation.
    Internal(String),
}

impl fmt::Display for PluginTuiHostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(message)
            | Self::PermissionDenied(message)
            | Self::InvalidRequest(message)
            | Self::Internal(message) => formatter.write_str(message),
        }
    }
}

impl Error for PluginTuiHostError {}

/// Session history replay requested when subscribing to session events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSessionEventReplay {
    /// Do not replay persisted history before streaming live events.
    None,
    /// Replay a bounded number of recent session events before streaming live events.
    Recent { limit: usize },
    /// Replay a projection-sized history window before streaming live events.
    ProjectionWindow { request: ProjectionWindowRequest },
}

/// Request to subscribe to events for one explicit Bcode session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSessionEventSubscriptionRequest {
    /// Session to observe.
    pub session_id: SessionId,
    /// Optional persisted history replay to send before live events.
    pub replay: PluginSessionEventReplay,
    /// Requested event channel buffer size. Hosts may clamp this value.
    pub buffer: usize,
}

/// Session events delivered to plugin-owned TUI surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum PluginSessionEvent {
    /// The session was attached and optional persisted history is available.
    Attached {
        /// Attached session summary.
        session: SessionSummary,
        /// Replayed persisted history, according to the subscription request.
        history: Vec<SessionEvent>,
    },
    /// Durable session event.
    Session(SessionEvent),
    /// Ephemeral live session event.
    SessionLive(SessionLiveEvent),
    /// Events were dropped because the subscriber could not keep up.
    Lagged {
        /// Number of events dropped since the previous lag notification.
        dropped_count: u64,
    },
    /// The subscription stopped because the host disconnected or failed.
    Disconnected {
        /// Human-readable failure message.
        message: String,
    },
}

/// Active subscription to session events.
#[derive(Debug)]
pub struct PluginSessionEventSubscription {
    /// Receiver for generic session events.
    pub receiver: mpsc::Receiver<PluginSessionEvent>,
}

/// Host services available to native TUI plugin surfaces.
pub trait PluginTuiHost: Send + Sync {
    /// Spawn an async task on Bcode's native Tokio runtime.
    fn spawn(&self, task: PluginTask);

    /// Spawn blocking work on Bcode's Tokio blocking pool.
    fn spawn_blocking(&self, task: Box<dyn FnOnce() + Send + 'static>);

    /// Request another terminal redraw.
    fn request_redraw(&self);

    /// Subscribe to events for one explicit Bcode session.
    ///
    /// # Errors
    ///
    /// Returns an error when the host does not support session subscriptions, permission is
    /// denied, or the request is invalid.
    fn subscribe_session_events(
        &self,
        _request: PluginSessionEventSubscriptionRequest,
    ) -> Result<PluginSessionEventSubscription, PluginTuiHostError> {
        Err(PluginTuiHostError::Unsupported(
            "session event subscriptions are not available from this host".to_string(),
        ))
    }
}

/// Actions a native TUI surface can return to the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginTuiAction {
    /// No host action is needed.
    None,
    /// Redraw the terminal.
    Redraw,
    /// Close the current surface.
    Close { outcome: Option<serde_json::Value> },
    /// Open another registered surface.
    OpenSurface { surface_id: String },
    /// Run a host command.
    RunCommand { command: String },
}

impl PluginTuiAction {
    /// Return whether this action requests a redraw.
    #[must_use]
    pub const fn requests_redraw(&self) -> bool {
        matches!(self, Self::Redraw)
    }
}

/// How a visual adapter's rows should be composed into the host transcript block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginTuiVisualRenderMode {
    /// Rows are rendered inside the host-provided transcript block chrome/header.
    Inline,
    /// Rows are rendered inside host transcript block chrome with a plugin-selected title.
    TranscriptBlock,
    /// Rows replace the host-provided transcript block chrome/header.
    FullBlock,
}

/// Diff layout preference supplied by the TUI host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginTuiDiffLayout {
    Auto { breakpoint: u16 },
    Unified,
    SideBySide,
}

/// Host-owned presentation context for visual adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginTuiVisualRenderContext {
    width: u16,
    diff_layout: PluginTuiDiffLayout,
    working_directory: Option<PathBuf>,
}

impl PluginTuiVisualRenderContext {
    /// Construct a complete visual presentation context.
    #[must_use]
    pub const fn new(
        width: u16,
        diff_layout: PluginTuiDiffLayout,
        working_directory: Option<PathBuf>,
    ) -> Self {
        Self {
            width,
            diff_layout,
            working_directory,
        }
    }

    /// Return the width assigned to the visual.
    #[must_use]
    pub const fn width(&self) -> u16 {
        self.width
    }

    /// Return the effective diff viewer policy.
    #[must_use]
    pub const fn diff_layout(&self) -> PluginTuiDiffLayout {
        self.diff_layout
    }

    /// Format a path against the invocation working directory when known.
    #[must_use]
    pub fn display_path(&self, path: impl AsRef<Path>) -> crate::path::DisplayPath {
        self.working_directory.as_deref().map_or_else(
            || crate::path::display_without_base(path.as_ref()),
            |working_directory| crate::path::display(path.as_ref(), working_directory),
        )
    }
}

/// Opaque bounded artifact bytes delivered asynchronously to a plugin-owned visual adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginTuiArtifactChunk {
    pub tool_call_id: String,
    pub artifact_id: String,
    pub reference_key: String,
    pub producer_plugin_id: String,
    pub schema: String,
    pub schema_version: u32,
    pub content_type: Option<String>,
    pub offset: u64,
    pub total_bytes: u64,
    pub revision: u64,
    pub finalized: bool,
    pub bytes: Vec<u8>,
}

/// Native Rust plugin artifact/view renderer for inline transcript content.
pub trait PluginTuiVisualAdapter: Send + Sync {
    /// Return whether this adapter can render the artifact/view kind.
    fn supports(&self, kind: &str) -> bool;

    /// Return how this visual should be composed by the host.
    fn render_mode(&self, _kind: &str, _payload: &serde_json::Value) -> PluginTuiVisualRenderMode {
        PluginTuiVisualRenderMode::Inline
    }

    /// Convert a renderer event into neutral input for an active invocation.
    fn invocation_event_input(
        &self,
        _invocation_id: &str,
        _kind: &str,
        _payload: &serde_json::Value,
        _event: &Event,
    ) -> Option<bcode_tool::ToolInvocationInput> {
        None
    }

    /// Return whether this adapter consumes streamed bytes for one artifact reference.
    ///
    /// Hosts use this before scheduling range reads so unrelated references on the same artifact
    /// are not fetched or retried through an adapter that cannot interpret them.
    fn accepts_artifact_reference(
        &self,
        _kind: &str,
        _reference_key: &str,
        _content_type: Option<&str>,
    ) -> bool {
        false
    }

    /// Consume one ordered opaque artifact range fetched by the host outside rendering.
    ///
    /// The host may redeliver metadata revisions but does not redeliver byte ranges. Adapters must
    /// interpret bytes only for artifact schemas they own.
    ///
    /// # Errors
    ///
    /// Returns an error when the chunk metadata, byte range, or plugin-owned payload is invalid.
    fn artifact_chunk(&self, _chunk: &PluginTuiArtifactChunk) -> Result<(), String> {
        Ok(())
    }

    /// Build transcript rows for the artifact/view payload at the given width.
    fn rows(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        context: &PluginTuiVisualRenderContext,
    ) -> Vec<Line>;
}

/// Native Rust plugin surface rendered directly with `bmux_tui`.
pub trait PluginTuiSurface: Send {
    /// Stable surface identifier.
    fn id(&self) -> &'static str;

    /// Human-readable surface title.
    fn title(&self) -> &'static str;

    /// Return preferred height for this surface at the given width.
    #[must_use]
    fn preferred_height(&mut self, _width: u16) -> u16 {
        1
    }

    /// Render this surface inside the host-assigned area.
    fn render(&mut self, area: Rect, frame: &mut Frame<'_>);

    /// Handle routed terminal input.
    fn handle_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction;

    /// Poll internal async completions without blocking.
    fn poll(&mut self, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        PluginTuiAction::None
    }

    /// Drain effectful asynchronous work that was queued by synchronous input handling.
    fn drain_effects<'a>(
        &'a mut self,
        _host: &'a dyn PluginTuiHost,
    ) -> Pin<Box<dyn Future<Output = PluginTuiAction> + Send + 'a>> {
        Box::pin(async { PluginTuiAction::None })
    }
}

/// Terminal renderer/adapter for a typed renderer-neutral interaction.
pub trait TerminalInteractionRenderer<C>: Default + Send + 'static
where
    C: PluginInteraction,
{
    /// Native terminal surface kind.
    const SURFACE_KIND: &'static str;

    /// Stable surface identifier.
    fn id(&self) -> &'static str;

    /// Human-readable surface title.
    fn title(&self) -> &'static str;

    /// Return preferred height for a snapshot at the given width.
    #[must_use]
    fn preferred_height(&mut self, snapshot: &C::Snapshot, width: u16) -> u16;

    /// Render the snapshot.
    fn render(&mut self, snapshot: &C::Snapshot, area: Rect, frame: &mut Frame<'_>);

    /// Translate terminal input to a semantic interaction input.
    fn input(&mut self, event: &Event, snapshot: &C::Snapshot) -> Option<InteractionInput>;
}

struct TypedTerminalInteractionSurface<C, R>
where
    C: PluginInteraction,
    R: TerminalInteractionRenderer<C>,
{
    controller: C,
    renderer: R,
}

impl<C, R> TypedTerminalInteractionSurface<C, R>
where
    C: PluginInteraction,
    R: TerminalInteractionRenderer<C>,
{
    const fn new(controller: C, renderer: R) -> Self {
        Self {
            controller,
            renderer,
        }
    }
}

impl<C, R> PluginTuiSurface for TypedTerminalInteractionSurface<C, R>
where
    C: PluginInteraction,
    R: TerminalInteractionRenderer<C>,
{
    fn id(&self) -> &'static str {
        self.renderer.id()
    }

    fn title(&self) -> &'static str {
        self.renderer.title()
    }

    fn preferred_height(&mut self, width: u16) -> u16 {
        self.renderer
            .preferred_height(&self.controller.snapshot(), width)
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        self.renderer
            .render(&self.controller.snapshot(), area, frame);
    }

    fn handle_event(&mut self, event: &Event, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        let Some(input) = self.renderer.input(event, &self.controller.snapshot()) else {
            return PluginTuiAction::None;
        };
        plugin_tui_action_from_interaction_output(self.controller.handle_input(input))
    }
}

/// Factory for typed terminal interaction surfaces.
pub struct TypedTerminalInteractionSurfaceFactory<C, R>
where
    C: PluginInteraction,
    R: TerminalInteractionRenderer<C>,
{
    marker: PhantomData<fn() -> (C, R)>,
}

impl<C, R> TypedTerminalInteractionSurfaceFactory<C, R>
where
    C: PluginInteraction,
    R: TerminalInteractionRenderer<C>,
{
    /// Create a typed terminal interaction surface factory.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            marker: PhantomData,
        }
    }
}

impl<C, R> Default for TypedTerminalInteractionSurfaceFactory<C, R>
where
    C: PluginInteraction,
    R: TerminalInteractionRenderer<C>,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<C, R> PluginTuiSurfaceFactory for TypedTerminalInteractionSurfaceFactory<C, R>
where
    C: PluginInteraction,
    R: TerminalInteractionRenderer<C>,
{
    fn surface_kind(&self) -> &'static str {
        R::SURFACE_KIND
    }

    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async move {
            let request = serde_json::from_value::<C::Request>(request.options)?;
            Ok(Box::new(TypedTerminalInteractionSurface::<C, R>::new(
                C::new(request),
                R::default(),
            )) as BoxedPluginTuiSurface)
        })
    }
}

fn plugin_tui_action_from_interaction_output(output: InteractionOutput) -> PluginTuiAction {
    match output {
        InteractionOutput::None => PluginTuiAction::None,
        InteractionOutput::Redraw => PluginTuiAction::Redraw,
        InteractionOutput::Submitted { payload } => PluginTuiAction::Close {
            outcome: Some(payload),
        },
        InteractionOutput::Cancelled => PluginTuiAction::Close { outcome: None },
    }
}

/// Parameters used to open a native plugin TUI surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginTuiSurfaceOpenRequest {
    /// Host-assigned surface instance id.
    pub instance_id: String,
    /// Repository path or workspace path associated with the surface.
    pub repo_path: Option<PathBuf>,
    /// Plugin-defined target identifier.
    pub target: Option<String>,
    /// Plugin-defined JSON options.
    #[serde(default)]
    pub options: serde_json::Value,
}

/// Factory for plugin-owned native TUI surfaces.
pub trait PluginTuiSurfaceFactory: Send + Sync {
    /// Stable surface kind identifier.
    fn surface_kind(&self) -> &'static str;

    /// Open a new surface instance.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested surface cannot be opened.
    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture;
}

/// Registry of native TUI surfaces contributed by one plugin.
#[derive(Default)]
pub struct PluginTuiRegistry {
    factories: BTreeMap<String, Box<dyn PluginTuiSurfaceFactory>>,
    visual_adapters: Vec<Box<dyn PluginTuiVisualAdapter>>,
}

impl std::fmt::Debug for PluginTuiRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginTuiRegistry")
            .field("surface_kinds", &self.factories.keys().collect::<Vec<_>>())
            .field("visual_adapters", &self.visual_adapters.len())
            .finish()
    }
}

impl PluginTuiRegistry {
    /// Register a native TUI surface factory.
    pub fn register_factory(&mut self, factory: Box<dyn PluginTuiSurfaceFactory>) {
        self.factories
            .insert(factory.surface_kind().to_string(), factory);
    }

    /// Register a typed terminal renderer for a renderer-neutral interaction.
    pub fn register_interactive_surface<C, R>(&mut self)
    where
        C: PluginInteraction,
        R: TerminalInteractionRenderer<C>,
    {
        self.register_factory(Box::new(
            TypedTerminalInteractionSurfaceFactory::<C, R>::new(),
        ));
    }

    /// Register a native TUI visual adapter.
    pub fn register_visual_adapter(&mut self, adapter: Box<dyn PluginTuiVisualAdapter>) {
        self.visual_adapters.push(adapter);
    }

    /// Return the number of native visual adapters in this registry.
    #[must_use]
    pub fn visual_adapter_count(&self) -> usize {
        self.visual_adapters.len()
    }

    /// Return whether a native visual adapter supports this payload kind.
    #[must_use]
    pub fn supports_visual(&self, kind: &str) -> bool {
        self.visual_adapters
            .iter()
            .any(|adapter| adapter.supports(kind))
    }

    /// Return how a native visual adapter wants this payload composed.
    #[must_use]
    pub fn visual_render_mode(
        &self,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Option<PluginTuiVisualRenderMode> {
        self.visual_adapters
            .iter()
            .find(|adapter| adapter.supports(kind))
            .map(|adapter| adapter.render_mode(kind, payload))
    }

    /// Convert a renderer event through a matching visual adapter.
    #[must_use]
    pub fn visual_invocation_event_input(
        &self,
        invocation_id: &str,
        kind: &str,
        payload: &serde_json::Value,
        event: &Event,
    ) -> Option<bcode_tool::ToolInvocationInput> {
        self.visual_adapters
            .iter()
            .find(|adapter| adapter.supports(kind))
            .and_then(|adapter| adapter.invocation_event_input(invocation_id, kind, payload, event))
    }

    /// Return whether the owning visual adapter consumes one artifact reference.
    #[must_use]
    pub fn visual_accepts_artifact_reference(
        &self,
        kind: &str,
        reference_key: &str,
        content_type: Option<&str>,
    ) -> bool {
        self.visual_adapters
            .iter()
            .find(|adapter| adapter.supports(kind))
            .is_some_and(|adapter| {
                adapter.accepts_artifact_reference(kind, reference_key, content_type)
            })
    }

    /// Deliver opaque artifact bytes through the adapter that owns the artifact schema.
    ///
    /// # Errors
    ///
    /// Returns an error when the owning adapter rejects malformed or non-contiguous bytes.
    pub fn visual_artifact_chunk(&self, chunk: &PluginTuiArtifactChunk) -> Result<bool, String> {
        let Some(adapter) = self
            .visual_adapters
            .iter()
            .find(|adapter| adapter.supports(&chunk.schema))
        else {
            return Ok(false);
        };
        adapter.artifact_chunk(chunk)?;
        Ok(true)
    }

    /// Build transcript rows with host-owned presentation preferences.
    #[must_use]
    pub fn visual_rows(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        context: &PluginTuiVisualRenderContext,
    ) -> Option<Vec<Line>> {
        self.visual_adapters
            .iter()
            .find(|adapter| adapter.supports(kind))
            .map(|adapter| adapter.rows(kind, payload, context))
    }

    /// Open a registered surface.
    ///
    /// # Errors
    ///
    /// Returns an error when no factory exists or the factory fails to open the surface.
    pub async fn open(
        &self,
        surface_kind: &str,
        request: PluginTuiSurfaceOpenRequest,
    ) -> Result<BoxedPluginTuiSurface, PluginTuiError> {
        let factory = self
            .factories
            .get(surface_kind)
            .ok_or_else(|| format!("unsupported TUI surface kind: {surface_kind}"))?;
        factory.open(request).await
    }
}

#[derive(Debug, Clone)]
pub struct TokioPluginTuiHost {
    handle: tokio::runtime::Handle,
    redraw_sender: mpsc::UnboundedSender<()>,
}

impl TokioPluginTuiHost {
    /// Create a host handle from the current Tokio runtime.
    ///
    /// # Panics
    ///
    /// Panics if called outside a Tokio runtime.
    #[must_use]
    pub fn current(redraw_sender: mpsc::UnboundedSender<()>) -> Self {
        Self {
            handle: tokio::runtime::Handle::current(),
            redraw_sender,
        }
    }
}

impl PluginTuiHost for TokioPluginTuiHost {
    fn spawn(&self, task: PluginTask) {
        let redraw_sender = self.redraw_sender.clone();
        drop(self.handle.spawn(async move {
            task.await;
            let _ = redraw_sender.send(());
        }));
    }

    fn spawn_blocking(&self, task: Box<dyn FnOnce() + Send + 'static>) {
        let redraw_sender = self.redraw_sender.clone();
        drop(self.handle.spawn_blocking(move || {
            task();
            let _ = redraw_sender.send(());
        }));
    }

    fn request_redraw(&self) {
        let _ = self.redraw_sender.send(());
    }
}
