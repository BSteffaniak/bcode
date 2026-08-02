//! Native Tokio-backed TUI surface host APIs for plugins.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use crate::interaction::{InteractionInput, InteractionOutput, PluginInteraction};
use bcode_session_models::{ProjectionWindowRequest, SessionId};
use bcode_session_view_models::{ReasoningPresentationPolicy, SessionViewSnapshot};
use bmux_keyboard::KeyStroke;
use bmux_text_edit::{TextEditCommand, TextMotion};
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::Line;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
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

/// Renderer-neutral durable workflow binding supplied by the owning plugin.
///
/// The fields are deliberately bounded and generic. Domain data belongs in the typed workflow
/// input and compiled definition rather than in an opaque metadata payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkflowBinding {
    /// Plugin that owns product presentation and lifecycle vocabulary.
    pub owner_plugin_id: String,
    /// Stable product-facing workflow kind within the owner plugin.
    pub workflow_kind: String,
    /// Stable scope key used for bounded associated-run lookup, such as a session identity.
    pub scope_key: String,
    /// Optional compact user-facing label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_label: Option<String>,
    /// Whether at most one non-terminal run may exist for this owner/kind/scope.
    #[serde(default)]
    pub single_active: bool,
}

impl PluginWorkflowBinding {
    /// Return the exact generic lookup key carried across plugin host boundaries.
    #[must_use]
    pub fn lookup(&self) -> PluginWorkflowLookup {
        PluginWorkflowLookup {
            owner_plugin_id: self.owner_plugin_id.clone(),
            workflow_kind: self.workflow_kind.clone(),
            scope_key: self.scope_key.clone(),
        }
    }
}

/// Exact generic workflow association key available to plugin surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkflowLookup {
    pub owner_plugin_id: String,
    pub workflow_kind: String,
    pub scope_key: String,
}

/// Durable workflow status exposed without coupling the plugin SDK to persistence internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginWorkflowStatus {
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
    RepairRequired,
}

/// Bounded associated workflow summary exposed to plugin-owned surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkflowSummary {
    pub run_id: String,
    pub definition_id: String,
    pub definition_version: u32,
    pub status: PluginWorkflowStatus,
    /// Whether durable cancellation has been requested but the run is not yet terminal.
    #[serde(default)]
    pub cancellation_requested: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Bounded associated workflow inspection exposed to plugin-owned surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkflowInspection {
    pub run: PluginWorkflowSummary,
    pub definition: bcode_workflow::WorkflowDefinition,
    pub waits: Vec<serde_json::Value>,
    pub attempts: Vec<serde_json::Value>,
    pub events: Vec<serde_json::Value>,
    pub grants: Vec<serde_json::Value>,
    pub resource_leases: Vec<serde_json::Value>,
    pub outputs: Vec<serde_json::Value>,
    pub child_session_ids: Vec<SessionId>,
}

/// Lifecycle transition available through generic plugin host workflow routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginWorkflowControlAction {
    Pause,
    Resume,
    Cancel,
}

/// Result of applying one associated workflow lifecycle transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkflowControlResult {
    pub run: Option<PluginWorkflowSummary>,
    pub changed: bool,
}

/// Async associated workflow lookup result.
pub type PluginWorkflowLookupFuture = Pin<
    Box<
        dyn Future<Output = Result<Option<PluginWorkflowSummary>, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Async associated workflow inspection result.
pub type PluginWorkflowInspectionFuture = Pin<
    Box<
        dyn Future<Output = Result<Option<PluginWorkflowInspection>, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Async associated workflow lifecycle result.
pub type PluginWorkflowControlFuture = Pin<
    Box<
        dyn Future<Output = Result<PluginWorkflowControlResult, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Renderer-neutral request for a plugin surface to start one durable workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkflowStartRequest {
    /// Logical and exact identities derived from the validated compiled definition.
    pub identity: bcode_workflow::WorkflowDefinitionIdentity,
    pub definition: bcode_workflow::WorkflowDefinition,
    /// Optional stable run identity chosen by the plugin for crash-safe start retries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub parent_session_id: SessionId,
    pub input: serde_json::Value,
    pub binding: PluginWorkflowBinding,
}

