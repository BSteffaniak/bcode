//! Terminal rendering for the streaming presentation configurator.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::{Border, Line, Panel, Span, Text, TextBlock, TextWrap, Widget};
use bmux_tui::style::Modifier;
use bmux_tui::text::wrap_line_word;
use bmux_tui_components::action_row::ActionRow;
use bmux_tui_components::button::ButtonStyles;
use bmux_tui_components::checkbox::{Checkbox, CheckboxStyles};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing};

use super::render::TuiTheme;
use super::streaming_configurator::{
    StreamingConfiguratorGeometry, StreamingConfiguratorState, curve_action_buttons,
    numeric_action_buttons, outcome_action_buttons, source_preset_buttons,
};
use super::streaming_source_scenario::LONG_RESPONSE;

const STACK_BREAKPOINT: u16 = 86;
const MIN_USEFUL_WIDTH: u16 = 54;
const MIN_USEFUL_HEIGHT: u16 = 29;

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
    let controls_height = 14;
    let preview_height = content.height.saturating_sub(controls_height);
    let area = Rect::new(
        content.x,
        content.y.saturating_add(preview_height),
        content.width,
        controls_height,
    );
    let geometry = StreamingConfiguratorGeometry {
        enabled: Rect::new(area.x, area.y, 24.min(area.width), 1),
        curve: Rect::new(
            area.x.saturating_add(22),
            area.y + 1,
            area.width.saturating_sub(22),
            1,
        ),
        rate: Rect::new(
            area.x.saturating_add(42),
            area.y + 2,
            area.width.saturating_sub(42).min(14),
            1,
        ),
        lag: Rect::new(
            area.x.saturating_add(42),
            area.y + 3,
            area.width.saturating_sub(42).min(14),
            1,
        ),
        source_preset: Rect::new(
            area.x.saturating_add(34),
            area.y + 4,
            area.width.saturating_sub(34),
            1,
        ),
        source_chunk_size: Rect::new(
            area.x.saturating_add(42),
            area.y + 5,
            area.width.saturating_sub(42).min(14),
            1,
        ),
        source_size_variation: Rect::new(
            area.x.saturating_add(42),
            area.y + 6,
            area.width.saturating_sub(42).min(14),
            1,
        ),
        source_interval: Rect::new(
            area.x.saturating_add(42),
            area.y + 7,
            area.width.saturating_sub(42).min(14),
            1,
        ),
        source_interval_variation: Rect::new(
            area.x.saturating_add(42),
            area.y + 8,
            area.width.saturating_sub(42).min(14),
            1,
        ),
        outcomes: Rect::new(area.x, area.y + 9, area.width.min(48), 1),
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
            "The terminal is too small for the streaming comparison. Resize to at least 54 × 29.",
        )
        .style(theme.selection)
        .wrap(TextWrap::Word)
        .render(content, frame);
        return;
    }

    let controls_height = 14;
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
        state.preview_scroll_rows(),
        state.follows_latest(),
        raw_area,
        frame,
        theme,
    );
    render_preview(
        "Smoothed presentation",
        state.controller().smoothed_text(),
        state.preview_scroll_rows(),
        state.follows_latest(),
        smoothed_area,
        frame,
        theme,
    );
    render_controls(state, controls_area, frame, theme);
}

fn preview_text(text: &str) -> Text {
    Text::from_lines(text.split('\n').map(Line::raw).collect::<Vec<_>>())
}

fn wrapped_row_count(text: &Text, width: u16) -> usize {
    let width = usize::from(width.max(1));
    text.lines
        .iter()
        .map(|line| wrap_line_word(line, width).len())
        .sum()
}

fn configurator_modal(theme: TuiTheme) -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(Size::new(54, 29), Size::new(140, 52), Insets::all(1)),
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

