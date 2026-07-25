use bcode_markdown_render::{MarkdownRenderOptions, MarkdownTheme, render_markdown_lines};
#[cfg(test)]
use bmux_keyboard::Modifiers;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Color, Line, Span, Style, Terminal};

const FOOTER_HEIGHT: u16 = 1;

#[derive(Debug)]
pub(crate) struct Pager {
    markdown: String,
    lines: Vec<Line>,
    width: u16,
    height: u16,
    offset: usize,
    styled: bool,
}

impl Pager {
    pub(crate) fn new(markdown: String, width: u16, height: u16, styled: bool) -> Self {
        let mut pager = Self {
            markdown,
            lines: Vec::new(),
            width: width.max(1),
            height: height.max(1),
            offset: 0,
            styled,
        };
        pager.render_markdown();
        pager
    }

    pub(crate) fn resize(&mut self, width: u16, height: u16) {
        let width = width.max(1);
        let height = height.max(1);
        if self.width == width && self.height == height {
            return;
        }
        self.width = width;
        self.height = height;
        self.render_markdown();
        self.clamp_offset();
    }

    pub(crate) fn handle_key(&mut self, stroke: KeyStroke) -> bool {
        if !stroke.modifiers.is_empty() {
            return false;
        }
        match stroke.key {
            KeyCode::Escape | KeyCode::Char('q') => return true,
            KeyCode::Down | KeyCode::Char('j') => self.offset = self.offset.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => self.offset = self.offset.saturating_sub(1),
            KeyCode::Space | KeyCode::PageDown => {
                self.offset = self.offset.saturating_add(self.page_size());
            }
            KeyCode::Char('b') | KeyCode::PageUp => {
                self.offset = self.offset.saturating_sub(self.page_size());
            }
            KeyCode::Home | KeyCode::Char('g') => self.offset = 0,
            KeyCode::End | KeyCode::Char('G') => self.offset = self.max_offset(),
            _ => {}
        }
        self.clamp_offset();
        false
    }

    pub(crate) fn draw<W: std::io::Write>(
        &self,
        terminal: &mut Terminal<W>,
    ) -> std::io::Result<()> {
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.fill(area, " ", Style::new());
                for (row, line) in self
                    .lines
                    .iter()
                    .skip(self.offset)
                    .take(usize::from(self.content_height()))
                    .enumerate()
                {
                    frame.write_line(
                        Rect::new(0, u16::try_from(row).unwrap_or(u16::MAX), self.width, 1),
                        line,
                    );
                }
                if self.height > 1 {
                    frame.write_line(Rect::new(0, self.height - 1, self.width, 1), &self.footer());
                }
            })
            .map(|_| ())
    }

    fn render_markdown(&mut self) {
        let theme = if self.styled {
            MarkdownTheme::default()
        } else {
            monochrome_theme()
        };
        self.lines = render_markdown_lines(
            &self.markdown,
            MarkdownRenderOptions::new(self.width).with_theme(theme),
        );
    }

    const fn content_height(&self) -> u16 {
        if self.height > FOOTER_HEIGHT {
            self.height - FOOTER_HEIGHT
        } else {
            self.height
        }
    }

    fn page_size(&self) -> usize {
        usize::from(self.content_height().max(1))
    }

    fn max_offset(&self) -> usize {
        self.lines.len().saturating_sub(self.page_size())
    }

    fn clamp_offset(&mut self) {
        self.offset = self.offset.min(self.max_offset());
    }

    fn footer(&self) -> Line {
        let total = self.lines.len();
        let position = if total == 0 {
            0
        } else {
            self.offset.saturating_add(1).min(total)
        };
        let content = format!(" {position}/{total}  j/k scroll  space/b page  g/G ends  q quit");
        let style = if self.styled {
            Style::new().fg(Color::BrightBlack)
        } else {
            Style::new()
        };
        Line::from_spans(vec![Span::styled(content, style)])
    }
}

