//! Native Tokio-backed TUI surface host APIs for plugins.

use std::collections::BTreeMap;
use std::error::Error;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
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

/// Host services available to native TUI plugin surfaces.
pub trait PluginTuiHost: Send + Sync {
    /// Spawn an async task on Bcode's native Tokio runtime.
    fn spawn(&self, task: PluginTask);

    /// Spawn blocking work on Bcode's Tokio blocking pool.
    fn spawn_blocking(&self, task: Box<dyn FnOnce() + Send + 'static>);

    /// Request another terminal redraw.
    fn request_redraw(&self);
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

/// Native Rust plugin surface rendered directly with `bmux_tui`.
pub trait PluginTuiSurface: Send {
    /// Stable surface identifier.
    fn id(&self) -> &'static str;

    /// Human-readable surface title.
    fn title(&self) -> &'static str;

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
}

impl std::fmt::Debug for PluginTuiRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginTuiRegistry")
            .field("surface_kinds", &self.factories.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl PluginTuiRegistry {
    /// Register a native TUI surface factory.
    pub fn register_factory(&mut self, factory: Box<dyn PluginTuiSurfaceFactory>) {
        self.factories
            .insert(factory.surface_kind().to_string(), factory);
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
        drop(self.handle.spawn(task));
    }

    fn spawn_blocking(&self, task: Box<dyn FnOnce() + Send + 'static>) {
        drop(self.handle.spawn_blocking(task));
    }

    fn request_redraw(&self) {
        let _ = self.redraw_sender.send(());
    }
}
