//! BMUX backend session picker rendering.

use bmux_text_edit::TextEditBuffer;
use bmux_tui::frame::Frame;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

use super::picker_render::{
    picker_list_area, render_picker_chrome, render_picker_list, render_picker_status,
};
use super::session_picker::{SessionPickerApp, SessionPickerMode};

/// Render the session picker.
pub(super) fn render_picker(app: &mut SessionPickerApp, frame: &mut Frame<'_>) {
    let Some((inner, list_y)) = render_picker_chrome(
        " Sessions ",
        &header_line(app.mode()),
        filter_input(app),
        input_placeholder(app.mode()),
        frame,
    ) else {
        return;
    };

    let bottom_y = render_picker_status(inner, app.status(), status_style(app.mode()), frame);
    let Some(list_area) = picker_list_area(inner, list_y, bottom_y) else {
        return;
    };
    let items = app.list_items();
    render_picker_list(&items, app.list_state_mut(), list_area, frame);
}

const fn filter_input(app: &SessionPickerApp) -> &TextEditBuffer {
    match app.mode() {
        SessionPickerMode::Filter | SessionPickerMode::DeleteConfirm => app.filter(),
        SessionPickerMode::Rename => app.rename(),
    }
}

const fn input_placeholder(mode: SessionPickerMode) -> &'static str {
    match mode {
        SessionPickerMode::Filter | SessionPickerMode::DeleteConfirm => "Filter sessions",
        SessionPickerMode::Rename => "New session name",
    }
}

fn header_line(mode: SessionPickerMode) -> Line {
    let help = match mode {
        SessionPickerMode::Filter => {
            "  Enter selects  Ctrl-N creates  Ctrl-R renames  Ctrl-D deletes  Esc cancels"
        }
        SessionPickerMode::Rename => "  Enter saves rename  Esc cancels",
        SessionPickerMode::DeleteConfirm => "  Y confirms delete  N/Esc cancels",
    };
    Line::from_spans(vec![
        Span::styled("Bcode sessions", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(help),
    ])
}

const fn status_style(mode: SessionPickerMode) -> Style {
    match mode {
        SessionPickerMode::DeleteConfirm => Style::new().fg(Color::Red),
        SessionPickerMode::Filter | SessionPickerMode::Rename => {
            Style::new().fg(Color::BrightBlack)
        }
    }
}
