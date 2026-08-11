//! Interactive theme picker rendering.

use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::list::{List, ListItem};
use bmux_tui::prelude::{Line, Span, StatefulWidget};
use bmux_tui::style::Modifier;
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing};

use super::render::TuiTheme;
use super::theme_picker::ThemePickerState;

const PREVIEW_BREAKPOINT: u16 = 72;
const PREVIEW_GAP: u16 = 2;
const PREVIEW_MIN_WIDTH: u16 = 26;

/// Render the theme picker overlay.
pub fn render_theme_picker(picker: &mut ThemePickerState, frame: &mut Frame<'_>, theme: TuiTheme) {
    let modal = theme_picker_modal(theme);
    modal.render(frame.area(), frame);
    let content = modal.content_area(frame.area());
    let diagnostics_height = u16::from(!picker.diagnostics().is_empty());
    let body = Rect::new(
        content.x,
        content.y,
        content.width,
        content.height.saturating_sub(diagnostics_height),
    );
    let (list_area, preview_area) = theme_picker_body_areas(body);
    let items = picker
        .entries()
        .iter()
        .map(|entry| {
            let current = if entry.selected {
                "current"
            } else {
                &entry.source
            };
            let variants = match (entry.has_dark_variant, entry.has_light_variant) {
                (true, true) => "dark+light",
                (true, false) => "dark",
                (false, true) => "light",
                (false, false) => "fixed",
            };
            ListItem::new(Line::from_spans(vec![
                Span::styled(entry.display_name.clone(), theme.text),
                Span::styled(
                    format!(
                        "  {}  [{current}; {variants}; {}]",
                        entry.id, entry.validation
                    ),
                    theme.muted,
                ),
            ]))
        })
        .collect::<Vec<_>>();
    let list_state = picker.list_render_state(list_area.height);
    List::new(&items)
        .style(theme.text)
        .highlight_symbol("› ")
        .selected_style(theme.selection)
        .render(list_area, frame, list_state);
    if let Some(preview_area) = preview_area {
        render_preview(picker, preview_area, frame, theme);
    }
    if let Some(diagnostic) = picker.diagnostics().first() {
        frame.write_line_with_fallback_style(
            Rect::new(
                content.x,
                content.bottom().saturating_sub(1),
                content.width,
                1,
            ),
            &Line::from_spans(vec![Span::styled(
                format!("Skipped: {diagnostic}"),
                theme.muted,
            )]),
            theme.overlay,
        );
    }
}

fn render_preview(picker: &ThemePickerState, area: Rect, frame: &mut Frame<'_>, theme: TuiTheme) {
    frame.fill(area, " ", theme.raised);
    let Some(entry) = picker.selected_entry() else {
        return;
    };
    let variants = match (entry.has_dark_variant, entry.has_light_variant) {
        (true, true) => "dark + light",
        (true, false) => "dark variant",
        (false, true) => "light variant",
        (false, false) => "fixed palette",
    };
    let rows = [
        Line::from_spans(vec![Span::styled(
            entry.display_name.clone(),
            theme.focused.add_modifier(Modifier::BOLD),
        )]),
        Line::from_spans(vec![Span::styled(
            format!("{} · {} · {variants}", entry.id, entry.source),
            theme.muted,
        )]),
        Line::from_spans(vec![
            Span::styled("You  ", theme.focused.add_modifier(Modifier::BOLD)),
            Span::styled("Please review this change.", theme.text),
        ]),
        Line::from_spans(vec![
            Span::styled("Bcode  ", theme.info.add_modifier(Modifier::BOLD)),
            Span::styled("I’ll inspect the semantic diff.", theme.text),
        ]),
        Line::from_spans(vec![
            Span::styled("● running  ", theme.info),
            Span::styled("filesystem.read", theme.text),
        ]),
        Line::from_spans(vec![
            Span::styled("✓ succeeded  ", theme.success),
            Span::styled("2 files", theme.muted),
        ]),
        Line::from_spans(vec![
            Span::styled("fn ", theme.info),
            Span::styled("theme_preview", theme.focused),
            Span::styled("()", theme.text),
        ]),
        Line::from_spans(vec![
            Span::styled(" selected ", theme.selection),
            Span::styled("  focused ", theme.focused),
        ]),
        Line::from_spans(vec![
            Span::styled("██", theme.focused),
            Span::styled(" ██", theme.info),
            Span::styled(" ██", theme.success),
            Span::styled(" ██", theme.warning),
            Span::styled(" ██", theme.error),
            Span::styled("  resolved semantic palette", theme.muted),
        ]),
    ];
    for (offset, row) in rows.iter().take(usize::from(area.height)).enumerate() {
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        frame.write_line_with_fallback_style(
            Rect::new(area.x, area.y.saturating_add(offset), area.width, 1),
            row,
            theme.raised,
        );
    }
}

