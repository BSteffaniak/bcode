//! Bcode latest-activity presentation recipes.

use std::time::Duration;

use bmux_tui::prelude::{Line, Modifier, Span, Style};
use bmux_tui::text_width::{display_width, truncate_to_display_width};

/// Semantic styles for the stale latest-activity banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatestActivityStyle {
    /// Banner surface.
    pub background: Style,
    /// Inactive message style.
    pub muted: Style,
    /// Jump affordance style.
    pub info: Style,
}

/// Render the non-animated latest-activity banner.
#[must_use]
pub fn stale_latest_activity_line(width: u16, key_label: &str, style: LatestActivityStyle) -> Line {
    let width = usize::from(width);
    let message = if width < 30 {
        format!("messages below · {key_label}")
    } else {
        format!("New messages below · {key_label} to jump")
    };
    let text = truncate_to_display_width(&message, width.saturating_sub(1));
    let text_width = display_width(&text);
    let left_width = width.saturating_sub(1).saturating_sub(text_width) / 2;
    let right_width = width
        .saturating_sub(1)
        .saturating_sub(text_width)
        .saturating_sub(left_width);
    Line::from_spans(vec![
        Span::styled(" ".repeat(left_width), style.background),
        Span::styled(text, style.background.patch(style.muted)),
        Span::styled(" ".repeat(right_width), style.background),
        Span::styled(
            "▾",
            style
                .background
                .patch(style.info)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

/// Semantic styles for the glowing latest-activity banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlowingLatestActivityStyle {
    /// Banner surface.
    pub background: Style,
    /// Message style while glowing.
    pub text: Style,
    /// Glow peak style.
    pub info: Style,
    /// Glow trail style.
    pub accent: Style,
}

/// Glow intensity of a burst of activity `age` into its glow `window`: fades from the burst
/// (clamped to 1..=8) toward 1 over the window.
#[must_use]
pub fn latest_activity_effective_burst(burst: u8, age: Duration, window: Duration) -> u8 {
    let window_ms = window.as_millis().max(1);
    let remaining = window_ms.saturating_sub(age.as_millis());
    let scaled = (u128::from(burst) * remaining).div_ceil(window_ms);
    u8::try_from(scaled.clamp(1, 8)).unwrap_or(1)
}

/// Animation frame period of the glow at intensity `burst`: stronger bursts animate faster.
#[must_use]
pub fn latest_activity_frame(burst: u8) -> Duration {
    Duration::from_millis(
        210_u64
            .saturating_sub(u64::from(burst).saturating_mul(21))
            .max(36),
    )
}

/// Render the glowing latest-activity banner at intensity `burst`.
///
/// `phase` is the number of [`latest_activity_frame`] periods since the glow started. The
/// banner is the centered message between two glow rails moving toward it.
#[must_use]
pub fn glowing_latest_activity_line(
    width: u16,
    key_label: &str,
    burst: u8,
    phase: usize,
    style: GlowingLatestActivityStyle,
) -> Line {
    let width = usize::from(width);
    let message = if width < 30 {
        format!("messages below · {key_label}")
    } else {
        format!("New messages below · {key_label} to jump")
    };
    let text = truncate_to_display_width(&message, width);
    let text_width = display_width(&text);
    let left_width = width.saturating_sub(text_width) / 2;
    let right_width = width.saturating_sub(text_width).saturating_sub(left_width);
    let mut spans = Vec::new();
    push_glow_rail(&mut spans, left_width, phase, burst, false, style);
    spans.push(Span::styled(
        text,
        style
            .background
            .patch(style.text)
            .add_modifier(Modifier::BOLD),
    ));
    push_glow_rail(
        &mut spans,
        right_width,
        phase.saturating_add(left_width / 3),
        burst,
        true,
        style,
    );
    Line::from_spans(spans)
}

fn push_glow_rail(
    spans: &mut Vec<Span>,
    width: usize,
    phase: usize,
    burst: u8,
    reverse: bool,
    style: GlowingLatestActivityStyle,
) {
    const LOW_GLYPHS: [&str; 3] = ["·", "•", "▾"];
    const HIGH_GLYPHS: [&str; 3] = ["·", "◆", "▾"];
    let glyphs = if burst >= 5 { HIGH_GLYPHS } else { LOW_GLYPHS };
    let intensity = usize::from(burst.min(8));
    let period = 18_usize.saturating_sub(intensity.saturating_mul(2)).max(4);
    let trail = 1_usize.saturating_add(intensity);
    let phase_step = 1_usize.saturating_add(intensity / 3);
    for column in 0..width {
        let wave_column = if reverse {
            width.saturating_sub(column).saturating_sub(1)
        } else {
            column
        };
        let wave = wave_column.saturating_add(phase.saturating_mul(phase_step)) % period;
        let distance = wave.min(period.saturating_sub(wave));
        let glyph = match distance {
            0 => glyphs[2],
            1 | 2 => glyphs[1],
            _ if distance <= trail => glyphs[0],
            _ => {
                spans.push(Span::styled(" ", style.background));
                continue;
            }
        };
        let role = if distance == 0 {
            style.info
        } else {
            style.accent
        };
        let mut cell = style.background.patch(role);
        if distance == 0 || (intensity >= 3 && distance <= 1) || (intensity >= 7 && distance <= 2) {
            cell = cell.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(glyph, cell));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glowing(width: u16, burst: u8) -> Line {
        glowing_latest_activity_line(
            width,
            "end",
            burst,
            0,
            GlowingLatestActivityStyle {
                background: Style::new(),
                text: Style::new(),
                info: Style::new(),
                accent: Style::new(),
            },
        )
    }

    fn text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_str())
            .collect()
    }

    #[test]
    fn glowing_banner_fills_the_width_and_strong_bursts_use_high_glyphs() {
        for width in [0, 1, 12, 29, 30, 80] {
            assert_eq!(display_width(&text(&glowing(width, 3))), usize::from(width));
        }
        let weak = text(&glowing(80, 3));
        let strong = text(&glowing(80, 6));
        assert!(weak.contains("New messages below · end to jump"));
        assert!(weak.contains('•') && !weak.contains('◆'));
        assert!(strong.contains('◆') && !strong.contains('•'));
    }

    #[test]
    fn glow_fades_over_its_window_and_strong_bursts_animate_faster() {
        let window = Duration::from_millis(400);
        assert_eq!(
            latest_activity_effective_burst(8, Duration::ZERO, window),
            8
        );
        assert_eq!(latest_activity_effective_burst(8, window / 2, window), 4);
        assert_eq!(latest_activity_effective_burst(8, window, window), 1);
        assert_eq!(
            latest_activity_effective_burst(0, Duration::ZERO, window),
            1
        );
        assert!(latest_activity_frame(8) < latest_activity_frame(1));
        assert_eq!(latest_activity_frame(20), Duration::from_millis(36));
    }

    #[test]
    fn narrow_banner_uses_compact_copy_and_fits() {
        let line = stale_latest_activity_line(
            20,
            "end",
            LatestActivityStyle {
                background: Style::new(),
                muted: Style::new(),
                info: Style::new(),
            },
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(text.contains("messages below"));
        assert_eq!(display_width(&text), 20);
    }
}
