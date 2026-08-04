//! Interactive theme picker rendering.

use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::list::{List, ListItem};
use bmux_tui::prelude::{Line, Span, StatefulWidget};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing};

use super::render::TuiTheme;
use super::theme_picker::ThemePickerState;

/// Render the theme picker overlay.
pub fn render_theme_picker(picker: &mut ThemePickerState, frame: &mut Frame<'_>, theme: TuiTheme) {
    let modal = theme_picker_modal(theme);
    modal.render(frame.area(), frame);
    let area = modal.content_area(frame.area());
    let items = picker
        .entries()
        .iter()
        .map(|entry| {
            let current = if entry.selected {
                "current"
            } else {
                &entry.source
            };
            ListItem::new(Line::from_spans(vec![
                Span::styled(entry.display_name.clone(), theme.text),
                Span::styled(
                    format!("  {}  [{current}; {}]", entry.id, entry.validation),
                    theme.muted,
                ),
            ]))
        })
        .collect::<Vec<_>>();
    let list_height = area
        .height
        .saturating_sub(u16::from(!picker.diagnostics().is_empty()));
    let list_area = Rect::new(area.x, area.y, area.width, list_height);
    List::new(&items)
        .style(theme.text)
        .highlight_symbol("› ")
        .selected_style(theme.selection)
        .render(list_area, frame, picker.list_mut());
    if let Some(diagnostic) = picker.diagnostics().first() {
        frame.write_line_with_fallback_style(
            Rect::new(
                area.x,
                area.y.saturating_add(list_height),
                area.width,
                u16::from(!picker.diagnostics().is_empty()),
            ),
            &Line::from_spans(vec![Span::styled(
                format!("Skipped: {diagnostic}"),
                theme.muted,
            )]),
            theme.text,
        );
    }
}

/// Return the picker list area for rendering and hit testing.
#[must_use]
pub fn theme_picker_list_area(frame_area: Rect, theme: TuiTheme) -> Rect {
    theme_picker_modal(theme).content_area(frame_area)
}

/// Resolve an absolute picker row from a left mouse event.
#[must_use]
pub fn theme_picker_row(
    picker: &ThemePickerState,
    mouse: MouseEvent,
    frame_area: Rect,
    theme: TuiTheme,
) -> Option<(usize, bool)> {
    let activate = matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left));
    if !activate && !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return None;
    }
    let mut area = theme_picker_list_area(frame_area, theme);
    if !picker.diagnostics().is_empty() {
        area.height = area.height.saturating_sub(1);
    }
    if !area.contains(mouse.position) {
        return None;
    }
    let relative = usize::from(mouse.position.y.saturating_sub(area.y));
    let row = picker.list_offset().saturating_add(relative);
    (row < picker.entries().len()).then_some((row, activate))
}

fn theme_picker_modal(theme: TuiTheme) -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(Size::new(48, 8), Size::new(76, 16), Insets::all(2)),
        theme.modal_theme(),
    )
    .title(" Themes · ↑/↓ preview · enter apply · esc cancel ")
    .padding(Insets::new(1, 1, 1, 1))
    .placement(ModalPlacement::UpperThird)
}
