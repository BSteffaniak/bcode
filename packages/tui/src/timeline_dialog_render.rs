//! TUI timeline dialog rendering.

use bcode_markdown_render::markdown_to_plain_text;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Size};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::Modifier;
use bmux_tui::text_width::display_width;
use bmux_tui_components::dialog::{Dialog, DialogState};
use bmux_tui_components::modal_frame::{ModalPlacement, ModalSizing};
use unicode_segmentation::UnicodeSegmentation;

use super::render::TuiTheme;
use super::timeline_dialog::{TimelineDialogState, TimelineEntry};

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
    let sizing = dialog_sizing();
    let layout = Dialog::new(&[], &[], theme.modal_theme())
        .title(" Timeline ")
        .sizing(sizing)
        .placement(ModalPlacement::Centered)
        .layout(frame.area());
    if layout.body.is_empty() {
        return;
    }
    let visible_entries = usize::from(layout.body.height.saturating_sub(3));
    state.sync_scroll(visible_entries);
    let body = rows(state, layout.body.width, visible_entries, theme);
    Dialog::new(&body, &[], theme.modal_theme())
        .title(" Timeline ")
        .sizing(sizing)
        .placement(ModalPlacement::Centered)
        .render(frame.area(), &DialogState::new(), frame);
}

const fn dialog_sizing() -> ModalSizing {
    ModalSizing::new(
        Size::new(MIN_DIALOG_WIDTH, MIN_DIALOG_HEIGHT),
        Size::new(MAX_DIALOG_WIDTH, MAX_DIALOG_HEIGHT),
        Insets::all(4),
    )
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
        theme.border.add_modifier(Modifier::BOLD),
    )]));
    if state.entries().is_empty() {
        rows.push(Line::from_spans(vec![Span::styled(
            "No user messages in this session.",
            theme.muted,
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
        theme.muted,
    )]));
    rows
}

fn entry_line(entry: &TimelineEntry, selected: bool, width: u16, theme: TuiTheme) -> Line {
    let marker = if selected { "›" } else { " " };
    let base = if selected {
        theme.selection.add_modifier(Modifier::BOLD)
    } else {
        theme.text
    };
    let accent = if selected {
        theme.selection.add_modifier(Modifier::BOLD)
    } else {
        theme.border
    };
    let dim = if selected {
        theme.selection
    } else {
        theme.muted
    };
    let reserved = TIMESTAMP_WIDTH.saturating_add(4);
    let preview_width = usize::from(width).saturating_sub(reserved).max(8);
    Line::from_spans(vec![
        Span::styled(marker, accent),
        Span::styled(" ", base),
        Span::styled(format_timestamp(entry.timestamp_ms()), dim),
        Span::styled("  ", base),
        Span::styled(markdown_preview(entry.text(), preview_width), base),
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

fn markdown_preview(value: &str, width: usize) -> String {
    truncate_display_width(&markdown_to_plain_text(value), width)
}

fn truncate_display_width(value: &str, width: usize) -> String {
    if display_width(value) <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let content_width = width.saturating_sub(1);
    let mut result = String::new();
    let mut used = 0_usize;
    for grapheme in value.graphemes(true) {
        let grapheme_width = display_width(grapheme);
        if used.saturating_add(grapheme_width) > content_width {
            break;
        }
        result.push_str(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::{markdown_preview, truncate_display_width};
    use bmux_tui::text_width::display_width;

    #[test]
    fn timeline_preview_flattens_markdown_semantically() {
        assert_eq!(
            markdown_preview("# Request\n\n- Use **care**\n- Run `cargo test`", 80),
            "Request Use care Run cargo test"
        );
    }

    #[test]
    fn timeline_preview_truncates_by_terminal_display_width() {
        let preview = truncate_display_width("こんにちは world", 8);
        assert!(display_width(&preview) <= 8);
        assert!(preview.ends_with('…'));
    }
}