const fn monochrome_theme() -> MarkdownTheme {
    let plain = Style::new();
    MarkdownTheme {
        text: plain,
        heading: plain,
        link: plain,
        strong: plain,
        emphasis: plain,
        strikethrough: plain,
        inline_code: plain,
        code_block_text: plain,
        code_block_border: plain,
        blockquote_bar: plain,
        alert_note: plain,
        alert_tip: plain,
        alert_important: plain,
        alert_warning: plain,
        alert_caution: plain,
        list_marker: plain,
        task_checked: plain,
        task_unchecked: plain,
        table_border: plain,
        horizontal_rule: plain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_pager() -> Pager {
        use std::fmt::Write as _;

        let markdown = (0..20).fold(String::new(), |mut output, line| {
            write!(output, "{line}\n\n").expect("writing to a string cannot fail");
            output
        });
        Pager::new(markdown, 40, 6, true)
    }

    fn key(key: KeyCode) -> KeyStroke {
        KeyStroke::simple(key)
    }

    #[test]
    fn navigation_saturates_at_document_bounds() {
        let mut pager = long_pager();
        pager.handle_key(key(KeyCode::Up));
        assert_eq!(pager.offset, 0);
        pager.handle_key(key(KeyCode::End));
        assert_eq!(pager.offset, pager.max_offset());
        pager.handle_key(key(KeyCode::Down));
        assert_eq!(pager.offset, pager.max_offset());
        pager.handle_key(key(KeyCode::Home));
        assert_eq!(pager.offset, 0);
    }

    #[test]
    fn page_navigation_uses_content_height() {
        let mut pager = long_pager();
        pager.handle_key(key(KeyCode::Space));
        assert_eq!(pager.offset, 5);
        pager.handle_key(key(KeyCode::Char('b')));
        assert_eq!(pager.offset, 0);
    }

    #[test]
    fn resize_reflows_and_clamps_offset() {
        let mut pager = Pager::new("word ".repeat(100), 10, 4, true);
        pager.handle_key(key(KeyCode::End));
        assert!(pager.offset > 0);
        pager.resize(200, 50);
        assert_eq!(pager.offset, 0);
    }

    #[test]
    fn quit_keys_exit() {
        let mut pager = long_pager();
        assert!(pager.handle_key(key(KeyCode::Escape)));
        assert!(pager.handle_key(key(KeyCode::Char('q'))));
        assert!(!pager.handle_key(key(KeyCode::Char('x'))));
    }

    #[test]
    fn empty_short_and_tiny_viewports_stay_in_bounds() {
        for (markdown, width, height) in [
            (String::new(), 0, 0),
            ("short".to_owned(), 80, 24),
            ("word ".repeat(20), 1, 1),
        ] {
            let mut pager = Pager::new(markdown, width, height, true);
            pager.handle_key(key(KeyCode::Down));
            pager.handle_key(key(KeyCode::PageDown));
            assert!(pager.offset <= pager.max_offset());
            pager.handle_key(key(KeyCode::End));
            assert_eq!(pager.offset, pager.max_offset());
            pager.handle_key(key(KeyCode::Home));
            assert_eq!(pager.offset, 0);
        }
    }

    #[test]
    fn arrow_page_and_character_aliases_are_equivalent() {
        for (first, second) in [
            (KeyCode::Down, KeyCode::Char('j')),
            (KeyCode::Up, KeyCode::Char('k')),
            (KeyCode::PageDown, KeyCode::Space),
            (KeyCode::PageUp, KeyCode::Char('b')),
            (KeyCode::Home, KeyCode::Char('g')),
            (KeyCode::End, KeyCode::Char('G')),
        ] {
            let mut left = long_pager();
            let mut right = long_pager();
            left.handle_key(key(KeyCode::End));
            right.handle_key(key(KeyCode::End));
            left.handle_key(key(first));
            right.handle_key(key(second));
            assert_eq!(left.offset, right.offset);
        }
    }

    #[test]
    fn modified_keys_are_ignored() {
        let mut pager = long_pager();
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Char('q'),
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        assert!(!pager.handle_key(stroke));
        assert_eq!(pager.offset, 0);
    }

    #[test]
    fn monochrome_theme_has_no_styles() {
        let theme = monochrome_theme();
        assert_eq!(theme.text, Style::new());
        assert_eq!(theme.heading, Style::new());
        assert_eq!(theme.inline_code, Style::new());
    }
}
