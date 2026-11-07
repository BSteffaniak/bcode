//! Interactive profile picker rendering.
//!
//! The preview pane renders the picker's sample transcript through the real
//! transcript row pipeline under the live structural policy, so it shows
//! exactly what the selected profile does to tools and messages.

use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::Modifier;
use bmux_tui_components::modal_frame::{
    ModalFrame, ModalFrameComponent, ModalPlacement, ModalSizing,
};

use super::profile_picker::ProfilePickerState;
use super::render::TuiTheme;
use super::structure::StructureSnapshot;

const PREVIEW_BREAKPOINT: u16 = 72;
const PREVIEW_GAP: u16 = 2;
const PREVIEW_MIN_WIDTH: u16 = 36;
const LIST_MAX_WIDTH: u16 = 34;
/// Rows above the sample transcript: name, description, rule.
const PREVIEW_HEADER_ROWS: u16 = 3;

/// Render the profile picker with hints from effective bindings.
pub fn render_profile_picker_with_keymap(
    picker: &mut ProfilePickerState,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
    structure: StructureSnapshot,
    keymap: &super::keymap::BmuxKeyMap,
) {
    let modal = profile_picker_modal(theme).title(format!(
        " Profiles · {} preview · {} apply · {} cancel ",
        keymap.hint("profile_picker", "tui.control.down"),
        keymap.hint("profile_picker", "tui.control.submit"),
        keymap.hint("profile_picker", "tui.control.cancel")
    ));
    let area = Rect::new(0, 0, frame.area().width, frame.area().height);
    let content = modal.content_area(area);
    let component = ModalFrameComponent::new("profile.modal", modal, TextBlock::new(""));
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    component.paint(&layout, frame);
    let (list_area, preview_area) = profile_picker_body_areas(content);
    let original = picker.original().map(str::to_owned);
    let items = picker
        .entries()
        .iter()
        .map(|entry| {
            let mut spans = vec![Span::styled(entry.display_name.clone(), theme.text)];
            if entry.id == original {
                spans.push(Span::styled("  current", theme.muted));
            }
            Line::from_spans(spans)
        })
        .collect::<Vec<_>>();
    let list_state = picker.list_render_state(list_area.height);
    super::picker_render::render_picker_list(&items, list_state, list_area, frame, theme);
    if let Some(preview_area) = preview_area {
        render_preview(picker, preview_area, frame, theme, structure);
    }
}

fn render_preview(
    picker: &ProfilePickerState,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
    structure: StructureSnapshot,
) {
    frame.fill(LocalRect::terminal(area), " ", theme.raised);
    let Some(entry) = picker.selected_entry() else {
        return;
    };
    let header = [
        Line::from_spans(vec![
            Span::styled(
                entry.display_name.clone(),
                theme.focused.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                entry
                    .id
                    .as_deref()
                    .map_or_else(String::new, |id| format!("  {id}")),
                theme.muted,
            ),
        ]),
        Line::from_spans(vec![Span::styled(
            entry.description.clone().unwrap_or_default(),
            theme.muted,
        )]),
        Line::from_spans(vec![Span::styled(
            "─".repeat(usize::from(area.width)),
            theme.border,
        )]),
    ];
    let sample = super::render::sample_transcript_rows(picker.sample(), area.width, structure);
    for (offset, row) in header
        .iter()
        .chain(sample.iter())
        .take(usize::from(area.height))
        .enumerate()
    {
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        frame.write_line_with_fallback_style(
            LocalRect::terminal(Rect::new(
                area.x,
                area.y.saturating_add(offset),
                area.width,
                1,
            )),
            row,
            theme.raised,
        );
    }
}

fn profile_picker_body_areas(area: Rect) -> (Rect, Option<Rect>) {
    if area.width < PREVIEW_BREAKPOINT || area.height <= PREVIEW_HEADER_ROWS + 2 {
        return (area, None);
    }
    let list_width = (area.width / 3).min(LIST_MAX_WIDTH);
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
pub fn profile_picker_list_area(frame_area: Rect, theme: TuiTheme) -> Rect {
    let modal = profile_picker_modal(theme);
    profile_picker_body_areas(modal.content_area(frame_area)).0
}

/// Resolve an absolute picker row from a left mouse event.
#[must_use]
pub fn profile_picker_row(
    picker: &ProfilePickerState,
    mouse: MouseEvent,
    frame_area: Rect,
    theme: TuiTheme,
) -> Option<(usize, bool)> {
    let activate = matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left));
    if !activate && !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return None;
    }
    let area = profile_picker_list_area(frame_area, theme);
    if !area.contains(mouse.position) {
        return None;
    }
    let relative = usize::from(mouse.position.y.saturating_sub(area.y));
    let row = picker.list_offset().saturating_add(relative);
    (row < picker.entries().len()).then_some((row, activate))
}

