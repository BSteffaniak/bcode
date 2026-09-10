//! ANSI source presentation. Terminal commands never become cell text.

use bmux_tui::prelude::Line;

/// Parse a source preview's ANSI styling without executing terminal commands.
/// Styles carry across logical lines, but each preview starts with a fresh style.
#[must_use]
pub fn ansi_source(text: &str) -> Option<Vec<Line>> {
    text.contains('\x1b')
        .then(|| bmux_tui::ansi::ansi_to_lines(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::prelude::{Color, Modifier};

    #[test]
    fn extended_colors_are_normalized_into_styles() {
        let lines =
            ansi_source("\x1b[38;5;208morange\x1b[0m \x1b[38;2;180;100;255mpurple").unwrap();
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Indexed(208)));
        assert_eq!(lines[0].spans[2].style.fg, Some(Color::Rgb(180, 100, 255)));
    }

    #[test]
    fn ansi_styles_cross_lines_and_reset_without_leaking_commands() {
        let lines =
            ansi_source("\x1b[31;44;1;3;4;7mred\ncontinued\x1b[0m plain\x1b]0;title\x07\x1b[2J")
                .unwrap();
        let style = lines[0].spans[0].style;
        assert_eq!(style.fg, Some(Color::Red));
        assert_eq!(style.bg, Some(Color::Blue));
        assert!(style.modifiers.contains(
            Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINE | Modifier::REVERSED
        ));
        assert_eq!(lines[1].spans[0].style, style);
        assert_eq!(lines[1].spans[1].style, bmux_tui::prelude::Style::default());
        assert_eq!(lines[1].plain_text(), "continued plain");
        assert!(
            lines
                .iter()
                .all(|line| !line.plain_text().chars().any(char::is_control))
        );
    }
}
