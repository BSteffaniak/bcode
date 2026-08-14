//! Terminal rendering for the streaming presentation configurator.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::{Border, Line, Panel, Span, TextBlock, TextWrap, Widget};
use bmux_tui::style::Modifier;
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing};

use super::render::TuiTheme;
use super::streaming_configurator::{
    StreamingConfiguratorFocus, StreamingConfiguratorState, StreamingPreviewController,
};

const STACK_BREAKPOINT: u16 = 86;
const MIN_USEFUL_WIDTH: u16 = 54;
const MIN_USEFUL_HEIGHT: u16 = 24;

/// Render the opaque streaming configurator surface.
pub fn render_streaming_configurator(
    state: &StreamingConfiguratorState,
    frame: &mut Frame<'_>,
    theme: TuiTheme,
) {
    let modal = configurator_modal(theme);
    modal.render(frame.area(), frame);
    let content = modal.content_area(frame.area());
    if content.width < MIN_USEFUL_WIDTH || content.height < MIN_USEFUL_HEIGHT {
        TextBlock::new(
            "The terminal is too small for the streaming comparison. Resize to at least 54 × 24.",
        )
        .style(theme.selection)
        .wrap(TextWrap::Word)
        .render(content, frame);
        return;
    }

    let controls_height = 9;
    let preview_height = content.height.saturating_sub(controls_height);
    let preview_area = Rect::new(content.x, content.y, content.width, preview_height);
    let controls_area = Rect::new(
        content.x,
        content.y.saturating_add(preview_height),
        content.width,
        controls_height,
    );
    let (raw_area, smoothed_area) = preview_areas(preview_area);
    render_preview(
        "Raw provider chunks",
        state.controller().raw_text(),
        raw_area,
        frame,
        theme,
    );
    render_preview(
        "Smoothed presentation",
        state.controller().smoothed_text(),
        smoothed_area,
        frame,
        theme,
    );
    render_controls(state, controls_area, frame, theme);
}

fn configurator_modal(theme: TuiTheme) -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(Size::new(54, 24), Size::new(140, 46), Insets::all(1)),
        theme.modal_theme(),
    )
    .title(" Streaming presentation configurator ")
    .padding(Insets::new(1, 1, 1, 1))
    .placement(ModalPlacement::Centered)
}

fn preview_areas(area: Rect) -> (Rect, Rect) {
    if area.width >= STACK_BREAKPOINT {
        let left_width = area.width.saturating_sub(1) / 2;
        (
            Rect::new(area.x, area.y, left_width, area.height),
            Rect::new(
                area.x.saturating_add(left_width).saturating_add(1),
                area.y,
                area.width.saturating_sub(left_width).saturating_sub(1),
                area.height,
            ),
        )
    } else {
        let top_height = area.height.saturating_sub(1) / 2;
        (
            Rect::new(area.x, area.y, area.width, top_height),
            Rect::new(
                area.x,
                area.y.saturating_add(top_height).saturating_add(1),
                area.width,
                area.height.saturating_sub(top_height).saturating_sub(1),
            ),
        )
    }
}

fn render_preview(title: &str, text: &str, area: Rect, frame: &mut Frame<'_>, theme: TuiTheme) {
    let panel = Panel::new()
        .border(Border::rounded())
        .title(title)
        .padding(Insets::new(0, 1, 0, 1))
        .background(theme.overlay)
        .title_style(theme.focused)
        .content_style(theme.text);
    let inner = panel.inner_area(area);
    panel.render(area, frame);
    TextBlock::new(text.to_owned())
        .style(theme.text)
        .wrap(TextWrap::Word)
        .render(inner, frame);
}

fn render_controls(
    state: &StreamingConfiguratorState,
    area: Rect,
    frame: &mut Frame<'_>,
    theme: TuiTheme,
) {
    let policy = state.selected_policy();
    let curve = match policy.curve {
        bcode_session_view_models::StreamingInterpolationCurve::Linear => "linear",
        bcode_session_view_models::StreamingInterpolationCurve::EaseIn => "ease in",
        bcode_session_view_models::StreamingInterpolationCurve::EaseOut => "ease out",
        bcode_session_view_models::StreamingInterpolationCurve::EaseInOut => "ease in/out",
    };
    let rate = if policy.graphemes_per_second == 0 {
        "immediate".to_owned()
    } else {
        format!("{} graphemes/s", policy.graphemes_per_second)
    };
    let playback = if state.controller().is_paused() {
        "paused"
    } else if state.controller().is_completed() {
        "completed"
    } else {
        "running"
    };
    let rows = [
        setting_line(
            state.focus() == StreamingConfiguratorFocus::Enabled,
            "Enabled",
            if policy.enabled { "yes" } else { "no" },
            theme,
        ),
        setting_line(
            state.focus() == StreamingConfiguratorFocus::Curve,
            "Curve",
            curve,
            theme,
        ),
        setting_line(
            state.focus() == StreamingConfiguratorFocus::GraphemesPerSecond,
            "Rate",
            &rate,
            theme,
        ),
        setting_line(
            state.focus() == StreamingConfiguratorFocus::MaxLag,
            "Maximum backlog age",
            &format!("{} ms", policy.max_lag_ms),
            theme,
        ),
        setting_line(
            state.focus() == StreamingConfiguratorFocus::Reset,
            "Reset override",
            if state.reset_pending() {
                "pending"
            } else {
                "no"
            },
            theme,
        ),
        Line::from_spans(vec![
            Span::styled("Source: bursty provider · ", theme.muted),
            Span::styled(
                format!(
                    "chunk {}/{} · {playback}",
                    state.controller().delivered_chunks(),
                    StreamingPreviewController::total_chunks()
                ),
                theme.text,
            ),
        ]),
        Line::from_spans(vec![Span::styled(
            "↑↓ select  ←→ adjust  shift+←→ coarse  space toggle  r restart  p pause  enter apply  esc cancel",
            theme.muted,
        )]),
    ];
    for (offset, row) in rows.into_iter().enumerate() {
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        if offset >= area.height {
            break;
        }
        frame.write_line(Rect::new(area.x, area.y + offset, area.width, 1), &row);
    }
}

fn setting_line(focused: bool, label: &str, value: &str, theme: TuiTheme) -> Line {
    let marker = if focused { "›" } else { " " };
    let marker_style = if focused {
        theme.focused.add_modifier(Modifier::BOLD)
    } else {
        theme.muted
    };
    Line::from_spans(vec![
        Span::styled(marker, marker_style),
        Span::styled(format!(" {label}: "), theme.muted),
        Span::styled(value.to_owned(), theme.focused),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_layout_switches_from_columns_to_stacks() {
        let wide = Rect::new(0, 0, 100, 20);
        let (raw, smooth) = preview_areas(wide);
        assert_eq!(raw.y, smooth.y);
        assert!(raw.x < smooth.x);
        let narrow = Rect::new(0, 0, 70, 20);
        let (raw, smooth) = preview_areas(narrow);
        assert_eq!(raw.x, smooth.x);
        assert!(raw.y < smooth.y);
        assert_eq!(raw.width, smooth.width);
    }
}
