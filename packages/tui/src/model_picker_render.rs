//! TUI model picker rendering.

use bmux_tui::frame::Frame;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

use super::model_picker::ModelPickerApp;
use super::picker_render::{
    picker_list_area, render_picker_chrome, render_picker_list, render_picker_status,
};
use super::render::TuiTheme;

/// Render the model picker.
pub fn render_model_picker(app: &mut ModelPickerApp, frame: &mut Frame<'_>, theme: TuiTheme) {
    let Some((inner, list_y)) = render_picker_chrome(
        " Models ",
        &Line::from_spans(vec![
            Span::styled("Select model", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(
                "  Enter selects  Esc cancels  s sort  S reverse  i ignore  u unignore  I ignored",
            ),
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
    let items = app.list_items();
    render_picker_list(&items, app.list_state_mut(), list_area, frame);
}
