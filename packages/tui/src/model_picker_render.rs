//! TUI model picker rendering.

use bmux_tui::frame::Frame;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

use super::model_picker::{ModelPickerApp, ModelPickerMode};
use super::picker_render::{
    picker_list_area, render_picker_chrome, render_picker_list, render_picker_status,
};
use super::render::TuiTheme;

/// Render the model picker.
pub fn render_model_picker(app: &mut ModelPickerApp, frame: &mut Frame<'_>, theme: TuiTheme) {
    let help = match app.mode() {
        ModelPickerMode::Actions => {
            "  Enter selects  / filter  Esc cancels  s sort  S reverse  i ignore  u unignore  I ignored"
        }
        ModelPickerMode::Filter => "  Typing filters  Enter selects  Esc exits filter",
    };
    let Some((inner, list_y)) = render_picker_chrome(
        " Models ",
        &Line::from_spans(vec![
            Span::styled("Select model", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(help),
        ]),
        app.filter_mut(),
        "Filter models",
        frame,
        theme,
    ) else {
        return;
    };

    let status = format!(
        "{} · {}{}",
        app.status(),
        app.sort_label(),
        if app.show_ignored() {
            " · ignored visible"
        } else {
            ""
        }
    );
    let bottom_y = render_picker_status(inner, &status, Style::new().fg(Color::BrightBlack), frame);
    let Some(list_area) = picker_list_area(inner, list_y, bottom_y) else {
        return;
    };
    if list_area.height == 0 {
        return;
    }
    frame.write_line_with_fallback_style(
        bmux_tui::geometry::Rect::new(list_area.x, list_area.y, list_area.width, 1),
        &app.header_line(list_area.width),
        super::picker_render::picker_base_style(),
    );
    let item_area = bmux_tui::geometry::Rect::new(
        list_area.x,
        list_area.y.saturating_add(1),
        list_area.width,
        list_area.height.saturating_sub(1),
    );
    let items = app.list_items(item_area.width);
    render_picker_list(&items, app.list_state_mut(), item_area, frame);
}
