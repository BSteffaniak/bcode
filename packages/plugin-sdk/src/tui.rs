//! Native Tokio-backed TUI surface host APIs for plugins.

use std::future::Future;
use std::pin::Pin;

use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use tokio::sync::mpsc;

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
    Close,
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
}

/// Tokio-runtime-backed TUI plugin host handle.
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