impl PluginWorkflowStartRequest {
    /// Build a renderer-neutral start request from a typed durable specification and typed input.
    ///
    /// # Errors
    ///
    /// Returns an error when the input cannot be serialized or fails its generated schema.
    pub fn typed<I>(
        spec: &bcode_workflow::WorkflowSpec<I>,
        input: &I,
        parent_session_id: SessionId,
        binding: PluginWorkflowBinding,
        run_id: Option<String>,
    ) -> Result<Self, bcode_workflow::WorkflowError>
    where
        I: Serialize + DeserializeOwned + schemars::JsonSchema + Send + 'static,
    {
        Ok(Self {
            identity: spec.identity().clone(),
            definition: spec.definition().clone(),
            run_id,
            parent_session_id,
            input: spec.serialize_input(input)?,
            binding,
        })
    }
}

/// Async workflow-start result returned by a native TUI host.
pub type PluginWorkflowStartFuture = Pin<
    Box<
        dyn Future<Output = Result<PluginWorkflowStartResponse, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Renderer-neutral durable workflow-start result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkflowStartResponse {
    pub run_id: String,
    pub runtime_work_id: String,
}

/// Request to observe renderer-neutral semantic state for one explicit Bcode session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSessionViewSubscriptionRequest {
    /// Session to observe.
    pub session_id: SessionId,
    /// Bounded projection window used for initial attachment and authoritative resynchronization.
    pub projection: ProjectionWindowRequest,
    /// Renderer-local reasoning presentation policy. This never changes provider requests or
    /// durable session history.
    pub reasoning_policy: ReasoningPresentationPolicy,
    /// Requested snapshot channel buffer size. Hosts may clamp this value.
    pub buffer: usize,
}

/// Semantic session-view updates delivered to plugin-owned TUI surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSessionViewUpdate {
    /// Complete authoritative state. Consumers replace prior state by session id.
    Snapshot(Box<SessionViewSnapshot>),
    /// The observer stopped because attachment or daemon connectivity failed.
    Disconnected {
        /// Human-readable failure message.
        message: String,
    },
}

/// Active subscription to renderer-neutral semantic session state.
#[derive(Debug)]
pub struct PluginSessionViewSubscription {
    /// Receiver for complete authoritative snapshots and terminal observer failures.
    pub receiver: mpsc::Receiver<PluginSessionViewUpdate>,
}

/// Host services available to native TUI plugin surfaces.
pub trait PluginTuiHost: Send + Sync {
    /// Spawn an async task on Bcode's native Tokio runtime.
    fn spawn(&self, task: PluginTask);

    /// Spawn blocking work on Bcode's Tokio blocking pool.
    fn spawn_blocking(&self, task: Box<dyn FnOnce() + Send + 'static>);

    /// Request another terminal redraw.
    fn request_redraw(&self);

    /// Copy plain text to the host clipboard.
    ///
    /// # Errors
    ///
    /// Returns an error when the host does not support clipboard writes or the platform clipboard
    /// cannot be opened or updated.
    fn copy_text(&self, _text: String) -> Result<(), PluginTuiHostError> {
        Err(PluginTuiHostError::Unsupported(
            "clipboard writes are not available from this host".to_string(),
        ))
    }

    /// Resolve a key stroke to one host-configured text edit command.
    fn text_edit_command(&self, _stroke: KeyStroke) -> Option<TextEditCommand> {
        None
    }

    /// Resolve a key stroke to one host-configured selection motion.
    fn text_selection_motion(&self, _stroke: KeyStroke) -> Option<TextMotion> {
        None
    }

    /// Return whether a key stroke is configured to submit composer-like input.
    fn text_submit(&self, _stroke: KeyStroke) -> bool {
        false
    }

    /// Start one durable workflow through the host's generic workflow service.
    ///
    /// # Errors
    ///
    /// Returns an error when the host does not expose workflow start, permission is denied, or the
    /// daemon rejects registration/run admission.
    fn start_workflow(&self, _request: PluginWorkflowStartRequest) -> PluginWorkflowStartFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow start is not available from this host".to_string(),
            ))
        })
    }

    /// Return the newest workflow associated with one exact generic binding.
    fn associated_workflow(&self, _lookup: PluginWorkflowLookup) -> PluginWorkflowLookupFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow lookup is not available from this host".to_string(),
            ))
        })
    }

    /// Return a bounded aggregate inspection for the newest associated workflow.
    fn inspect_associated_workflow(
        &self,
        _lookup: PluginWorkflowLookup,
        _limit: usize,
    ) -> PluginWorkflowInspectionFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow inspection is not available from this host".to_string(),
            ))
        })
    }

    /// Apply one lifecycle transition to the newest associated workflow.
    fn control_associated_workflow(
        &self,
        _lookup: PluginWorkflowLookup,
        _action: PluginWorkflowControlAction,
    ) -> PluginWorkflowControlFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow lifecycle control is not available from this host".to_string(),
            ))
        })
    }

    /// Observe renderer-neutral semantic state for one explicit Bcode session.
    ///
    /// The host owns bounded attachment, event projection, reconnect, and resynchronization. Every
    /// update is a complete `SessionViewSnapshot`, so consumers replace prior state instead of
    /// interpreting raw durable or live events.
    ///
    /// # Errors
    ///
    /// Returns an error when the host does not support semantic session observation, permission is
    /// denied, or the request is invalid.
    fn subscribe_session_view(
        &self,
        _request: PluginSessionViewSubscriptionRequest,
    ) -> Result<PluginSessionViewSubscription, PluginTuiHostError> {
        Err(PluginTuiHostError::Unsupported(
            "semantic session observation is not available from this host".to_string(),
        ))
    }
}

