//! Terminal rendering for the streaming presentation configurator.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::{Border, Line, Panel, Span, TextBlock, TextWrap, Widget};
use bmux_tui::style::Modifier;
use bmux_tui_components::action_row::ActionRow;
use bmux_tui_components::button::ButtonStyles;
use bmux_tui_components::checkbox::{Checkbox, CheckboxStyles};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing};

use super::render::TuiTheme;
use super::streaming_configurator::{
    StreamingConfiguratorFocus, StreamingConfiguratorGeometry, StreamingConfiguratorState,
    StreamingPreviewController, curve_action_buttons, numeric_action_buttons,
    outcome_action_buttons,
};

const STACK_BREAKPOINT: u16 = 86;
const MIN_USEFUL_WIDTH: u16 = 54;
const MIN_USEFUL_HEIGHT: u16 = 24;

pub fn streaming_configurator_geometry(
    _state: &StreamingConfiguratorState,
    frame_area: Rect,
    theme: TuiTheme,
) -> Option<StreamingConfiguratorGeometry> {
    let modal = configurator_modal(theme);
    let content = modal.content_area(frame_area);
    if content.width < MIN_USEFUL_WIDTH || content.height < MIN_USEFUL_HEIGHT {
        return None;
    }
    let controls_height = 9;
    let preview_height = content.height.saturating_sub(controls_height);
    let area = Rect::new(
        content.x,
        content.y.saturating_add(preview_height),
        content.width,
        controls_height,
    );
    let geometry = StreamingConfiguratorGeometry {
        enabled: Rect::new(area.x, area.y, 18.min(area.width), 1),
        curve: Rect::new(
            area.x.saturating_add(10),
            area.y + 1,
            area.width.saturating_sub(10),
            1,
        ),
        rate: Rect::new(
            area.x.saturating_add(28),
            area.y + 2,
            area.width.saturating_sub(28).min(10),
            1,
        ),
        lag: Rect::new(
            area.x.saturating_add(28),
            area.y + 3,
            area.width.saturating_sub(28).min(10),
            1,
        ),
        outcomes: Rect::new(area.x, area.y + 4, area.width.min(30), 1),
        surface: frame_area,
    };
    Some(geometry)
}

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

