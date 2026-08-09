//! Bcode latest-activity presentation recipes.

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

#[cfg(test)]
mod tests {
    use super::*;

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
