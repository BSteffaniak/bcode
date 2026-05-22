//! TUI command palette rendering.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::palette::CommandPalette;
use bmux_tui::prelude::{StatefulWidget, Style};
use bmux_tui::style::Color;

use super::command_palette::BmuxCommandPalette;

/// Render a command palette overlay.
pub fn render_palette(palette: &mut BmuxCommandPalette, frame: &mut Frame<'_>) {
    let area = palette_area(frame.area());
    let items = palette.cloned_items();
    let widget = CommandPalette::new(&items)
        .panel(
            Panel::new()
                .border(Border::single().style(Style::new().fg(Color::Cyan)))
                .title(" Commands ")
                .padding(Insets::new(1, 1, 1, 1)),
        )
        .empty("No matching commands");
    widget.render(area, frame, palette.state_mut());
}

fn palette_area(area: Rect) -> Rect {
    let width = area.width.saturating_sub(4).min(72);
    let height = area.height.saturating_sub(4).min(12);
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height) / 3);
    Rect::new(x, y, width, height)
}
