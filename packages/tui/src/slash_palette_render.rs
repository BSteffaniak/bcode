//! TUI slash completion rendering.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::prelude::{Line, Span, Style, Widget};
use bmux_tui::style::{Color, Modifier};

use super::render::TuiTheme;
use super::slash_palette::{SlashItem, SlashPalette};

const POPUP_MAX_HEIGHT: u16 = 8;
const POPUP_MAX_WIDTH: u16 = 88;
const POPUP_SIDE_MARGIN: u16 = 2;

/// Render slash completions above the composer.
pub fn render_palette(
    palette: &SlashPalette,
    composer_content_area: Rect,
    frame: &mut Frame<'_>,
    theme: TuiTheme,
) {
    let frame_area = frame.area();
    let composer = composer_panel_area(composer_content_area);
    let Some(area) = slash_palette_area(frame_area, composer, palette.item_count()) else {
        return;
    };

    frame.fill(area, " ", Style::new());
    let panel = Panel::new()
        .border(Border::single().style(Style::new().fg(theme.accent)))
        .title(" Slash Commands  tab/enter accept · ↑/↓ select · esc hide ")
        .padding(Insets::new(0, 1, 0, 1));
    panel.render(area, frame);

    let inner = panel.inner_area(area);
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
            Rect::new(inner.x, y, inner.width, 1),
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
        Style::new()
            .fg(Color::White)
            .bg(Color::Rgb(38, 52, 64))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let badge_style = Style::new()
        .fg(Color::Black)
        .bg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let available = usize::from(width.saturating_sub(15));
    Line::from_spans(vec![
        Span::styled(if selected { "› " } else { "  " }, base),
        Span::styled(" cmd ", badge_style),
        Span::styled("  ", base),
        Span::styled(
            truncate_end(item.command(), available.min(30)),
            base.add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", base),
        Span::styled(
            truncate_end(item.description(), available.saturating_sub(30)),
            base.fg(Color::BrightBlack),
        ),
    ])
}

fn truncate_end(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut output = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

fn usize_to_u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}