fn profile_picker_modal(theme: TuiTheme) -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(Size::new(40, 8), Size::new(140, 40), Insets::all(2)),
        theme.modal_theme(),
    )
    .chrome(crate::render::dialog_chrome())
    .title(" Profiles ")
    .padding(Insets::new(1, 1, 1, 1))
    .placement(ModalPlacement::Centered)
}

#[cfg(test)]
mod tests {
    use bcode_presentation_policy::models::{LayerSource, PresentationSection, RuleLayer};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Point;

    use super::*;
    use crate::structure::StructureState;
    use crate::theme::ProfileCatalogEntry;

    fn picker() -> ProfilePickerState {
        ProfilePickerState::new(
            vec![ProfileCatalogEntry {
                id: "zen".to_owned(),
                display_name: "Zen".to_owned(),
                description: Some("Focus mode".to_owned()),
                selected: true,
            }],
            Some("zen".to_owned()),
        )
    }

    fn snapshot(rules: &str) -> StructureSnapshot {
        let mut state = StructureState::default();
        state.set_layers(&[RuleLayer {
            source: LayerSource::Theme("test".into()),
            rules: toml::from_str::<PresentationSection>(rules).unwrap().rules,
        }]);
        state.snapshot()
    }

    fn render(area: Rect, structure: StructureSnapshot) -> (Buffer, String) {
        let theme = TuiTheme::for_theme_id("bcode-dark");
        let mut buffer = Buffer::empty(area);
        render_profile_picker_with_keymap(
            &mut picker(),
            &mut PaintCx::new(&mut Frame::new(&mut buffer)),
            theme,
            structure,
            &crate::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
        );
        let text = (0..area.height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n");
        (buffer, text)
    }

    #[test]
    fn preview_reflects_the_structural_policy() {
        let area = Rect::new(0, 0, 130, 44);
        let (_, expanded) = render(area, snapshot(""));
        let (_, collapsed) = render(
            area,
            snapshot(
                "[[rules]]\nmatch = { kind = \"tool\" }\nset = { disclosure = \"collapsed\" }",
            ),
        );
        assert!(expanded.contains("Focus mode"), "{expanded}");
        assert!(expanded.contains("let row = 1"), "{expanded}");
        assert!(!collapsed.contains("let row = 1"), "{collapsed}");
        assert!(collapsed.contains("▸ "), "{collapsed}");
        // The visibility floor keeps the failure summarized and the decision open.
        assert!(collapsed.contains("E0599"), "{collapsed}");
    }

    #[test]
    fn preview_leaves_the_active_row_snapshot_untouched() {
        let area = Rect::new(0, 0, 130, 44);
        let item = bcode_tui_components::transcript::item::tool_result_item(
            "c1",
            Some("read"),
            None,
            "body",
            false,
        );
        let before = crate::render::structure_signature(&item);
        let _ = render(
            area,
            snapshot("[[rules]]\nset = { disclosure = \"collapsed\" }"),
        );
        assert_eq!(crate::render::structure_signature(&item), before);
    }

    #[test]
    fn narrow_and_short_pickers_drop_the_preview_and_stay_in_bounds() {
        for area in [
            Rect::new(0, 0, 60, 20),
            Rect::new(0, 0, 120, 9),
            Rect::new(0, 0, 8, 3),
        ] {
            let (buffer, text) = render(area, snapshot(""));
            assert!(!text.contains("Focus mode"), "{area:?}: {text}");
            assert!(
                buffer
                    .get(Point::new(area.right() - 1, area.bottom() - 1))
                    .is_some()
            );
        }
    }

    #[test]
    fn hit_testing_selects_list_rows_only() {
        let theme = TuiTheme::for_theme_id("bcode-dark");
        let frame_area = Rect::new(0, 0, 130, 44);
        let picker = picker();
        let list = profile_picker_list_area(frame_area, theme);
        let row = |x, y, kind| {
            profile_picker_row(
                &picker,
                MouseEvent::new(kind, Point::new(x, y)),
                frame_area,
                theme,
            )
        };
        assert_eq!(
            row(list.x, list.y + 1, MouseEventKind::Down(MouseButton::Left)),
            Some((1, false))
        );
        assert_eq!(
            row(list.x, list.y, MouseEventKind::Up(MouseButton::Left)),
            Some((0, true))
        );
        assert_eq!(
            row(
                list.right() + 3,
                list.y,
                MouseEventKind::Down(MouseButton::Left)
            ),
            None
        );
        assert_eq!(
            row(list.x, list.y + 5, MouseEventKind::Down(MouseButton::Left)),
            None
        );
    }
}
