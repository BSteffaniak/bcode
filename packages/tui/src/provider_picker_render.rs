//! TUI provider picker rendering.

use bmux_tui::paint::PaintCx;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;

use super::picker_render::{picker_list_area, render_picker_chrome, render_picker_list};
use super::provider_picker::ProviderPickerApp;
use super::render::TuiTheme;

/// Render the provider picker.
#[cfg(test)]
pub fn render_provider_picker(
    app: &mut ProviderPickerApp,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) {
    render_provider_picker_with_keymap(
        app,
        frame,
        theme,
        &super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
    );
}

/// Render shortcuts from effective bindings.
pub fn render_provider_picker_with_keymap(
    app: &mut ProviderPickerApp,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
    keymap: &super::keymap::BmuxKeyMap,
) {
    let Some((inner, list_y)) = render_picker_chrome(
        " Providers ",
        &Line::from_spans(vec![
            Span::styled(
                "Select model provider",
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "  {} selects  {} cancels",
                keymap.hint("provider_picker", "tui.control.submit"),
                keymap.hint("provider_picker", "tui.control.cancel")
            )),
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
    let items = app.list_items(theme.muted);
    render_picker_list(
        &items,
        app.list_render_state(list_area.height),
        list_area,
        frame,
        theme,
    );
}
