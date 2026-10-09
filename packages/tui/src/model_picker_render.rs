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
    app.set_rendered_item_area(None);
    app.set_check_consent_visible(false);
    let scope = if app.mode() == ModelPickerMode::Filter {
        "model_picker"
    } else {
        "model_picker_actions"
    };
    let help = format!(
        "  {} selects  {} cancels  {} filter  {} sort  {} reverse  {} ignore  {} unignore  {} ignored  {} check selected model (quota/cost)",
        keymap.hint(scope, "tui.control.submit"),
        keymap.hint(scope, "tui.control.cancel"),
        keymap.hint("model_picker_actions", "tui.model.filter"),
        keymap.hint("model_picker_actions", "tui.model.sort"),
        keymap.hint("model_picker_actions", "tui.model.reverseSort"),
        keymap.hint("model_picker_actions", "tui.model.ignore"),
        keymap.hint("model_picker_actions", "tui.model.unignore"),
        keymap.hint("model_picker_actions", "tui.model.showIgnored"),
        keymap.hint("model_picker_actions", "tui.model.check")
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
    if matches!(
        app.mode(),
        ModelPickerMode::ConfirmCheck | ModelPickerMode::Checking
    ) {
        render_check_warning(app, frame, theme, inner, list_y, bottom_y);
        return;
    }
    let Some(list_y) = render_selected_details(app, frame, theme, inner, list_y, bottom_y) else {
        return;
    };
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
    app.set_rendered_item_area(Some(item_area));
    let items = app.list_items(item_area.width, theme);
    render_picker_list(
        &items,
        app.list_render_state(item_area.height),
        item_area,
        frame,
        theme,
    );
}

fn render_selected_details(
    app: &ModelPickerApp,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
    inner: bmux_tui::geometry::Rect,
    mut list_y: u16,
    bottom_y: u16,
) -> Option<u16> {
    for detail in [
        app.selected_scope_summary(),
        app.selected_availability_summary(),
    ] {
        if list_y >= bottom_y {
            return None;
        }
        frame.write_line_with_fallback_style(
            LocalRect::terminal(bmux_tui::geometry::Rect::new(
                inner.x,
                list_y,
                inner.width,
                1,
            )),
            &Line::from(detail).truncate(usize::from(inner.width)),
            theme.muted,
        );
        list_y = list_y.saturating_add(1);
    }
    Some(list_y)
}

fn render_check_warning(
    app: &mut ModelPickerApp,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
    inner: bmux_tui::geometry::Rect,
    list_y: u16,
    bottom_y: u16,
) {
    let warning = if app.mode() == ModelPickerMode::ConfirmCheck {
        format!(
            "{}\nThis checks only the selected model, with a 20 second limit. Resize if this warning does not fit.",
            app.status()
        )
    } else {
        format!(
            "{}\nEsc closes this picker; the bounded request may still finish and consume quota.",
            app.status()
        )
    };
    let rows = warning
        .lines()
        .flat_map(|line| {
            bmux_tui::text::wrap_line_bounded(
                &Line::from(line),
                bmux_tui::text::TextWrapGeometry::uniform(usize::from(inner.width)),
                bmux_tui::text::TextWrap::Word,
            )
        })
        .collect::<Vec<_>>();
    let available_rows = usize::from(bottom_y.saturating_sub(list_y));
    app.set_check_consent_visible(inner.width > 0 && rows.len() <= available_rows);
    for (offset, row) in rows.iter().take(available_rows).enumerate() {
        frame.write_line_with_fallback_style(
            LocalRect::terminal(bmux_tui::geometry::Rect::new(
                inner.x,
                list_y.saturating_add(u16::try_from(offset).unwrap_or(u16::MAX)),
                inner.width,
                1,
            )),
            row,
            theme.warning,
        );
    }
}
