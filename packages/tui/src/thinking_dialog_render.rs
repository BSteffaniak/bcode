//! TUI thinking settings dialog rendering.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::prelude::{Line, Span, Style, Widget};
use bmux_tui::style::{Color, Modifier};

use super::thinking_dialog::ThinkingDialogState;

const MIN_DIALOG_WIDTH: u16 = 56;
const MAX_DIALOG_WIDTH: u16 = 96;
const MIN_DIALOG_HEIGHT: u16 = 15;
const MAX_DIALOG_HEIGHT: u16 = 22;

/// Render a thinking settings dialog.
pub fn render_thinking_dialog(state: &ThinkingDialogState, frame: &mut Frame<'_>) {
    let area = dialog_area(frame.area());
    Panel::new()
        .border(Border::rounded().style(Style::new().fg(Color::Cyan)))
        .title(" Thinking settings ")
        .padding(Insets::new(1, 2, 1, 2))
        .render(area, frame);

    let content = area.inset(Insets::new(2, 3, 2, 3));
    let rows = rows(state);
    for (row_index, line) in rows.iter().take(usize::from(content.height)).enumerate() {
        let Ok(row_offset) = u16::try_from(row_index) else {
            return;
        };
        frame.write_line(
            Rect::new(
                content.x,
                content.y.saturating_add(row_offset),
                content.width,
                1,
            ),
            line,
        );
    }
}

/// Return the thinking dialog panel area for a terminal area.
#[must_use]
pub fn dialog_area(area: Rect) -> Rect {
    let available_width = area.width.saturating_sub(4);
    let available_height = area.height.saturating_sub(4);
    let width = available_width.clamp(MIN_DIALOG_WIDTH.min(available_width), MAX_DIALOG_WIDTH);
    let height = available_height.clamp(MIN_DIALOG_HEIGHT.min(available_height), MAX_DIALOG_HEIGHT);
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height) / 3);
    Rect::new(x, y, width, height)
}

fn rows(state: &ThinkingDialogState) -> Vec<Line> {
    let mut rows = Vec::new();
    rows.push(Line::from_spans(vec![Span::styled(
        "Control what reasoning is requested and whether provider-visible reasoning is shown.",
        Style::new().fg(Color::BrightWhite),
    )]));
    rows.push(Line::default());
    rows.push(setting_row(
        state.focused_row() == 0,
        "Display reasoning",
        if state.visible() { "shown" } else { "hidden" },
        Some("local TUI display only"),
    ));
    rows.push(setting_row(
        state.focused_row() == 1,
        "Reasoning effort",
        state.effective_effort_label(),
        Some(&values_help(state.effort_values())),
    ));
    rows.push(setting_row(
        state.focused_row() == 2,
        "Reasoning summary",
        state.effective_summary_label(),
        Some(&values_help(state.summary_values())),
    ));
    rows.push(Line::default());
    rows.push(Line::from_spans(vec![
        Span::styled(
            "Enter",
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" apply   "),
        Span::styled(
            "Esc",
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" cancel   "),
        Span::styled(
            "↑/↓",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" move   "),
        Span::styled(
            "Space",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" change"),
    ]));
    rows
}

fn setting_row(focused: bool, label: &str, value: &str, help: Option<&str>) -> Line {
    let marker = if focused { "›" } else { " " };
    let marker_style = if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::BrightBlack)
    };
    let mut spans = vec![
        Span::styled(marker, marker_style),
        Span::raw(" "),
        Span::styled(format!("{label}: "), Style::new().fg(Color::BrightBlack)),
        Span::styled(value.to_owned(), Style::new().fg(Color::Cyan)),
    ];
    if let Some(help) = help {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            help.to_owned(),
            Style::new().fg(Color::BrightBlack),
        ));
    }
    Line::from_spans(spans)
}

fn values_help(values: &[String]) -> String {
    if values.is_empty() {
        "provider values unknown".to_owned()
    } else {
        format!("available: {}", values.join(", "))
    }
}
