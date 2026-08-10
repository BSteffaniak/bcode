//! Reusable Bcode transcript tool-card header composition.

use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui_components::theme::ComponentTheme;

/// Semantic presentation for a Bcode transcript tool card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolCardStyle {
    /// Card marker/accent style.
    pub accent: Style,
    /// Card title style.
    pub title: Style,
    /// Field-label and secondary metadata style.
    pub muted: Style,
    /// Field-value and body style.
    pub value: Style,
    /// Link and external-destination style.
    pub link: Style,
    /// Successful change or outcome style.
    pub success: Style,
    /// Caution, cursor, or pending style.
    pub warning: Style,
    /// Failed change or outcome style.
    pub error: Style,
}

impl ToolCardStyle {
    /// Resolve renderer-owned component presentation with a terminal-safe fallback.
    #[must_use]
    pub fn from_component_theme(theme: Option<ComponentTheme>) -> Self {
        let theme = theme.unwrap_or_default();
        Self {
            accent: theme.focused,
            title: theme.text,
            muted: theme.muted,
            value: theme.text,
            link: theme.info,
            success: theme.success,
            warning: theme.warning,
            error: theme.error,
        }
    }
}

/// Render a bounded Bcode tool-card header with semantic icon, title, and metadata spans.
#[must_use]
pub fn tool_card_header_rows(
    icon: Span,
    title: Span,
    metadata: impl IntoIterator<Item = Span>,
    width: u16,
    separator_style: Style,
) -> Vec<Line> {
    crate::compact::header_rows(icon, title, metadata, width, separator_style)
}

/// Render the single-line form of a Bcode tool-card header.
#[must_use]
pub fn tool_card_header(icon: Span, title: Span) -> Line {
    Line::from_spans(vec![icon, title])
}

/// Append a Bcode tool-card key/value row when a value is present.
pub fn push_tool_card_detail(
    rows: &mut Vec<Line>,
    key: &str,
    value: Option<&str>,
    label_style: Style,
    value_style: Style,
) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        rows.push(Line::from_spans(vec![
            Span::styled(format!("  {key}: "), label_style),
            Span::styled(value.to_owned(), value_style),
        ]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_card_style_uses_component_theme_roles() {
        let theme = ComponentTheme {
            focused: Style::new().fg(bmux_tui::style::Color::Cyan),
            text: Style::new().fg(bmux_tui::style::Color::White),
            muted: Style::new().fg(bmux_tui::style::Color::BrightBlack),
            info: Style::new().fg(bmux_tui::style::Color::Blue),
            success: Style::new().fg(bmux_tui::style::Color::Green),
            warning: Style::new().fg(bmux_tui::style::Color::Yellow),
            error: Style::new().fg(bmux_tui::style::Color::Red),
            ..ComponentTheme::default()
        };
        let style = ToolCardStyle::from_component_theme(Some(theme));
        assert_eq!(style.accent, theme.focused);
        assert_eq!(style.title, theme.text);
        assert_eq!(style.muted, theme.muted);
        assert_eq!(style.value, theme.text);
        assert_eq!(style.link, theme.info);
        assert_eq!(style.success, theme.success);
        assert_eq!(style.warning, theme.warning);
        assert_eq!(style.error, theme.error);
    }

    #[test]
    fn detail_omits_missing_values() {
        let mut rows = Vec::new();
        push_tool_card_detail(&mut rows, "path", None, Style::new(), Style::new());
        assert!(rows.is_empty());
        push_tool_card_detail(
            &mut rows,
            "path",
            Some("src/lib.rs"),
            Style::new(),
            Style::new(),
        );
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn tool_card_header_wraps_metadata_within_width() {
        let rows = tool_card_header_rows(
            Span::raw("◆ "),
            Span::raw("Tool"),
            [Span::raw("long metadata"), Span::raw("more")],
            12,
            Style::new(),
        );
        assert!(rows.len() > 1);
    }
}
