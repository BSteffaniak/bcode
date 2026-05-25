//! TUI thinking settings dialog rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};

use super::thinking_dialog::ThinkingDialogState;

const MODAL_BG: Color = Color::Black;

const MIN_DIALOG_WIDTH: u16 = 56;
const MAX_DIALOG_WIDTH: u16 = 96;
const MIN_DIALOG_HEIGHT: u16 = 15;
const MAX_DIALOG_HEIGHT: u16 = 22;

/// Render a thinking settings dialog.
pub fn render_thinking_dialog(state: &ThinkingDialogState, frame: &mut Frame<'_>) {
    let modal = modal_frame();
    modal.render(frame.area(), frame);

    let content = modal.content_area(frame.area());
    let rows = rows(state);
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

fn modal_frame() -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(
            Size::new(MIN_DIALOG_WIDTH, MIN_DIALOG_HEIGHT),
            Size::new(MAX_DIALOG_WIDTH, MAX_DIALOG_HEIGHT),
            Insets::all(4),
        ),
        ModalTheme::dark(Color::Cyan),
    )
    .title(" Thinking settings ")
    .padding(Insets::new(1, 2, 1, 2))
    .placement(ModalPlacement::UpperThird)
}

fn rows(state: &ThinkingDialogState) -> Vec<Line> {
    let mut rows = Vec::new();
    rows.push(Line::from_spans(vec![Span::styled(
        "Control what reasoning is requested and whether provider-visible reasoning is shown.",
        Style::new().fg(Color::BrightWhite).bg(MODAL_BG),
    )]));
    rows.push(modal_blank_line());
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
        Some(&values_help(
            state.effort_values(),
            state.effort_values_are_provider_declared(),
        )),
    ));
    rows.push(setting_row(
        state.focused_row() == 2,
        "Reasoning summary",
        state.effective_summary_label(),
        Some(&values_help(
            state.summary_values(),
            state.summary_values_are_provider_declared(),
        )),
    ));
    rows.push(modal_blank_line());
    rows.push(Line::from_spans(vec![
        Span::styled(
            "Enter",
            Style::new()
                .fg(Color::Green)
                .bg(MODAL_BG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" apply   ", Style::new().bg(MODAL_BG)),
        Span::styled(
            "Esc",
            Style::new()
                .fg(Color::Yellow)
                .bg(MODAL_BG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" cancel   ", Style::new().bg(MODAL_BG)),
        Span::styled(
            "↑/↓",
            Style::new()
                .fg(Color::Cyan)
                .bg(MODAL_BG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" move   ", Style::new().bg(MODAL_BG)),
        Span::styled(
            "Space",
            Style::new()
                .fg(Color::Cyan)
                .bg(MODAL_BG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" change", Style::new().bg(MODAL_BG)),
    ]));
    rows
}

fn modal_blank_line() -> Line {
    Line::from_spans(vec![Span::styled("", Style::new().bg(MODAL_BG))])
}

fn setting_row(focused: bool, label: &str, value: &str, help: Option<&str>) -> Line {
    let marker = if focused { "›" } else { " " };
    let marker_style = if focused {
        Style::new()
            .fg(Color::Cyan)
            .bg(MODAL_BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::BrightBlack).bg(MODAL_BG)
    };
    let mut spans = vec![
        Span::styled(marker, marker_style),
        Span::styled(" ", Style::new().bg(MODAL_BG)),
        Span::styled(
            format!("{label}: "),
            Style::new().fg(Color::BrightBlack).bg(MODAL_BG),
        ),
        Span::styled(value.to_owned(), Style::new().fg(Color::Cyan).bg(MODAL_BG)),
    ];
    if let Some(help) = help {
        spans.push(Span::styled("  ", Style::new().bg(MODAL_BG)));
        spans.push(Span::styled(
            help.to_owned(),
            Style::new().fg(Color::BrightBlack).bg(MODAL_BG),
        ));
    }
    Line::from_spans(spans)
}

fn values_help(values: &[String], provider_declared: bool) -> String {
    if values.is_empty() {
        "provider values unknown".to_owned()
    } else if provider_declared {
        format!("available: {}", values.join(", "))
    } else {
        format!("fallback: {}", values.join(", "))
    }
}
