//! BMUX backend skill picker rendering.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::input::TextInput;
use bmux_tui::list::List;
use bmux_tui::prelude::{Line, Span, StatefulWidget, Style, Widget};
use bmux_tui::style::{Color, Modifier};

use super::skill_picker::{SkillPickerApp, SkillPickerMode};

/// Render the skill picker.
pub(super) fn render_skill_picker(app: &mut SkillPickerApp, frame: &mut Frame<'_>) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }
    let panel = Panel::new()
        .border(Border::single().style(Style::new().fg(Color::Cyan)))
        .title(" Skills ")
        .padding(Insets::new(1, 1, 1, 1));
    panel.render(area, frame);
    let inner = panel.inner_area(area);
    frame.write_line(
        Rect::new(inner.x, inner.y, inner.width, 1),
        &Line::from_spans(vec![
            Span::styled("Skills", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw("  Enter invoke  a activate  d deactivate  ? help  Esc cancel"),
        ]),
    );
    let filter = Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1);
    TextInput::new(app.filter())
        .placeholder("Filter skills")
        .render(filter, frame);

    let bottom_height = match app.mode() {
        SkillPickerMode::Filter => 1,
        SkillPickerMode::Argument => 3,
    };
    let bottom_y = inner.bottom().saturating_sub(bottom_height);
    if matches!(app.mode(), SkillPickerMode::Argument) {
        frame.write_line(
            Rect::new(inner.x, bottom_y, inner.width, 1),
            &Line::from_spans(vec![Span::styled(
                "Invocation arguments/display text:",
                Style::new().fg(Color::BrightBlack),
            )]),
        );
        TextInput::new(app.argument())
            .placeholder("Optional arguments")
            .render(
                Rect::new(inner.x, bottom_y.saturating_add(1), inner.width, 1),
                frame,
            );
    } else {
        frame.write_line(
            Rect::new(inner.x, bottom_y, inner.width, 1),
            &Line::from_spans(vec![Span::styled(
                "Use / palette to reopen. Activation persists for this session.",
                Style::new().fg(Color::BrightBlack),
            )]),
        );
    }

    let list_y = filter.y.saturating_add(2);
    if bottom_y <= list_y {
        return;
    }
    let list_area = Rect::new(inner.x, list_y, inner.width, bottom_y - list_y);
    let items = app.list_items();
    let mut state = *app.list_state_mut();
    state.ensure_selected_visible(list_area.height, items.len());
    List::new(&items)
        .highlight_symbol("> ")
        .render(list_area, frame, &mut state);
    *app.list_state_mut() = state;
}
