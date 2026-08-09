//! TUI auth-pool picker rendering.

use bmux_tui::frame::Frame;
use bmux_tui::prelude::{Line, Span};

use super::auth_pool_picker::AuthPoolPickerApp;
use super::picker_render::{picker_list_area, render_picker_chrome, render_picker_list};
use super::render::TuiTheme;

/// Render the auth-pool profile picker.
pub fn render_auth_pool_picker(
    app: &mut AuthPoolPickerApp,
    frame: &mut Frame<'_>,
    theme: TuiTheme,
) {
    let mut unused_filter = super::text_input_flow::empty_state();
    let Some((inner, list_y)) = render_picker_chrome(
        " Auth subscriptions ",
        &Line::from_spans(vec![Span::raw(
            "Enter promotes  c clears override  Esc cancels",
        )]),
        &mut unused_filter,
        "",
        frame,
        theme,
    ) else {
        return;
    };
    let Some(list_area) = picker_list_area(inner, list_y, inner.bottom()) else {
        return;
    };
    let items = app.list_items(theme.muted, theme.focused);
    render_picker_list(&items, app.list_state_mut(), list_area, frame, theme);
}