/// Text returned by a plugin-owned TUI surface.
///
/// Legacy string values deserialize as plain text. Rich text is opt-in through the formatted
/// object shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginTuiText {
    /// Backward-compatible untyped text, interpreted literally.
    Plain(String),
    /// Explicitly formatted text.
    Formatted {
        /// Text content.
        text: String,
        /// Renderer-neutral format.
        #[serde(default)]
        format: bcode_command::CommandTextFormat,
    },
}

impl PluginTuiText {
    /// Split this value into text and its explicit renderer-neutral format.
    #[must_use]
    pub fn into_parts(self) -> (String, bcode_command::CommandTextFormat) {
        match self {
            Self::Plain(text) => (text, bcode_command::CommandTextFormat::PlainText),
            Self::Formatted { text, format } => (text, format),
        }
    }
}

/// Typed outcome returned when a plugin-owned TUI surface closes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginTuiSurfaceOutcome {
    /// Optional host status text.
    #[serde(default)]
    pub status: Option<String>,
    /// Optional transcript text. Legacy strings remain plain text.
    #[serde(default)]
    pub append_text: Option<PluginTuiText>,
    /// Optional working directory to attach to the active session.
    #[serde(default)]
    pub set_session_working_directory: Option<String>,
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
    /// Open an ordinary Bcode session and resume this surface when the native viewer returns.
    OpenSession { session_id: SessionId },
    /// Open another registered surface.
    OpenSurface { surface_id: String },
    /// Run a host command.
    RunCommand { command: String },
}

impl PluginTuiAction {
    /// Return whether this action requests a redraw.
    #[must_use]
    pub const fn requests_redraw(&self) -> bool {
        matches!(self, Self::Redraw | Self::OpenSession { .. })
    }
}

/// Host-owned transcript header hints supplied by a plugin visual adapter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginTuiTranscriptHeader {
    /// Plugin-selected title for host transcript chrome.
    pub title: Option<String>,
    /// Configured invocation timeout in milliseconds, when known.
    pub timeout_ms: Option<u64>,
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

    /// Return the invocation working directory supplied by the host.
    #[must_use]
    pub fn working_directory(&self) -> Option<&Path> {
        self.working_directory.as_deref()
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

/// Maximum diagnostic observations drained from one adapter at a time.
pub const MAX_PLUGIN_TUI_DIAGNOSTICS_PER_DRAIN: usize = 64;

/// One bounded numeric diagnostic emitted by a plugin-owned visual adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginTuiDiagnostic {
    /// Stable adapter-owned diagnostic name.
    pub name: String,
    /// Non-negative observation value.
    pub value: u64,
}

/// Native Rust plugin artifact/view renderer for inline transcript content.
pub trait PluginTuiVisualAdapter: Send + Sync {
    /// Return whether this adapter can render the artifact/view kind.
    fn supports(&self, kind: &str) -> bool;

    /// Return how this visual should be composed by the host.
    fn render_mode(&self, _kind: &str, _payload: &serde_json::Value) -> PluginTuiVisualRenderMode {
        PluginTuiVisualRenderMode::Inline
    }

