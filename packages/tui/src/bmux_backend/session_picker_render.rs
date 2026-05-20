//! BMUX backend session picker rendering.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::input::TextInput;
use bmux_tui::list::List;
use bmux_tui::prelude::{Line, Span, StatefulWidget, Style, Widget};
use bmux_tui::style::{Color, Modifier};

use super::session_picker::SessionPickerApp;

/// Render the session picker.
pub(super) fn render_picker(app: &mut SessionPickerApp, frame: &mut Frame<'_>) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }

    let panel = Panel::new()
        .border(Border::single().style(Style::new().fg(Color::Cyan)))
        .title(" Sessions ")
        .padding(Insets::new(1, 1, 1, 1));
    panel.render(area, frame);

    let inner = panel.inner_area(area);
    let header = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.write_line(
        header,
        &Line::from_spans(vec![
            Span::styled("Bcode sessions", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw("  Enter selects  Ctrl-N creates  Esc cancels"),
        ]),
    );

    let filter = Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1);
    TextInput::new(app.filter())
        .placeholder("Filter sessions")
        .render(filter, frame);

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
            Style::new().fg(Color::BrightBlack),
        )]),
    );

    let list_y = filter.y.saturating_add(2);
    let list_bottom = status.y;
    if list_bottom <= list_y {
        return;
    }
    let list_area = Rect::new(inner.x, list_y, inner.width, list_bottom - list_y);
    let items = app.list_items();
    let mut state = *app.list_state_mut();
    state.ensure_selected_visible(list_area.height, items.len());
    List::new(&items)
        .highlight_symbol("> ")
        .render(list_area, frame, &mut state);
    *app.list_state_mut() = state;
}
