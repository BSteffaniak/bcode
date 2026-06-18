//! TUI thinking settings dialog rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};

use super::render::TuiTheme;
use super::thinking_dialog::ThinkingDialogState;

const MODAL_BG: Color = Color::Black;

const MIN_DIALOG_WIDTH: u16 = 56;
const MAX_DIALOG_WIDTH: u16 = 96;
const MIN_DIALOG_HEIGHT: u16 = 15;
const MAX_DIALOG_HEIGHT: u16 = 22;

/// Render a thinking settings dialog.
pub fn render_thinking_dialog(state: &ThinkingDialogState, frame: &mut Frame<'_>, theme: TuiTheme) {
    let modal = modal_frame(theme);
    modal.render(frame.area(), frame);

    let content = modal.content_area(frame.area());
    let rows = rows(state, theme);
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
    .title(" Thinking settings ")
    .padding(Insets::new(1, 2, 1, 2))
    .placement(ModalPlacement::UpperThird)
}

fn rows(state: &ThinkingDialogState, theme: TuiTheme) -> Vec<Line> {
    let mut rows = Vec::new();
    rows.push(Line::from_spans(vec![Span::styled(
        "Control what reasoning is requested and whether provider-visible reasoning is shown.",
        Style::new().fg(Color::BrightWhite).bg(MODAL_BG),
    )]));
    if !state.supported() {
        rows.push(Line::from_spans(vec![Span::styled(
            "This model does not advertise reasoning support. Add a model metadata override to enable it.",
            Style::new().fg(Color::Yellow).bg(MODAL_BG),
        )]));
    }
    rows.push(modal_blank_line());
    rows.push(setting_row(
        state.focused_row() == 0,
        "Display reasoning",
        if state.visible() { "shown" } else { "hidden" },
        Some("local TUI display only"),
        theme,
    ));
    rows.push(setting_row(
        state.focused_row() == 1,
        "Reasoning effort",
        if state.supported() {
            state.effective_effort_label()
        } else {
            "unsupported"
        },
        Some(&values_help(
            state.effort_values(),
            state.effort_values_source(),
        )),
        theme,
    ));
    rows.push(setting_row(
        state.focused_row() == 2,
        "Reasoning summary",
        if state.supported() {
            state.effective_summary_label()
        } else {
            "unsupported"
        },
        Some(&values_help(
            state.summary_values(),
            state.summary_values_source(),
        )),
        theme,
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
                .fg(theme.accent)
                .bg(MODAL_BG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" move   ", Style::new().bg(MODAL_BG)),
        Span::styled(
            "Space",
            Style::new()
                .fg(theme.accent)
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

fn setting_row(
    focused: bool,
    label: &str,
    value: &str,
    help: Option<&str>,
    theme: TuiTheme,
) -> Line {
    let marker = if focused { "›" } else { " " };
    let marker_style = if focused {
        Style::new()
            .fg(theme.accent)
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
        Span::styled(value.to_owned(), Style::new().fg(theme.accent).bg(MODAL_BG)),
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

fn values_help(values: &[String], source: bcode_model::ModelReasoningCapabilitySource) -> String {
    if values.is_empty() {
        return "not supported or unknown".to_owned();
    }
    let values = values.join(", ");
    match source {
        bcode_model::ModelReasoningCapabilitySource::ConfigOverride => {
            format!("config: {values}")
        }
        bcode_model::ModelReasoningCapabilitySource::ProviderMetadata => {
            format!("provider: {values}")
        }
        bcode_model::ModelReasoningCapabilitySource::KnownModelTable => {
            format!("known model: {values}")
        }
        bcode_model::ModelReasoningCapabilitySource::GenericFallback
        | bcode_model::ModelReasoningCapabilitySource::Unknown => {
            format!("common values; provider may reject: {values}")
        }
    }
}
