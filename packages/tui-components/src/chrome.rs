//! Bcode status and header chrome composition with bounded width degradation.

use bmux_tui::prelude::{Span, Style};
use bmux_tui::text_width::display_width;
use unicode_segmentation::UnicodeSegmentation;

/// One semantic Bcode chrome segment.
#[derive(Debug, Clone)]
pub struct ChromeSegment {
    text: String,
    style: Style,
    priority: u8,
    truncatable: bool,
}

impl ChromeSegment {
    /// Construct a required segment.
    #[must_use]
    pub const fn required(text: String, style: Style, truncatable: bool) -> Self {
        Self {
            text,
            style,
            priority: u8::MAX,
            truncatable,
        }
    }

    /// Construct an optional segment. Lower priorities disappear first.
    #[must_use]
    pub const fn optional(text: String, style: Style, priority: u8, truncatable: bool) -> Self {
        Self {
            text,
            style,
            priority,
            truncatable,
        }
    }
}

/// A Bcode chrome line that drops optional segments and truncates designated content.
pub struct ChromeLine {
    separator: String,
    separator_style: Style,
    segments: Vec<ChromeSegment>,
}

impl ChromeLine {
    /// Construct an empty line.
    #[must_use]
    pub fn new(separator: impl Into<String>, separator_style: Style) -> Self {
        Self {
            separator: separator.into(),
            separator_style,
            segments: Vec::new(),
        }
    }

    /// Add a required segment.
    #[must_use]
    pub fn required(mut self, text: String, style: Style, truncatable: bool) -> Self {
        self.segments
            .push(ChromeSegment::required(text, style, truncatable));
        self
    }

    /// Add a non-empty optional segment.
    #[must_use]
    pub fn optional(mut self, text: String, style: Style, priority: u8, truncatable: bool) -> Self {
        if !text.is_empty() {
            self.segments
                .push(ChromeSegment::optional(text, style, priority, truncatable));
        }
        self
    }

    /// Resolve styled spans within the available width.
    #[must_use]
    pub fn spans(mut self, width: usize) -> Vec<Span> {
        while self.width() > width {
            let Some(index) = self
                .segments
                .iter()
                .enumerate()
                .filter(|(_, segment)| segment.priority < u8::MAX)
                .min_by_key(|(_, segment)| segment.priority)
                .map(|(index, _)| index)
            else {
                break;
            };
            self.segments.remove(index);
        }
        if self.width() > width {
            let fixed = self.width().saturating_sub(
                self.segments
                    .iter()
                    .filter(|segment| segment.truncatable)
                    .map(|segment| display_width(&segment.text))
                    .sum::<usize>(),
            );
            let count = self
                .segments
                .iter()
                .filter(|segment| segment.truncatable)
                .count();
            if let Some(each) = width.saturating_sub(fixed).checked_div(count) {
                for segment in self
                    .segments
                    .iter_mut()
                    .filter(|segment| segment.truncatable)
                {
                    segment.text = truncate_chrome_part(&segment.text, each);
                }
            }
        }
        let mut spans = Vec::new();
        for (index, segment) in self.segments.into_iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(self.separator.clone(), self.separator_style));
            }
            spans.push(Span::styled(segment.text, segment.style));
        }
        spans
    }

    fn width(&self) -> usize {
        self.segments
            .iter()
            .map(|segment| display_width(&segment.text))
            .sum::<usize>()
            .saturating_add(
                self.segments
                    .len()
                    .saturating_sub(1)
                    .saturating_mul(display_width(&self.separator)),
            )
    }
}

fn truncate_chrome_part(part: &str, max_width: usize) -> String {
    if display_width(part) <= max_width {
        return part.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_owned();
    }
    let mut suffix = String::new();
    let mut width = 1_usize;
    for grapheme in part.graphemes(true).rev() {
        let grapheme_width = display_width(grapheme);
        if width.saturating_add(grapheme_width) > max_width {
            break;
        }
        suffix.insert_str(0, grapheme);
        width = width.saturating_add(grapheme_width);
    }
    format!("…{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_lines_drop_low_priority_before_truncating_required_content() {
        let spans = ChromeLine::new(" · ", Style::new())
            .required("required-content".to_owned(), Style::new(), true)
            .optional("low".to_owned(), Style::new(), 1, false)
            .optional("high".to_owned(), Style::new(), 10, false)
            .spans(12);
        let text = spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(!text.contains("low"));
        assert!(display_width(&text) <= 12);
    }
}
