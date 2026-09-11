//! TUI slash completion rendering.

use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::Modifier;
use bmux_tui_components::picker_frame::{
    PickerFrame, PickerFrameComponent, PickerFramePolicy, PickerFrameStyles,
};

use super::render::TuiTheme;
use super::slash_palette::{SlashItem, SlashPalette};

const POPUP_MAX_HEIGHT: u16 = 8;
const POPUP_MAX_WIDTH: u16 = 88;
const POPUP_SIDE_MARGIN: u16 = 2;

/// Render slash completions above the composer.
pub fn render_palette(
    palette: &SlashPalette,
    composer_content_area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) {
    let frame_area = Rect::new(0, 0, frame.area().width, frame.area().height);
    let composer = composer_panel_area(composer_content_area);
    let Some(area) = slash_palette_area(frame_area, composer, palette.item_count()) else {
        return;
    };

    let title = format!(
        " Commands & Skills ({} skills) · type to search · ↑/↓ browse ",
        palette.skill_count()
    );
    let shell = PickerFrame::new()
        .title(&title)
        .policy(PickerFramePolicy {
            chrome: true,
            background: true,
            header: false,
            input: false,
            footer: false,
            margin: Insets::all(0),
            padding: Insets::new(0, 1, 0, 1),
            min_size: Size::new(area.width, area.height),
            max_size: Size::new(area.width, area.height),
            placement: bmux_tui_components::picker_frame::PickerFramePlacement::Center,
        })
        .styles(PickerFrameStyles {
            border: theme.border,
            background: theme.raised,
            header: theme.raised,
            input: theme.raised,
            list: theme.raised,
            footer: theme.raised,
        });
    let component = PickerFrameComponent::new("slash", shell, TextBlock::new(""));
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| component.paint(&layout, cx),
    );
    let panel = &layout.children[0];
    let list = panel
        .node
        .children
        .iter()
        .find(|child| child.node.id.as_str() == "slash.chrome.list")
        .expect("list child");
    let inner = Rect::new(
        area.x
            .saturating_add(u16::try_from(panel.x.saturating_add(list.x)).unwrap_or(u16::MAX)),
        area.y + u16::try_from(panel.y + list.y).unwrap_or(u16::MAX),
        u16::try_from(list.node.size.width).unwrap_or(u16::MAX),
        u16::try_from(list.node.size.height).unwrap_or(u16::MAX),
    );
    if inner.is_empty() {
        return;
    }
    for (row, item) in palette.visible_items(usize::from(inner.height)).enumerate() {
        let Ok(row) = u16::try_from(row) else {
            break;
        };
        let y = inner.y.saturating_add(row);
        let selected = item.source_index == palette.selected_index();
        frame.write_line(
            LocalRect::terminal(Rect::new(inner.x, y, inner.width, 1)),
            &slash_item_line(item.item, selected, inner.width, theme),
        );
    }
}

pub fn slash_palette_area(
    frame_area: Rect,
    composer_area: Rect,
    item_count: usize,
) -> Option<Rect> {
    if item_count == 0 || composer_area.y == 0 || frame_area.width < 8 {
        return None;
    }
    let height = POPUP_MAX_HEIGHT
        .min(usize_to_u16_saturating(item_count).saturating_add(2))
        .min(composer_area.y.saturating_sub(frame_area.y));
    if height == 0 {
        return None;
    }
    let width = frame_area
        .width
        .saturating_sub(POPUP_SIDE_MARGIN.saturating_mul(2))
        .clamp(8, POPUP_MAX_WIDTH);
    let max_x = frame_area
        .x
        .saturating_add(frame_area.width.saturating_sub(width));
    let x = composer_area.x.saturating_add(POPUP_SIDE_MARGIN).min(max_x);
    let y = composer_area.y.saturating_sub(height);
    Some(Rect::new(x, y, width, height))
}

pub fn slash_palette_row_from_mouse(
    frame_area: Rect,
    composer_content_area: Rect,
    mouse_x: u16,
    mouse_y: u16,
    item_count: usize,
) -> Option<usize> {
    let composer = composer_panel_area(composer_content_area);
    let area = slash_palette_area(frame_area, composer, item_count)?;
    let inner = area.inset(Insets::new(1, 1, 1, 1));
    if mouse_x < inner.x
        || mouse_x >= inner.right()
        || mouse_y < inner.y
        || mouse_y >= inner.bottom()
    {
        return None;
    }
    Some(usize::from(mouse_y.saturating_sub(inner.y)))
}

pub const fn composer_panel_area(content_area: Rect) -> Rect {
    Rect::new(
        content_area.x.saturating_sub(2),
        content_area.y.saturating_sub(1),
        content_area.width.saturating_add(4),
        content_area.height.saturating_add(2),
    )
}

fn slash_item_line(item: &SlashItem, selected: bool, width: u16, theme: TuiTheme) -> Line {
    let base = if selected {
        theme.selection.add_modifier(Modifier::BOLD)
    } else {
        theme.text
    };
    let badge_style = theme.border.add_modifier(Modifier::BOLD);
    let available = usize::from(width.saturating_sub(15));
    Line::from_spans(vec![
        Span::styled(if selected { "› " } else { "  " }, base),
        Span::styled(if item.is_skill() { "skill" } else { " cmd " }, badge_style),
        Span::styled("  ", base),
        Span::styled(
            truncate_end(item.command(), available.min(30)),
            base.add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", base),
        Span::styled(
            truncate_end(item.description(), available.saturating_sub(30)),
            if selected { base } else { theme.muted },
        ),
    ])
    .truncate(usize::from(width))
}

fn truncate_end(value: &str, width: usize) -> String {
    bmux_tui::text_width::truncate_to_display_width(value, width)
}

fn usize_to_u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}