fn render_preview(
    title: &str,
    text: &str,
    scroll_rows: usize,
    follow_latest: bool,
    area: Rect,
    frame: &mut Frame<'_>,
    theme: TuiTheme,
) {
    let panel = Panel::new()
        .border(Border::rounded())
        .title(title)
        .padding(Insets::new(0, 1, 0, 1))
        .background(theme.overlay)
        .title_style(theme.focused)
        .content_style(theme.text);
    let inner = panel.inner_area(area);
    panel.render(area, frame);
    let preview_text = preview_text(text);
    let base_scroll =
        wrapped_row_count(&preview_text, inner.width).saturating_sub(usize::from(inner.height));
    let vertical_scroll = if follow_latest {
        base_scroll
    } else {
        base_scroll.saturating_sub(scroll_rows)
    };
    TextBlock::new(preview_text)
        .style(theme.text)
        .wrap(TextWrap::Word)
        .vertical_scroll(vertical_scroll)
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
        Line::from(""),
        label_value_line("Curve", curve, theme),
        label_value_line("Rate", &rate, theme),
        label_value_line(
            "Maximum backlog age",
            &format!("{} ms", policy.max_lag_ms),
            theme,
        ),
        label_value_line(
            "Provider preset",
            &format!("{:?}", state.source_policy().preset),
            theme,
        ),
        label_value_line(
            "Target chunk size",
            &format!("{} chars", state.source_policy().target_chunk_chars),
            theme,
        ),
        label_value_line(
            "Size variation",
            &format!("{}%", state.source_policy().chunk_size_variation_percent),
            theme,
        ),
        label_value_line(
            "Base interval",
            &format!("{} ms", state.source_policy().base_interval_ms),
            theme,
        ),
        label_value_line(
            "Interval variation",
            &format!("{}%", state.source_policy().interval_variation_percent),
            theme,
        ),
        Line::from(""),
        Line::from_spans(vec![
            Span::styled("Source: deterministic provider · ", theme.muted),
            Span::styled(
                format!(
                    "{} / {} bytes · chunk {} · {playback}",
                    state.controller().accepted_bytes(),
                    LONG_RESPONSE.len(),
                    state.controller().delivered_chunks(),
                ),
                theme.text,
            ),
        ]),
        Line::from_spans(vec![Span::styled(
            "↑↓ select  ←→ adjust  shift+←→ coarse  space toggle  pgup/pgdn scroll  end follow  r restart  p pause  enter apply  esc cancel",
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
    let preset_actions = source_preset_buttons();
    ActionRow::new(&preset_actions)
        .styles(button_styles)
        .render_state_with_id_prefix(
            geometry.source_preset,
            state.source_preset_actions(),
            frame,
            "streaming.source.preset",
        );
    for (area, action_state, id) in [
        (
            geometry.source_chunk_size,
            state.source_chunk_size_actions(),
            "streaming.source.chunk-size",
        ),
        (
            geometry.source_size_variation,
            state.source_size_variation_actions(),
            "streaming.source.size-variation",
        ),
        (
            geometry.source_interval,
            state.source_interval_actions(),
            "streaming.source.interval",
        ),
        (
            geometry.source_interval_variation,
            state.source_interval_variation_actions(),
            "streaming.source.interval-variation",
        ),
    ] {
        ActionRow::new(&numeric_actions)
            .styles(button_styles)
            .render_state_with_id_prefix(area, action_state, frame, id);
    }
    let outcome_actions = outcome_action_buttons(state.reset_pending());
    ActionRow::new(&outcome_actions)
        .styles(button_styles)
        .render_state_with_id_prefix(
            geometry.outcomes,
            state.outcome_actions(),
            frame,
            "streaming.outcomes",
        );
}

fn label_value_line(label: &str, value: &str, theme: TuiTheme) -> Line {
    Line::from_spans(vec![
        Span::styled(format!("{label}: "), theme.muted),
        Span::styled(value.to_owned(), theme.text),
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
    fn preview_text_uses_exact_bmux_word_wrap_for_tail_scroll() {
        let text = "first short line\n\nwide 東京 and cafe\u{301} plus 👩🏽‍💻\na_very_long_word_without_spaces";
        let preview = preview_text(text);
        let expected = preview
            .lines
            .iter()
            .map(|line| wrap_line_word(line, 12).len())
            .sum::<usize>();
        assert_eq!(wrapped_row_count(&preview, 12), expected);
        assert_eq!(preview.lines.len(), 4);
    }

    #[test]
    fn preview_tail_fills_down_then_keeps_latest_at_bottom() {
        let theme = TuiTheme::for_theme_id("bcode-dark");
        let area = Rect::new(0, 0, 24, 6);
        let render = |text: &str| {
            let mut buffer = Buffer::empty(area);
            render_preview(
                "Preview",
                text,
                0,
                true,
                area,
                &mut Frame::new(&mut buffer),
                theme,
            );
            (0..area.height)
                .filter_map(|row| buffer.row_symbols(row))
                .collect::<Vec<_>>()
        };
        let short = render("one\ntwo");
        assert!(short[1].contains("one"));
        assert!(short[2].contains("two"));
        let long = render("one\ntwo\nthree\nfour\nfive\nsix");
        assert!(!long.iter().any(|row| row.contains("one")));
        assert!(long[4].contains("six"), "{long:?}");
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
    fn control_labels_and_components_do_not_overlap_or_clip() {
        let now = std::time::Instant::now();
        let state = StreamingConfiguratorState::new(
            now,
            bcode_session_view_models::StreamingPresentationPolicy::default(),
            bcode_session_view_models::StreamingPresentationPolicy::default(),
        );
        let text = render_text(Rect::new(0, 0, 120, 44), &state);
        for forbidden in ["Enabledyes", "Curve: l[", "char[", "ms["] {
            assert!(
                !text.contains(forbidden),
                "overlap/clipping {forbidden}: {text}"
            );
        }
        for expected in [
            "[x] Enabled",
            "Curve: linear",
            "[ Linear ]",
            "[ − ] [ + ]",
            "Provider preset: Balanced",
            "[ Balanced ]",
            "[ Reset ]",
            "[ Apply ]",
            "[ Cancel ]",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
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
            "chunk 0",
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
        for _ in 0..9 {
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
        assert!(reset.contains("[ Reset pending ]"), "{reset}");
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
        let mut cursor = now;
        while !state.controller().is_completed() {
            let Some(deadline) = state.next_deadline(cursor) else {
                break;
            };
            cursor = deadline;
            let _ = state.advance(cursor);
        }
        let completed = render_text(Rect::new(0, 0, 120, 50), &state);
        assert!(completed.contains("completed"), "{completed}");
        assert_eq!(
            state.controller().raw_text(),
            state.controller().smoothed_text()
        );
        assert!(state.controller().raw_text().contains("cafe\u{301}"));
        assert!(state.controller().raw_text().contains("👩🏽‍💻"));
        assert!(state.controller().raw_text().contains("東京"));

        for _ in 0..9 {
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
        assert!(reset.contains("[ Reset pending ]"), "{reset}");
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