    /// Return plugin-owned hints for the host transcript header.
    fn transcript_header(
        &self,
        _kind: &str,
        _payload: &serde_json::Value,
    ) -> PluginTuiTranscriptHeader {
        PluginTuiTranscriptHeader::default()
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

    /// Drain a bounded snapshot of adapter-owned work diagnostics.
    ///
    /// The default implementation emits no diagnostics. Hosts validate names and cap every drain;
    /// adapters must report only low-cardinality numeric work observations.
    fn drain_diagnostics(&self) -> Vec<PluginTuiDiagnostic> {
        Vec::new()
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

    /// Notify a retained surface that native-session navigation finished or failed.
    fn session_navigation_finished(&mut self, _session_id: SessionId, _result: Result<(), String>) {
    }

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
    fn input(
        &mut self,
        event: &Event,
        snapshot: &C::Snapshot,
        _host: &dyn PluginTuiHost,
    ) -> Option<InteractionInput>;
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

    fn handle_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction {
        let Some(input) = self
            .renderer
            .input(event, &self.controller.snapshot(), host)
        else {
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

/// Factory for one statically linked plugin TUI registry.
pub type PluginTuiRegistryFactory = fn() -> PluginTuiRegistry;

/// Distribution-provided static TUI extension registration.
#[derive(Clone, Copy)]
pub struct StaticPluginTuiExtension {
    plugin_id: &'static str,
    registry_factory: PluginTuiRegistryFactory,
}

impl std::fmt::Debug for StaticPluginTuiExtension {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StaticPluginTuiExtension")
            .field("plugin_id", &self.plugin_id)
            .finish_non_exhaustive()
    }
}

impl StaticPluginTuiExtension {
    /// Register one static plugin's native TUI extension factory.
    #[must_use]
    pub const fn new(plugin_id: &'static str, registry_factory: PluginTuiRegistryFactory) -> Self {
        Self {
            plugin_id,
            registry_factory,
        }
    }

    /// Return the owning plugin ID.
    #[must_use]
    pub const fn plugin_id(self) -> &'static str {
        self.plugin_id
    }

    /// Return the native TUI registry factory.
    #[must_use]
    pub const fn registry_factory(self) -> PluginTuiRegistryFactory {
        self.registry_factory
    }

    /// Construct an independent native TUI registry.
    #[must_use]
    pub fn registry(self) -> PluginTuiRegistry {
        (self.registry_factory)()
    }
}

/// Registry of native TUI surfaces contributed by one plugin.
#[derive(Default)]
pub struct PluginTuiRegistry {
    factories: BTreeMap<String, Box<dyn PluginTuiSurfaceFactory>>,
    visual_adapters: BTreeMap<String, std::sync::Arc<dyn PluginTuiVisualAdapter>>,
}

impl std::fmt::Debug for PluginTuiRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginTuiRegistry")
            .field("surface_kinds", &self.factories.keys().collect::<Vec<_>>())
            .field(
                "visual_adapters",
                &self.visual_adapters.keys().collect::<Vec<_>>(),
            )
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

    /// Register a native TUI visual adapter under one or more manifest adapter IDs.
    pub fn register_visual_adapter<I, S>(
        &mut self,
        adapter_ids: I,
        adapter: Box<dyn PluginTuiVisualAdapter>,
    ) where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let adapter: std::sync::Arc<dyn PluginTuiVisualAdapter> = adapter.into();
        for adapter_id in adapter_ids {
            self.visual_adapters
                .insert(adapter_id.into(), std::sync::Arc::clone(&adapter));
        }
    }

    fn visual_adapter(&self, adapter_id: &str, kind: &str) -> Option<&dyn PluginTuiVisualAdapter> {
        self.visual_adapters
            .get(adapter_id)
            .filter(|adapter| adapter.supports(kind))
            .map(std::convert::AsRef::as_ref)
    }

    /// Return the number of native visual adapters in this registry.
    #[must_use]
    pub fn visual_adapter_count(&self) -> usize {
        self.visual_adapters.len()
    }

    /// Return whether the exact registered adapter supports this payload kind.
    #[must_use]
    pub fn supports_visual_adapter(&self, adapter_id: &str, kind: &str) -> bool {
        self.visual_adapter(adapter_id, kind).is_some()
    }

    /// Return whether any native visual adapter supports this payload kind.
    #[must_use]
    pub fn supports_visual(&self, kind: &str) -> bool {
        self.visual_adapters
            .values()
            .any(|adapter| adapter.supports(kind))
    }

    /// Return how a native visual adapter wants this payload composed.
    #[must_use]
    pub fn visual_render_mode(
        &self,
        adapter_id: &str,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Option<PluginTuiVisualRenderMode> {
        self.visual_adapter(adapter_id, kind)
            .map(|adapter| adapter.render_mode(kind, payload))
    }

    /// Return host transcript header hints from a matching visual adapter.
    #[must_use]
    pub fn visual_transcript_header(
        &self,
        adapter_id: &str,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Option<PluginTuiTranscriptHeader> {
        self.visual_adapter(adapter_id, kind)
            .map(|adapter| adapter.transcript_header(kind, payload))
    }

    /// Convert a renderer event through a matching visual adapter.
    #[must_use]
    pub fn visual_invocation_event_input(
        &self,
        adapter_id: &str,
        invocation_id: &str,
        kind: &str,
        payload: &serde_json::Value,
        event: &Event,
    ) -> Option<bcode_tool::ToolInvocationInput> {
        self.visual_adapter(adapter_id, kind)
            .and_then(|adapter| adapter.invocation_event_input(invocation_id, kind, payload, event))
    }

    /// Return whether the owning visual adapter consumes one artifact reference.
    #[must_use]
    pub fn visual_accepts_artifact_reference(
        &self,
        adapter_id: &str,
        kind: &str,
        reference_key: &str,
        content_type: Option<&str>,
    ) -> bool {
        self.visual_adapter(adapter_id, kind)
            .is_some_and(|adapter| {
                adapter.accepts_artifact_reference(kind, reference_key, content_type)
            })
    }

    /// Deliver opaque artifact bytes through the adapter that owns the artifact schema.
    ///
    /// # Errors
    ///
    /// Returns an error when the owning adapter rejects malformed or non-contiguous bytes.
    pub fn visual_artifact_chunk(
        &self,
        adapter_id: &str,
        chunk: &PluginTuiArtifactChunk,
    ) -> Result<bool, String> {
        let Some(adapter) = self.visual_adapter(adapter_id, &chunk.schema) else {
            return Ok(false);
        };
        adapter.artifact_chunk(chunk)?;
        Ok(true)
    }

    /// Drain bounded diagnostics from every registered visual adapter.
    #[must_use]
    pub fn drain_visual_diagnostics(&self) -> Vec<PluginTuiDiagnostic> {
        self.visual_adapters
            .values()
            .flat_map(|adapter| {
                adapter
                    .drain_diagnostics()
                    .into_iter()
                    .take(MAX_PLUGIN_TUI_DIAGNOSTICS_PER_DRAIN)
            })
            .take(MAX_PLUGIN_TUI_DIAGNOSTICS_PER_DRAIN)
            .collect()
    }

    /// Build transcript rows with host-owned presentation preferences.
    #[must_use]
    pub fn visual_rows(
        &self,
        adapter_id: &str,
        kind: &str,
        payload: &serde_json::Value,
        context: &PluginTuiVisualRenderContext,
    ) -> Option<Vec<Line>> {
        self.visual_adapter(adapter_id, kind)
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

#[derive(Clone)]
pub struct TokioPluginTuiHost {
    handle: tokio::runtime::Handle,
    redraw_sender: mpsc::UnboundedSender<()>,
    text_input: Option<Arc<dyn PluginTuiTextInputResolver>>,
}

impl std::fmt::Debug for TokioPluginTuiHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokioPluginTuiHost")
            .field(
                "text_input",
                &self.text_input.as_ref().map(|_| "configured"),
            )
            .finish_non_exhaustive()
    }
}

/// Host resolver for configured composer-like text input intent.
pub trait PluginTuiTextInputResolver: Send + Sync {
    /// Resolve one configured edit command.
    fn edit_command(&self, stroke: KeyStroke) -> Option<TextEditCommand>;

    /// Resolve one configured selection motion.
    fn selection_motion(&self, stroke: KeyStroke) -> Option<TextMotion>;

    /// Return whether this stroke submits composer-like input.
    fn submits(&self, stroke: KeyStroke) -> bool;
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
            text_input: None,
        }
    }

    /// Attach a host-configured composer-like text input resolver.
    #[must_use]
    pub fn with_text_input_resolver(
        mut self,
        resolver: Arc<dyn PluginTuiTextInputResolver>,
    ) -> Self {
        self.text_input = Some(resolver);
        self
    }

    /// Replace the host-configured composer-like text input resolver.
    pub fn set_text_input_resolver(&mut self, resolver: Arc<dyn PluginTuiTextInputResolver>) {
        self.text_input = Some(resolver);
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

    fn text_edit_command(&self, stroke: KeyStroke) -> Option<TextEditCommand> {
        self.text_input.as_ref()?.edit_command(stroke)
    }

    fn text_selection_motion(&self, stroke: KeyStroke) -> Option<TextMotion> {
        self.text_input.as_ref()?.selection_motion(stroke)
    }

    fn text_submit(&self, stroke: KeyStroke) -> bool {
        self.text_input
            .as_ref()
            .is_some_and(|resolver| resolver.submits(stroke))
    }
}
