//! TUI model picker rendering.

use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;

use super::model_picker::{ModelPickerApp, ModelPickerMode};
use super::picker_render::{
    picker_list_area, render_picker_chrome, render_picker_list, render_picker_status,
};
use super::render::TuiTheme;

/// Render the model picker.
///
/// Render shortcuts from effective bindings.
pub fn render_model_picker_with_keymap(
    app: &mut ModelPickerApp,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
    keymap: &super::keymap::BmuxKeyMap,
) {
    let scope = if app.mode() == ModelPickerMode::Actions {
        "model_picker_actions"
    } else {
        "model_picker"
    };
    let help = format!(
        "  {} selects  {} cancels  {} filter  {} sort  {} reverse  {} ignore  {} unignore  {} ignored",
        keymap.hint(scope, "tui.control.submit"),
        keymap.hint(scope, "tui.control.cancel"),
        keymap.hint("model_picker_actions", "tui.model.filter"),
        keymap.hint("model_picker_actions", "tui.model.sort"),
        keymap.hint("model_picker_actions", "tui.model.reverseSort"),
        keymap.hint("model_picker_actions", "tui.model.ignore"),
        keymap.hint("model_picker_actions", "tui.model.unignore"),
        keymap.hint("model_picker_actions", "tui.model.showIgnored")
    );
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
    let bottom_y = render_picker_status(inner, &status, theme.muted, frame, theme);
    let Some(list_area) = picker_list_area(inner, list_y, bottom_y) else {
        return;
    };
    if list_area.height == 0 {
        return;
    }
    frame.write_line_with_fallback_style(
        LocalRect::terminal(bmux_tui::geometry::Rect::new(
            list_area.x,
            list_area.y,
            list_area.width,
            1,
        )),
        &app.header_line(list_area.width, theme),
        super::picker_render::picker_base_style(theme),
    );
    let item_area = bmux_tui::geometry::Rect::new(
        list_area.x,
        list_area.y.saturating_add(1),
        list_area.width,
        list_area.height.saturating_sub(1),
    );
    let items = app.list_items(item_area.width, theme);
    render_picker_list(
        &items,
        app.list_render_state(item_area.height),
        item_area,
        frame,
        theme,
    );
}
