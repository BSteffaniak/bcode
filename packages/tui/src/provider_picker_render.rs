//! TUI provider picker rendering.

use bmux_tui::frame::Frame;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;

use super::picker_render::{picker_list_area, render_picker_chrome, render_picker_list};
use super::provider_picker::ProviderPickerApp;
use super::render::TuiTheme;

/// Render the provider picker.
pub fn render_provider_picker(app: &mut ProviderPickerApp, frame: &mut Frame<'_>, theme: TuiTheme) {
    let Some((inner, list_y)) = render_picker_chrome(
        " Providers ",
        &Line::from_spans(vec![
            Span::styled(
                "Select model provider",
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  Enter selects  Esc cancels"),
        ]),
        app.filter_mut(),
        "Filter providers",
        frame,
        theme,
    ) else {
        return;
    };

    let Some(list_area) = picker_list_area(inner, list_y, inner.bottom()) else {
        return;
    };
    let items = app.list_items();
    render_picker_list(&items, app.list_state_mut(), list_area, frame);
}
