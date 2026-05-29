//! TUI skill picker rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, Style, Widget};
use bmux_tui::style::{Color, Modifier};

use super::picker_render::{
    picker_base_style, picker_list_area, render_picker_chrome, render_picker_list,
};
use super::skill_picker::{SkillPickerApp, SkillPickerMode};
use super::text_input_flow;

/// Render the skill picker.
pub fn render_skill_picker(app: &mut SkillPickerApp, frame: &mut Frame<'_>) {
    let Some((inner, list_y)) = render_picker_chrome(
        " Skills ",
        &Line::from_spans(vec![
            Span::styled("Skills", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw("  Enter invoke  a activate  d deactivate  ? help  Esc cancel"),
        ]),
        app.filter_mut(),
        "Filter skills",
        frame,
    ) else {
        return;
    };

    let bottom_y = render_bottom(app, inner, frame);
    let Some(list_area) = picker_list_area(inner, list_y, bottom_y) else {
        return;
    };
    let items = app.list_items();
    render_picker_list(&items, app.list_state_mut(), list_area, frame);
}

fn render_bottom(app: &mut SkillPickerApp, inner: Rect, frame: &mut Frame<'_>) -> u16 {
    let bottom_height = match app.mode() {
        SkillPickerMode::Filter => 1,
        SkillPickerMode::Argument => 3,
    };
    let bottom_y = inner.bottom().saturating_sub(bottom_height);
    if matches!(app.mode(), SkillPickerMode::Argument) {
        frame.write_line_with_fallback_style(
            Rect::new(inner.x, bottom_y, inner.width, 1),
            &Line::from_spans(vec![Span::styled(
                "Invocation arguments/display text:",
                Style::new().fg(Color::BrightBlack),
            )]),
            picker_base_style(),
        );
        let input_area = Rect::new(inner.x, bottom_y.saturating_add(1), inner.width, 1);
        app.argument_mut()
            .set_content_area(input_area, &text_input_flow::single_line_policy());
        TextInput::new(app.argument().buffer())
            .style(picker_base_style())
            .selection_style(Style::new().fg(Color::Black).bg(Color::Yellow))
            .placeholder("Optional arguments")
            .placeholder_style(Style::new().fg(Color::BrightBlack).bg(Color::Black))
            .vertical_scroll(app.argument().vertical_scroll())
            .render(input_area, frame);
    } else {
        frame.write_line_with_fallback_style(
            Rect::new(inner.x, bottom_y, inner.width, 1),
            &Line::from_spans(vec![Span::styled(
                "Use / palette to reopen. Activation persists for this session.",
                Style::new().fg(Color::BrightBlack),
            )]),
            picker_base_style(),
        );
    }
    bottom_y
}
