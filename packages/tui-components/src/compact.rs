//! Compact, width-aware transcript presentation helpers.

use bmux_tui::prelude::{Line, Span, Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Render a title and metadata, wrapping metadata onto indented continuation rows as needed.
#[must_use]
pub fn header_rows(
    marker: Span,
    title: Span,
    metadata: impl IntoIterator<Item = Span>,
    width: u16,
    separator_style: Style,
) -> Vec<Line> {
    let mut current = Line::from_spans(vec![marker, title]);
    let mut rows = Vec::new();
    for item in metadata {
        if item.content.is_empty() {
            continue;
        }
        let separator = Span::styled(" · ", separator_style);
        let current_width = line_width(&current);
        let added_width = UnicodeWidthStr::width(separator.content.as_str())
            + UnicodeWidthStr::width(item.content.as_str());
        if current_width + added_width <= usize::from(width) {
            current.spans.push(separator);
            current.spans.push(item);
        } else {
            rows.push(current);
            current = Line::from_spans(vec![Span::raw("  "), item]);
        }
    }
    rows.push(current);
    rows
}

/// Truncate text to a terminal-cell width while preserving grapheme boundaries.
#[must_use]
pub fn truncate_width(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let target = width.saturating_sub(1);
    let mut output = String::new();
    for grapheme in value.graphemes(true) {
        if UnicodeWidthStr::width(output.as_str()) + UnicodeWidthStr::width(grapheme) > target {
            break;
        }
        output.push_str(grapheme);
    }
    output.push('…');
    output
}

/// Format a byte count for compact display.
#[must_use]
pub fn bytes(value: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut divisor = 1_u64;
    let mut unit = 0;
    while value / divisor >= 1024 && unit < UNITS.len() - 1 {
        divisor *= 1024;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        let whole = value / divisor;
        let decimal = value % divisor * 10 / divisor;
        if whole >= 10 || decimal == 0 {
            format!("{whole} {}", UNITS[unit])
        } else {
            format!("{whole}.{decimal} {}", UNITS[unit])
        }
    }
}

fn line_width(line: &Line) -> usize {
    line.spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_str()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_header_metadata() {
        let rows = header_rows(
            Span::raw("◆ "),
            Span::raw("Read"),
            [Span::raw("a.rs"), Span::raw("lines 1–20")],
            18,
            Style::new(),
        );
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn truncates_unicode_by_display_width() {
        assert_eq!(truncate_width("ab🙂cd", 5), "ab🙂…");
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KB");
    }
}
