//! Reusable Bcode transcript tool-card header composition.

use bmux_tui::prelude::{Line, Span, Style};

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
