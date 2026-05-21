//! BMUX backend session picker rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, Style, Widget};
use bmux_tui::style::{Color, Modifier};

use super::picker_render::{render_picker_list, render_picker_panel};
use super::session_picker::{SessionPickerApp, SessionPickerMode};

/// Render the session picker.
pub(super) fn render_picker(app: &mut SessionPickerApp, frame: &mut Frame<'_>) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }

    let inner = render_picker_panel(" Sessions ", area, frame);
    let header = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.write_line(header, &header_line(app.mode()));

    let filter = Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1);
    match app.mode() {
        SessionPickerMode::Filter | SessionPickerMode::DeleteConfirm => {
            TextInput::new(app.filter())
                .placeholder("Filter sessions")
                .render(filter, frame);
        }
        SessionPickerMode::Rename => {
            TextInput::new(app.rename())
                .placeholder("New session name")
                .render(filter, frame);
        }
    }

    let status = Rect::new(
        inner.x,
        inner.bottom().saturating_sub(1),
        inner.width,
        u16::from(inner.height > 0),
    );
    frame.write_line(
        status,
        &Line::from_spans(vec![Span::styled(
            app.status().to_owned(),
            status_style(app.mode()),
        )]),
    );

    let list_y = filter.y.saturating_add(2);
    let list_bottom = status.y;
    if list_bottom <= list_y {
        return;
    }
    let list_area = Rect::new(inner.x, list_y, inner.width, list_bottom - list_y);
    let items = app.list_items();
    render_picker_list(&items, app.list_state_mut(), list_area, frame);
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
