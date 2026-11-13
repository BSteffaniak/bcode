//! TUI slash completion rendering.

use bcode_tui_theme::chrome::{MenuChrome, MenuFrame};
use bmux_tui::component::{Component, Constraints, LayoutCx, LayoutNode};
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::Modifier;
use bmux_tui::text_width::display_width;
use bmux_tui_components::picker_frame::{
    PickerFrame, PickerFrameComponent, PickerFramePolicy, PickerFrameStyles,
};

use super::render::TuiTheme;
use super::slash_palette::{SlashItem, SlashPalette};

const POPUP_MAX_HEIGHT: u16 = 8;
const POPUP_MAX_WIDTH: u16 = 88;
const POPUP_SIDE_MARGIN: u16 = 2;

fn palette_component(area: Rect, styles: PickerFrameStyles) -> PickerFrameComponent<'static> {
    let shell = PickerFrame::new()
        .chrome(crate::render::component_chrome())
        .title(" Commands ")
        .policy(PickerFramePolicy {
            chrome: true,
            background: true,
            header: false,
            input: false,
            footer: false,
            margin: Insets::all(0),
            padding: Insets::new(0, 1, 0, 1),
            min_size: area.size(),
            max_size: area.size(),
            placement: bmux_tui_components::picker_frame::PickerFramePlacement::Center,
        })
        .styles(styles);
    PickerFrameComponent::new("slash", shell, TextBlock::new(""))
}

// Both paint and mouse translation consume the public component's measured list slot.
fn list_area(area: Rect, layout: &LayoutNode) -> Option<Rect> {
    let panel = layout.children.first()?;
    let list = panel
        .node
        .children
        .iter()
        .find(|child| child.node.id.as_str() == "slash.chrome.list")?;
    Some(Rect::new(
        area.x
            .saturating_add(u16::try_from(panel.x.saturating_add(list.x)).ok()?),
        area.y
            .saturating_add(u16::try_from(panel.y.saturating_add(list.y)).ok()?),
        u16::try_from(list.node.size.width).ok()?,
        u16::try_from(list.node.size.height).ok()?,
    ))
}

/// Render slash completions above the composer, as the theme's `menus` chrome draws them.
pub fn render_palette(
    palette: &SlashPalette,
    composer_content_area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
    menus: MenuChrome,
) {
    let frame_area = Rect::new(0, 0, frame.area().width, frame.area().height);
    let composer = composer_panel_area(composer_content_area);
    let Some(area) = slash_palette_area(frame_area, composer, palette.item_count(), menus) else {
        return;
    };
    if menus.frame == MenuFrame::Plain {
        render_plain(palette, area, frame, theme, menus);
        return;
    }
    let component = palette_component(
        area,
        PickerFrameStyles {
            border: theme.raised.patch(theme.focused),
            background: theme.raised,
            header: theme.raised,
            input: theme.raised,
            list: theme.raised,
            footer: theme.raised,
        },
    );
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| component.paint(&layout, cx),
    );
    let Some(inner) = list_area(area, &layout).filter(|inner| !inner.is_empty()) else {
        return;
    };
    for (row, item) in palette.visible_items(usize::from(inner.height)).enumerate() {
        let Ok(row) = u16::try_from(row) else {
            break;
        };
        let row_area = LocalRect::terminal(Rect::new(
            inner.x,
            inner.y.saturating_add(row),
            inner.width,
            1,
        ));
        let selected = item.source_index == palette.selected_index();
        let background = if selected {
            theme.raised.patch(theme.selection)
        } else {
            theme.raised
        };
        frame.write_line_with_fallback_style(
            row_area,
            &slash_item_line(item.item, selected, inner.width, theme, menus),
            background,
        );
    }
}

/// Plain menus: each row is the marker column, the command padded to the widest command, and
/// the description; the selected row takes the focused style, the others the muted style.
fn render_plain(
    palette: &SlashPalette,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
    menus: MenuChrome,
) {
    let marker = menu_marker(menus);
    let marker_cells = display_width(&marker);
    let items: Vec<_> = palette.visible_items(usize::from(area.height)).collect();
    // One command column for every visible row, bounded to half the menu.
    let column = items
        .iter()
        .map(|item| display_width(item.item.command()))
        .max()
        .unwrap_or(0)
        .min(usize::from(area.width) / 2);
    for (row, item) in items.into_iter().enumerate() {
        let Ok(row) = u16::try_from(row) else {
            break;
        };
        let selected = item.source_index == palette.selected_index();
        let style = if selected { theme.focused } else { theme.muted };
        let command = item.item.command();
        let padding = column.saturating_sub(display_width(command));
        let mut spans = vec![
            Span::styled(
                if selected {
                    format!("{marker} ")
                } else {
                    " ".repeat(marker_cells + 1)
                },
                style,
            ),
            Span::styled(format!("{command}{}", " ".repeat(padding)), style),
        ];
        if menus.tags.is_shown() {
            spans.push(Span::styled(
                if item.item.is_skill() {
                    "  [skill]"
                } else {
                    "  [cmd]"
                },
                style,
            ));
        }
        spans.push(Span::styled(
            format!("  {}", item.item.description()),
            style,
        ));
        frame.write_line(
            LocalRect::terminal(Rect::new(area.x, area.y.saturating_add(row), area.width, 1)),
            &Line::from_spans(spans).truncate(usize::from(area.width)),
        );
    }
}