const fn preview_areas(area: Rect) -> (Rect, Rect) {
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

#[allow(clippy::too_many_lines)]
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
    let Some(geometry) = streaming_configurator_geometry(state, frame.area(), theme) else {
        return;
    };
    let checkbox = Checkbox::new("Enabled").styles(CheckboxStyles {
        normal: theme.text,
        focused: theme.focused,
        hovered: theme.selection,
        pressed: theme.focused.add_modifier(Modifier::BOLD),
        disabled: theme.muted,
    });
    checkbox.render_with_id(
        "streaming.enabled",
        geometry.enabled,
        state.enabled_checkbox(),
        frame,
    );
    let button_styles = ButtonStyles {
        normal: theme.text,
        focused: theme.focused,
        hovered: theme.selection,
        pressed: theme.focused.add_modifier(Modifier::BOLD),
        disabled: theme.muted,
    };
    let curve_actions = curve_action_buttons();
    ActionRow::new(&curve_actions)
        .styles(button_styles)
        .render_state_with_id_prefix(
            geometry.curve,
            state.curve_actions(),
            frame,
            "streaming.curve",
        );
    let numeric_actions = numeric_action_buttons();
    ActionRow::new(&numeric_actions)
        .styles(button_styles)
        .render_state_with_id_prefix(geometry.rate, state.rate_actions(), frame, "streaming.rate");
    ActionRow::new(&numeric_actions)
        .styles(button_styles)
        .render_state_with_id_prefix(geometry.lag, state.lag_actions(), frame, "streaming.lag");
    let outcome_actions = outcome_action_buttons();
    ActionRow::new(&outcome_actions)
        .styles(button_styles)
        .render_state_with_id_prefix(
            geometry.outcomes,
            state.outcome_actions(),
            frame,
            "streaming.outcomes",
        );
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
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;

    use super::*;

    fn render_text(area: Rect, state: &StreamingConfiguratorState) -> String {
        let theme = TuiTheme::for_theme_id("bcode-dark");
        let mut buffer = Buffer::empty(area);
        render_streaming_configurator(state, &mut Frame::new(&mut buffer), theme);
        (0..area.height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<String>()
    }

    #[test]
    fn renderer_registers_stable_bmux_component_mouse_hits() {
        let now = std::time::Instant::now();
        let state = StreamingConfiguratorState::new(
            now,
            bcode_session_view_models::StreamingPresentationPolicy::default(),
            bcode_session_view_models::StreamingPresentationPolicy::default(),
        );
        let theme = TuiTheme::for_theme_id("bcode-dark");
        let area = Rect::new(0, 0, 120, 40);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        render_streaming_configurator(&state, &mut frame, theme);
        let ids = frame
            .hits()
            .regions()
            .iter()
            .map(|hit| hit.id.as_str())
            .collect::<Vec<_>>();
        assert!(ids.contains(&"streaming.enabled"), "{ids:?}");
        assert!(
            ids.iter().any(|id| id.starts_with("streaming.curve")),
            "{ids:?}"
        );
        assert!(
            ids.iter().any(|id| id.starts_with("streaming.rate")),
            "{ids:?}"
        );
        assert!(
            ids.iter().any(|id| id.starts_with("streaming.outcomes")),
            "{ids:?}"
        );
    }

    #[test]
    fn renders_controls_playback_and_responsive_preview_labels() {
        let now = std::time::Instant::now();
        let state = StreamingConfiguratorState::new(
            now,
            bcode_session_view_models::StreamingPresentationPolicy::default(),
            bcode_session_view_models::StreamingPresentationPolicy::default(),
        );
        let wide = render_text(Rect::new(0, 0, 120, 36), &state);
        for expected in [
            "Streaming presentation configurator",
            "Raw provider chunks",
            "Smoothed presentation",
            "[x] Enabled",
            "300 graphemes/s",
            "chunk 0/13",
            "enter apply",
        ] {
            assert!(wide.contains(expected), "missing {expected}: {wide}");
        }
        let narrow = render_text(Rect::new(0, 0, 70, 36), &state);
        assert!(narrow.contains("Raw provider chunks"));
        assert!(narrow.contains("Smoothed presentation"));
    }

    #[test]
    fn renders_minimum_size_fallback_and_reset_state() {
        let now = std::time::Instant::now();
        let mut state = StreamingConfiguratorState::new(
            now,
            bcode_session_view_models::StreamingPresentationPolicy::default(),
            bcode_session_view_models::StreamingPresentationPolicy::default(),
        );
        let small = render_text(Rect::new(0, 0, 40, 16), &state);
        assert!(small.contains("terminal is too small"), "{small}");
        for _ in 0..4 {
            let _ = state.handle_key(
                bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Down),
                now,
            );
        }
        let _ = state.handle_key(
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Space),
            now,
        );
        let reset = render_text(Rect::new(0, 0, 120, 36), &state);
        assert!(reset.contains("[ Reset ]"), "{reset}");
    }

    #[test]
    fn renders_focus_pause_completion_reset_and_identical_unicode_markdown() {
        let now = std::time::Instant::now();
        let immediate = bcode_session_view_models::StreamingPresentationPolicy::immediate();
        let mut state = StreamingConfiguratorState::new(now, immediate, immediate);
        let focused = render_text(Rect::new(0, 0, 120, 40), &state);
        assert!(focused.contains("[ ] Enabled"), "{focused}");

        let _ = state.handle_key(
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Char('p')),
            now,
        );
        let paused = render_text(Rect::new(0, 0, 120, 40), &state);
        assert!(paused.contains("paused"), "{paused}");
        let _ = state.handle_key(
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Char('p')),
            now,
        );
        assert!(state.advance(now + std::time::Duration::from_millis(1_800)));
        let completed = render_text(Rect::new(0, 0, 120, 44), &state);
        assert!(completed.contains("completed"), "{completed}");
        assert_eq!(completed.matches("# Bursty streaming").count(), 2);
        assert_eq!(completed.matches("cafe\u{301}").count(), 2);
        assert_eq!(completed.matches("👩🏽‍💻").count(), 2);
        assert_eq!(completed.matches("東京").count(), 2);

        for _ in 0..4 {
            let _ = state.handle_key(
                bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Down),
                now,
            );
        }
        let _ = state.handle_key(
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Space),
            now,
        );
        let reset = render_text(Rect::new(0, 0, 120, 40), &state);
        assert!(reset.contains("[ Reset ]"), "{reset}");
    }

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
