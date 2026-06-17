//! TUI timeline dialog rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};

use super::render::TuiTheme;
use super::timeline_dialog::{TimelineDialogState, TimelineEntry};

const MODAL_BG: Color = Color::Black;
const MIN_DIALOG_WIDTH: u16 = 60;
const MAX_DIALOG_WIDTH: u16 = 110;
const MIN_DIALOG_HEIGHT: u16 = 12;
const MAX_DIALOG_HEIGHT: u16 = 28;
const TIMESTAMP_WIDTH: usize = 19;

/// Render the timeline dialog.
pub fn render_timeline_dialog(
    state: &mut TimelineDialogState,
    frame: &mut Frame<'_>,
    theme: TuiTheme,
) {
    let modal = modal_frame(theme);
    modal.render(frame.area(), frame);

    let content = modal.content_area(frame.area());
    if content.is_empty() {
        return;
    }
    let visible_entries = usize::from(content.height.saturating_sub(3));
    state.sync_scroll(visible_entries);
    let rows = rows(state, content.width, visible_entries, theme);
    for (row_index, line) in rows.iter().take(usize::from(content.height)).enumerate() {
        let Ok(row_offset) = u16::try_from(row_index) else {
            return;
        };
        modal.render_line(
            Rect::new(
                content.x,
                content.y.saturating_add(row_offset),
                content.width,
                1,
            ),
            line,
            frame,
        );
    }
}

fn modal_frame(theme: TuiTheme) -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(
            Size::new(MIN_DIALOG_WIDTH, MIN_DIALOG_HEIGHT),
            Size::new(MAX_DIALOG_WIDTH, MAX_DIALOG_HEIGHT),
            Insets::all(4),
        ),
        ModalTheme::dark(theme.accent),
    )
    .title(" Timeline ")
    .placement(ModalPlacement::Centered)
}

fn rows(
    state: &TimelineDialogState,
    width: u16,
    visible_entries: usize,
    theme: TuiTheme,
) -> Vec<Line> {
    let mut rows = Vec::new();
    rows.push(Line::from_spans(vec![Span::styled(
        "User messages",
        Style::new()
            .fg(theme.accent)
            .bg(MODAL_BG)
            .add_modifier(Modifier::BOLD),
    )]));
    if state.entries().is_empty() {
        rows.push(Line::from_spans(vec![Span::styled(
            "No user messages in this session.",
            Style::new().fg(Color::BrightBlack).bg(MODAL_BG),
        )]));
    } else {
        rows.extend(
            state
                .entries()
                .iter()
                .enumerate()
                .skip(state.scroll())
                .take(visible_entries)
                .map(|(index, entry)| entry_line(entry, index == state.selected(), width, theme)),
        );
    }
    rows.push(Line::from_spans(vec![Span::styled(
        "↑/↓ select · PgUp/PgDn jump · Enter go · Esc close",
        Style::new().fg(Color::BrightBlack).bg(MODAL_BG),
    )]));
    rows
}

fn entry_line(entry: &TimelineEntry, selected: bool, width: u16, theme: TuiTheme) -> Line {
    let marker = if selected { "›" } else { " " };
    let base = if selected {
        Style::new()
            .fg(Color::White)
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::White).bg(MODAL_BG)
    };
    let accent = if selected {
        Style::new()
            .fg(theme.accent)
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(theme.accent).bg(MODAL_BG)
    };
    let dim = if selected {
        Style::new().fg(Color::BrightBlack).bg(Color::Blue)
    } else {
        Style::new().fg(Color::BrightBlack).bg(MODAL_BG)
    };
    let reserved = TIMESTAMP_WIDTH.saturating_add(4);
    let preview_width = usize::from(width).saturating_sub(reserved).max(8);
    Line::from_spans(vec![
        Span::styled(marker, accent),
        Span::styled(" ", base),
        Span::styled(format_timestamp(entry.timestamp_ms()), dim),
        Span::styled("  ", base),
        Span::styled(truncate(entry.text(), preview_width), base),
    ])
}

fn format_timestamp(timestamp_ms: u64) -> String {
    let seconds = timestamp_ms / 1_000;
    let (year, month, day, hour, minute, second) = utc_components(seconds);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
}

fn utc_components(seconds: u64) -> (i32, u32, u32, u64, u64, u64) {
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let seconds_in_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        seconds_in_day / 3_600,
        (seconds_in_day % 3_600) / 60,
        seconds_in_day % 60,
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let days = days_since_epoch.saturating_add(719_468);
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    (
        i32::try_from(year).unwrap_or(i32::MAX),
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

fn truncate(value: &str, width: usize) -> String {
    let mut chars = value.chars();
    let mut result = chars.by_ref().take(width).collect::<String>();
    if chars.next().is_some() && width > 1 {
        result.truncate(result.len().saturating_sub(1));
        result.push('…');
    }
    result
}