/// The selected-row glyph of a menu.
fn menu_marker(menus: MenuChrome) -> String {
    menus
        .marker
        .map_or_else(|| "›".to_owned(), |marker| marker.as_str().to_owned())
}

/// Resolve bounded popup placement above the composer panel: a framed menu adds its border
/// rows to the rows it lists; a plain menu is its rows.
pub fn slash_palette_area(
    frame_area: Rect,
    composer_area: Rect,
    item_count: usize,
    menus: MenuChrome,
) -> Option<Rect> {
    if item_count == 0 || frame_area.width < 5 || composer_area.y <= frame_area.y {
        return None;
    }
    let (chrome_rows, minimum) = match menus.frame {
        MenuFrame::Framed => (2, 3),
        MenuFrame::Plain => (0, 1),
    };
    let max_rows = menus.rows.map_or(POPUP_MAX_HEIGHT - 2, u16::from);
    let height = max_rows
        .saturating_add(chrome_rows)
        .min(usize_to_u16_saturating(item_count).saturating_add(chrome_rows))
        .min(
            composer_area
                .y
                .min(frame_area.bottom())
                .saturating_sub(frame_area.y),
        );
    if height < minimum {
        return None;
    }
    let margin = if frame_area.width >= 12 {
        POPUP_SIDE_MARGIN
    } else {
        0
    };
    let width = frame_area
        .width
        .saturating_sub(margin.saturating_mul(2))
        .min(POPUP_MAX_WIDTH);
    let max_x = frame_area.right().saturating_sub(width);
    let x = composer_area
        .x
        .saturating_add(margin)
        .clamp(frame_area.x, max_x);
    let y = composer_area
        .y
        .min(frame_area.bottom())
        .saturating_sub(height);
    Some(Rect::new(x, y, width, height))
}

/// Return the measured list slot for scrolling-aware mouse selection.
pub fn slash_palette_list_area(
    frame_area: Rect,
    composer_content_area: Rect,
    item_count: usize,
    menus: MenuChrome,
) -> Option<Rect> {
    let composer = composer_panel_area(composer_content_area);
    let area = slash_palette_area(frame_area, composer, item_count, menus)?;
    if menus.frame == MenuFrame::Plain {
        return Some(area);
    }
    let component = palette_component(area, PickerFrameStyles::default());
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    list_area(area, &layout).filter(|inner| !inner.is_empty())
}

/// Return the visible row offset, not the source index, for a painted list cell.
pub fn slash_palette_row_from_mouse(
    frame_area: Rect,
    composer_content_area: Rect,
    mouse_x: u16,
    mouse_y: u16,
    item_count: usize,
    menus: MenuChrome,
) -> Option<usize> {
    let inner = slash_palette_list_area(frame_area, composer_content_area, item_count, menus)?;
    if mouse_x < inner.x
        || mouse_x >= inner.right()
        || mouse_y < inner.y
        || mouse_y >= inner.bottom()
    {
        return None;
    }
    Some(usize::from(mouse_y.saturating_sub(inner.y)))
}

/// Recover the composer panel from its content slot.
pub const fn composer_panel_area(content_area: Rect) -> Rect {
    Rect::new(
        content_area.x.saturating_sub(2),
        content_area.y.saturating_sub(1),
        content_area.width.saturating_add(4),
        content_area.height.saturating_add(2),
    )
}

fn slash_item_line(
    item: &SlashItem,
    selected: bool,
    width: u16,
    theme: TuiTheme,
    menus: MenuChrome,
) -> Line {
    let marker = menu_marker(menus);
    let base = theme.raised.patch(if selected {
        theme.selection
    } else {
        theme.text
    });
    let secondary = if selected {
        base.add_modifier(Modifier::DIM)
    } else {
        theme.raised.patch(theme.muted)
    };
    let mut spans = vec![
        Span::styled(
            if selected {
                format!("{marker} ")
            } else {
                " ".repeat(display_width(&marker) + 1)
            },
            if selected {
                bmux_tui::style::Style {
                    bg: base.bg,
                    ..base.patch(theme.focused)
                }
            } else {
                base
            },
        ),
        Span::styled(item.command(), base.add_modifier(Modifier::BOLD)),
    ];
    if menus.tags.is_shown() {
        spans.push(Span::styled(
            if item.is_skill() {
                "  [skill]"
            } else {
                "  [cmd]"
            },
            secondary,
        ));
    }
    spans.push(Span::styled("  ", secondary));
    spans.push(Span::styled(item.description(), secondary));
    Line::from_spans(spans).truncate(usize::from(width))
}

fn usize_to_u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}
