//! Literal source presentation; never interpret file bytes as terminal commands.

use std::borrow::Cow;

/// Escape controls before measuring source text, retaining logical line boundaries.
#[must_use]
pub fn visible_source(text: &str) -> Cow<'_, str> {
    if !text.chars().any(|ch| ch.is_control() && ch != '\n') {
        return Cow::Borrowed(text);
    }
    let mut output = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_control() && ch != '\n' {
            output.extend(ch.escape_default());
        } else {
            output.push(ch);
        }
    }
    Cow::Owned(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_are_visible_without_changing_unicode_or_line_boundaries() {
        assert_eq!(
            visible_source("\x1b[31m界\x1b[0m\n\t\r\x07\u{9b}"),
            "\\u{1b}[31m界\\u{1b}[0m\n\\t\\r\\u{7}\\u{9b}"
        );
        assert!(matches!(visible_source("plain 👩‍💻\n"), Cow::Borrowed(_)));
    }
}
