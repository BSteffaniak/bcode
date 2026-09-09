//! TUI skill picker rendering.

use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;

use super::picker_render::{
    picker_base_style, picker_list_area, render_picker_chrome, render_picker_list,
};
use super::render::TuiTheme;
use super::skill_picker::{SkillPickerApp, SkillPickerMode};
use super::text_input_flow;

/// Render the skill picker.
pub fn render_skill_picker(app: &mut SkillPickerApp, frame: &mut PaintCx<'_, '_>, theme: TuiTheme) {
    let Some((inner, list_y)) = render_picker_chrome(
        " Skills ",
        &Line::from_spans(vec![
            Span::styled("Skills", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw("  Enter invoke  a activate  d deactivate  ? help  Esc cancel"),
        ]),
        app.filter_mut(),
        "Filter skills",
        frame,
        theme,
    ) else {
        return;
    };

    let bottom_y = render_bottom(app, inner, frame, theme);
    let Some(list_area) = picker_list_area(inner, list_y, bottom_y) else {
        return;
    };
    let items = app.list_items(theme.muted);
    render_picker_list(
        &items,
        app.list_render_state(list_area.height),
        list_area,
        frame,
        theme,
    );
}

fn render_bottom(
    app: &mut SkillPickerApp,
    inner: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) -> u16 {
    let bottom_height = match app.mode() {
        SkillPickerMode::Filter => 1,
        SkillPickerMode::Argument => 3,
    };
    let bottom_y = inner.bottom().saturating_sub(bottom_height);
    if matches!(app.mode(), SkillPickerMode::Argument) {
        frame.write_line_with_fallback_style(
            LocalRect::terminal(Rect::new(inner.x, bottom_y, inner.width, 1)),
            &Line::from_spans(vec![Span::styled(
                "Invocation arguments/display text:",
                theme.muted,
            )]),
            picker_base_style(theme),
        );
        let input_area = Rect::new(inner.x, bottom_y.saturating_add(1), inner.width, 1);
        app.argument_mut()
            .set_content_area(input_area, &text_input_flow::single_line_policy());
        super::picker_render::paint_picker_input(
            app.argument_mut(),
            input_area,
            "Optional arguments",
            frame,
            theme,
        );
    } else {
        frame.write_line_with_fallback_style(
            LocalRect::terminal(Rect::new(inner.x, bottom_y, inner.width, 1)),
            &Line::from_spans(vec![Span::styled(
                "Use / palette to reopen. Activation persists for this session.",
                theme.muted,
            )]),
            picker_base_style(theme),
        );
    }
    bottom_y
}
