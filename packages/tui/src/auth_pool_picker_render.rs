//! TUI auth-pool picker rendering.

use bmux_tui::paint::PaintCx;
use bmux_tui::prelude::{Line, Span};

use super::auth_pool_picker::AuthPoolPickerApp;
use super::picker_render::{picker_list_area, render_picker_chrome};
use super::render::TuiTheme;

/// Render the auth-pool profile picker.
#[cfg(test)]
pub fn render_auth_pool_picker(
    app: &mut AuthPoolPickerApp,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) {
    render_auth_pool_picker_with_keymap(
        app,
        frame,
        theme,
        &super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
    );
}

/// Render shortcuts from effective bindings.
pub fn render_auth_pool_picker_with_keymap(
    app: &mut AuthPoolPickerApp,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
    keymap: &super::keymap::BmuxKeyMap,
) {
    let mut unused_filter = super::text_input_flow::empty_state();
    let Some((inner, list_y)) = render_picker_chrome(
        " Auth subscriptions ",
        &Line::from_spans(vec![Span::raw(format!(
            "{} promotes  {} clears override  {} cancels",
            keymap.hint("auth_pool_picker", "tui.control.submit"),
            keymap.hint("auth_pool_picker", "tui.auth.clear"),
            keymap.hint("auth_pool_picker", "tui.control.cancel")
        ))]),
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
    app.render_list(&items, list_area, frame, theme);
}
