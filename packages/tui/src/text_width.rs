//! Unicode-aware text measurement and wrapping helpers for TUI rendering.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Return the display width of a string in terminal cells.
#[must_use]
pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Wrap text at grapheme boundaries with a distinct first-line width.
#[must_use]
pub fn wrap_text_with_continuation(
    text: &str,
    first_width: usize,
    continuation_width: usize,
) -> Vec<String> {
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    let mut max_width = first_width.max(1);
    let continuation_width = continuation_width.max(1);

    for grapheme in text.graphemes(true) {
        let width = display_width(grapheme);
        if current_width > 0 && current_width.saturating_add(width) > max_width {
            rows.push(current);
            current = String::new();
            current_width = 0;
            max_width = continuation_width;
        }
        current.push_str(grapheme);
        current_width = current_width.saturating_add(width);
    }
    rows.push(current);
    rows
}

/// Truncate text to a terminal display width, appending an ellipsis when clipped.
#[must_use]
pub fn truncate_to_display_width(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }

    let mut output = String::new();
    let mut used = 0usize;
    let body_width = width.saturating_sub(1);
    for grapheme in text.graphemes(true) {
        let grapheme_width = display_width(grapheme);
        if used.saturating_add(grapheme_width) > body_width {
            output.push('…');
            return output;
        }
        output.push_str(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{display_width, truncate_to_display_width, wrap_text_with_continuation};

    #[test]
    fn wraps_combining_graphemes_without_splitting_marks() {
        let rows = wrap_text_with_continuation("e\u{301}e\u{301}e\u{301}", 2, 2);

        assert_eq!(rows, vec!["e\u{301}e\u{301}", "e\u{301}"]);
    }

    #[test]
    fn measures_emoji_sequence_as_a_grapheme() {
        assert_eq!(display_width("👨‍👩‍👧‍👦"), 2);
    }

    #[test]
    fn truncates_at_grapheme_boundaries() {
        assert_eq!(truncate_to_display_width("ab👨‍👩‍👧‍👦cd", 5), "ab👨‍👩‍👧‍👦…");
    }
}
