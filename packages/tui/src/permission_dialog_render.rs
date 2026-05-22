//! TUI permission dialog rendering.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::dialog::{Dialog, DialogAction};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::prelude::{Line, Span, StatefulWidget, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui::text::Text;

use super::permission_dialog::PermissionDialogState;

/// Render a permission approval dialog.
pub fn render_permission_dialog(state: &mut PermissionDialogState, frame: &mut Frame<'_>) {
    let area = dialog_area(frame.area());
    let permission = state.permission();
    let body = Text::from_lines(vec![
        Line::from_spans(vec![Span::styled(
            "Permission requested",
            Style::new().add_modifier(Modifier::BOLD),
        )]),
        Line::raw(""),
        Line::from_spans(vec![
            Span::styled("Tool: ", Style::new().fg(Color::BrightBlack)),
            Span::raw(permission.tool_name.clone()),
        ]),
        Line::from_spans(vec![
            Span::styled("Agent: ", Style::new().fg(Color::BrightBlack)),
            Span::raw(permission.agent_id.clone()),
        ]),
        Line::raw(""),
        Line::raw(permission.arguments_json.clone()),
    ]);
    let actions = vec![
        DialogAction::new("approve", "Approve"),
        DialogAction::new("deny", "Deny"),
    ];
    Dialog::new(body, &actions)
        .panel(
            Panel::new()
                .border(Border::single().style(Style::new().fg(Color::Yellow)))
                .title(" Permission ")
                .padding(Insets::new(1, 1, 1, 1)),
        )
        .render(area, frame, state.dialog_mut());
}

fn dialog_area(area: Rect) -> Rect {
    let width = area.width.saturating_sub(4).min(76);
    let height = area.height.saturating_sub(4).min(14);
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height) / 3);
    Rect::new(x, y, width, height)
}
