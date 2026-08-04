//! TUI reasoning output settings dialog rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing};

use super::render::TuiTheme;
use super::thinking_dialog::ThinkingDialogState;

const MIN_DIALOG_WIDTH: u16 = 56;
const MAX_DIALOG_WIDTH: u16 = 96;
const MIN_DIALOG_HEIGHT: u16 = 15;
const MAX_DIALOG_HEIGHT: u16 = 22;

/// Render a reasoning output settings dialog.
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
        theme.modal_theme(),
    )
    .title(" Reasoning output settings ")
    .padding(Insets::new(1, 2, 1, 2))
    .placement(ModalPlacement::UpperThird)
}

fn rows(state: &ThinkingDialogState, theme: TuiTheme) -> Vec<Line> {
    let mut rows = Vec::new();
    rows.push(Line::from_spans(vec![Span::styled(
        "Control requested reasoning effort and provider-visible reasoning summaries.",
        theme.text,
    )]));
    if !state.supported() {
        rows.push(Line::from_spans(vec![Span::styled(
            "This model does not advertise reasoning support. Add a model metadata override to enable it.",
            theme.selection,
        )]));
    }
    rows.push(modal_blank_line(theme));
    rows.push(setting_row(
        state.focused_row() == 0,
        "Show reasoning output",
        if state.visible() { "shown" } else { "hidden" },
        Some("local display filter; provider must emit reasoning events"),
        theme,
    ));
    rows.push(setting_row(
        state.focused_row() == 1,
        "Displayed reasoning",
        state.mode_label(),
        Some("local display filter"),
        theme,
    ));
    rows.push(setting_row(
        state.focused_row() == 2,
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
        state.focused_row() == 3,
        "Visible reasoning summary",
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
    rows.push(modal_blank_line(theme));
    rows.push(Line::from_spans(vec![
        Span::styled("Enter", theme.selection.add_modifier(Modifier::BOLD)),
        Span::styled(" apply   ", theme.text),
        Span::styled("Esc", theme.selection.add_modifier(Modifier::BOLD)),
        Span::styled(" cancel   ", theme.text),
        Span::styled(
            "↑/↓",
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" move   ", theme.text),
        Span::styled(
            "Space",
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" change", theme.text),
    ]));
    rows
}

fn modal_blank_line(theme: TuiTheme) -> Line {
    Line::from_spans(vec![Span::styled("", theme.text)])
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
        Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
    } else {
        theme.muted
    };
    let mut spans = vec![
        Span::styled(marker, marker_style),
        Span::styled(" ", theme.text),
        Span::styled(format!("{label}: "), theme.muted),
        Span::styled(value.to_owned(), Style::new().fg(theme.accent)),
    ];
    if let Some(help) = help {
        spans.push(Span::styled("  ", theme.text));
        spans.push(Span::styled(help.to_owned(), theme.muted));
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
