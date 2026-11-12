//! The contract between the run loop and the screens it drives.

use bcode_tui_theme::components::ComponentStyles;
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;

use crate::action::{Action, EventResult};
use crate::context::UiContext;

/// A screen the run loop drives: it reads the keymap, paints, handles input, and collects the
/// results of work it started.
pub trait View {
    /// Read the keys bound to the screen's actions (for hints, help, and the palette).
    fn prepare_keymap(&mut self, ui: &UiContext);

    /// Paint into `area` with the host's component `styles` (built-in colors when `None`).
    fn render(
        &mut self,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        styles: Option<&ComponentStyles>,
        ui: &UiContext,
    );

    /// Handle one input event.
    fn handle_event(&mut self, event: &Event, ui: &UiContext) -> EventResult;

    /// Collect finished work and advance timers; returns what changed.
    fn poll(&mut self, ui: &UiContext) -> Action;
}