const fn theme_picker_body_areas(area: Rect) -> (Rect, Option<Rect>) {
    if area.width < PREVIEW_BREAKPOINT || area.height < 8 {
        return (area, None);
    }
    let list_width = area.width.saturating_mul(5) / 9;
    let preview_x = area
        .x
        .saturating_add(list_width)
        .saturating_add(PREVIEW_GAP);
    let preview_width = area.right().saturating_sub(preview_x);
    if preview_width < PREVIEW_MIN_WIDTH {
        return (area, None);
    }
    (
        Rect::new(area.x, area.y, list_width, area.height),
        Some(Rect::new(preview_x, area.y, preview_width, area.height)),
    )
}

/// Return the picker list area for rendering and hit testing.
#[must_use]
pub fn theme_picker_list_area(frame_area: Rect, theme: TuiTheme) -> Rect {
    let modal = theme_picker_modal(theme);
    let area = modal.content_area(frame_area);
    theme_picker_body_areas(area).0
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
        ModalSizing::new(Size::new(48, 8), Size::new(112, 20), Insets::all(2)),
        theme.modal_theme(),
    )
    .title(" Themes · ↑/↓ preview · enter apply · esc cancel ")
    .padding(Insets::new(1, 1, 1, 1))
    .placement(ModalPlacement::UpperThird)
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};

    use super::*;
    use crate::theme::ThemeCatalogEntry;

    fn catalog_entry(id: &str, selected: bool) -> ThemeCatalogEntry {
        ThemeCatalogEntry {
            id: id.to_owned(),
            display_name: id.to_owned(),
            source: "bundled".to_owned(),
            has_dark_variant: true,
            has_light_variant: true,
            validation: "valid".to_owned(),
            selected,
        }
    }

    fn picker() -> ThemePickerState {
        ThemePickerState::new(vec![catalog_entry("bcode", true)], Vec::new())
    }

    #[test]
    fn picker_hit_testing_excludes_preview_and_diagnostic_rows() {
        let theme = TuiTheme::for_theme_id("bcode-dark");
        let frame_area = Rect::new(0, 0, 120, 24);
        let mut picker = picker();
        picker = ThemePickerState::new(picker.entries().to_vec(), vec!["invalid theme".to_owned()]);
        let list = theme_picker_list_area(frame_area, theme);
        let preview_click = MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(list.right().saturating_add(3), list.y),
        );
        assert_eq!(
            theme_picker_row(&picker, preview_click, frame_area, theme),
            None
        );
        let diagnostic_click = MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(list.x, list.bottom().saturating_sub(1)),
        );
        assert_eq!(
            theme_picker_row(&picker, diagnostic_click, frame_area, theme),
            None
        );
    }

    #[test]
    fn opaque_picker_frame_exercises_modal_surface_and_selection_hierarchy() {
        let theme = TuiTheme::for_theme_id("bcode-dark");
        let area = Rect::new(0, 0, 120, 24);
        let mut picker = picker();
        let mut buffer = Buffer::empty(area);
        render_theme_picker(&mut picker, &mut Frame::new(&mut buffer), theme);
        let modal = theme_picker_modal(theme);
        let content = modal.content_area(area);
        let (list, preview) = theme_picker_body_areas(content);
        let preview = preview.expect("wide picker has preview pane");
        let modal_surface = Point::new(list.right(), content.y);

        assert_eq!(
            buffer
                .get(modal_surface)
                .expect("modal content cell")
                .style
                .bg,
            theme.overlay.bg
        );
        assert_eq!(
            buffer
                .get(Point::new(preview.x, preview.y))
                .expect("preview cell")
                .style
                .bg,
            theme.raised.bg
        );
        assert!(buffer.cells().iter().any(|cell| {
            cell.style.bg == theme.selection.bg && cell.style.fg == theme.selection.fg
        }));
        assert_ne!(theme.overlay.bg, theme.raised.bg);
    }

    #[test]
    fn wide_picker_renders_bounded_semantic_preview_and_narrow_picker_collapses() {
        let theme = TuiTheme::for_theme_id("bcode-dark");
        let mut wide_picker = picker();
        let wide_area = Rect::new(0, 0, 120, 24);
        let mut wide = Buffer::empty(wide_area);
        render_theme_picker(&mut wide_picker, &mut Frame::new(&mut wide), theme);
        let text = (0..wide_area.height)
            .filter_map(|row| wide.row_symbols(row))
            .collect::<String>();
        assert!(text.contains("Please review this change."));
        assert!(text.contains("filesystem.read"));

        let mut narrow_picker = picker();
        let narrow_area = Rect::new(0, 0, 60, 16);
        let mut narrow = Buffer::empty(narrow_area);
        render_theme_picker(&mut narrow_picker, &mut Frame::new(&mut narrow), theme);
        let text = (0..narrow_area.height)
            .filter_map(|row| narrow.row_symbols(row))
            .collect::<String>();
        assert!(!text.contains("Please review this change."));
        assert!(narrow.get(Point::new(0, 0)).is_some());
    }

    #[test]
    fn short_picker_collapses_preview_and_bounds_diagnostics() {
        let theme = TuiTheme::for_theme_id("bcode-dark");
        let area = Rect::new(0, 0, 120, 8);
        let mut picker = ThemePickerState::new(
            vec![catalog_entry("bcode", true)],
            vec!["a rejected candidate with a deliberately long diagnostic".to_owned()],
        );
        let mut buffer = Buffer::empty(area);
        render_theme_picker(&mut picker, &mut Frame::new(&mut buffer), theme);
        let text = (0..area.height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<String>();

        assert!(!text.contains("Please review this change."));
        assert!(text.contains("Skipped:"));
        assert!(
            buffer
                .get(Point::new(area.right() - 1, area.bottom() - 1))
                .is_some()
        );
    }

    #[test]
    fn scrolled_picker_hit_testing_uses_rendered_list_offset() {
        let theme = TuiTheme::for_theme_id("bcode-dark");
        let area = Rect::new(0, 0, 60, 12);
        let entries = (0..24)
            .map(|index| catalog_entry(&format!("theme-{index}"), index == 0))
            .collect();
        let mut picker = ThemePickerState::new(entries, Vec::new());
        for _ in 0..23 {
            let _ = picker.handle_key(bmux_keyboard::KeyStroke::simple(
                bmux_keyboard::KeyCode::Down,
            ));
        }
        let mut buffer = Buffer::empty(area);
        render_theme_picker(&mut picker, &mut Frame::new(&mut buffer), theme);
        let list = theme_picker_list_area(area, theme);
        let expected = picker.list_offset();
        assert!(expected > 0);

        let click = MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(list.x, list.y),
        );
        assert_eq!(
            theme_picker_row(&picker, click, area, theme),
            Some((expected, false))
        );
        assert_eq!(
            picker.select_row(expected),
            crate::theme_picker::ThemePickerOutcome::Preview(format!("theme-{expected}"))
        );

        let release = MouseEvent::new(
            MouseEventKind::Up(MouseButton::Left),
            Point::new(list.x, list.y),
        );
        assert_eq!(
            theme_picker_row(&picker, release, area, theme),
            Some((expected, true))
        );
        assert_eq!(
            picker.activate_row(expected),
            crate::theme_picker::ThemePickerOutcome::Apply(format!("theme-{expected}"))
        );
    }
}
