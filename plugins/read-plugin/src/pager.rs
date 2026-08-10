use bcode_markdown_render::{MarkdownRenderOptions, MarkdownTheme, render_markdown_lines};
#[cfg(test)]
use bmux_keyboard::Modifiers;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Color, Line, Style, Terminal};
use bmux_tui_components::key_hint_bar::{KeyHint, KeyHintBar, KeyHintBarStyles};
use bmux_tui_components::text_view::{TextView, TextViewPolicy, TextViewState, TextViewStyles};

const FOOTER_HEIGHT: u16 = 1;

#[derive(Debug)]
pub(crate) struct Pager {
    markdown: String,
    lines: Vec<Line>,
    width: u16,
    height: u16,
    view_state: TextViewState,
    styled: bool,
}

impl Pager {
    pub(crate) fn new(markdown: String, width: u16, height: u16, styled: bool) -> Self {
        let mut pager = Self {
            markdown,
            lines: Vec::new(),
            width: width.max(1),
            height: height.max(1),
            view_state: TextViewState::new(),
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
        self.view_state
            .set_vertical_scroll(self.view_state.vertical_scroll().min(self.max_offset()));
    }

    pub(crate) fn handle_key(&mut self, stroke: KeyStroke) -> bool {
        if !stroke.modifiers.is_empty() {
            return false;
        }
        if matches!(stroke.key, KeyCode::Escape | KeyCode::Char('q')) {
            return true;
        }
        let event = match stroke.key {
            KeyCode::Char('j') => Event::Key(KeyStroke::simple(KeyCode::Down)),
            KeyCode::Char('k') => Event::Key(KeyStroke::simple(KeyCode::Up)),
            KeyCode::Space => Event::Key(KeyStroke::simple(KeyCode::PageDown)),
            KeyCode::Char('b') => Event::Key(KeyStroke::simple(KeyCode::PageUp)),
            KeyCode::Char('g') => Event::Key(KeyStroke::simple(KeyCode::Home)),
            KeyCode::Char('G') => Event::Key(KeyStroke::simple(KeyCode::End)),
            _ => Event::Key(stroke),
        };
        let area = Rect::new(0, 0, self.width, self.content_height());
        let view = TextView::new(&self.lines).policy(TextViewPolicy::scrollable());
        let _ = view.handle_event(area, &mut self.view_state, &event);
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
                let content_area = Rect::new(0, 0, self.width, self.content_height());
                TextView::new(&self.lines)
                    .policy(TextViewPolicy::scrollable())
                    .styles(TextViewStyles {
                        text: Style::new(),
                        empty: Style::new(),
                        background: Style::new(),
                    })
                    .render(content_area, &self.view_state, frame);
                if self.height > 1 {
                    let hints = [
                        KeyHint::new("j/k", "scroll"),
                        KeyHint::new("space/b", "page"),
                        KeyHint::new("g/G", "ends"),
                        KeyHint::new("q", "quit"),
                    ];
                    let styles = pager_hint_styles(self.styled);
                    let footer_area = Rect::new(0, self.height - 1, self.width, 1);
                    KeyHintBar::new(&hints)
                        .styles(styles)
                        .render(footer_area, frame);
                    let position = self.position_label();
                    let x = self
                        .width
                        .saturating_sub(u16::try_from(position.len()).unwrap_or(u16::MAX));
                    frame.write_line(
                        Rect::new(x, self.height - 1, self.width.saturating_sub(x), 1),
                        &Line::from(position),
                    );
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

    fn position_label(&self) -> String {
        let total = self.lines.len();
        let position = if total == 0 {
            0
        } else {
            self.view_state
                .vertical_scroll()
                .saturating_add(1)
                .min(total)
        };
        format!(" {position}/{total} ")
    }
}

const fn pager_hint_styles(styled: bool) -> KeyHintBarStyles {
    let muted = if styled {
        Style::new().fg(Color::BrightBlack)
    } else {
        Style::new()
    };
    KeyHintBarStyles {
        key: muted,
        label: muted,
        separator: muted,
        disabled: muted,
        background: Style::new(),
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
        assert_eq!(pager.view_state.vertical_scroll(), 0);
        pager.handle_key(key(KeyCode::End));
        assert_eq!(pager.view_state.vertical_scroll(), pager.max_offset());
        pager.handle_key(key(KeyCode::Down));
        assert_eq!(pager.view_state.vertical_scroll(), pager.max_offset());
        pager.handle_key(key(KeyCode::Home));
        assert_eq!(pager.view_state.vertical_scroll(), 0);
    }

    #[test]
    fn page_navigation_uses_content_height() {
        let mut pager = long_pager();
        pager.handle_key(key(KeyCode::Space));
        assert_eq!(pager.view_state.vertical_scroll(), 5);
        pager.handle_key(key(KeyCode::Char('b')));
        assert_eq!(pager.view_state.vertical_scroll(), 0);
    }

    #[test]
    fn resize_reflows_and_clamps_offset() {
        let mut pager = Pager::new("word ".repeat(100), 10, 4, true);
        pager.handle_key(key(KeyCode::End));
        assert!(pager.view_state.vertical_scroll() > 0);
        pager.resize(200, 50);
        assert_eq!(pager.view_state.vertical_scroll(), 0);
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
            assert!(pager.view_state.vertical_scroll() <= pager.max_offset());
            pager.handle_key(key(KeyCode::End));
            assert_eq!(pager.view_state.vertical_scroll(), pager.max_offset());
            pager.handle_key(key(KeyCode::Home));
            assert_eq!(pager.view_state.vertical_scroll(), 0);
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
            assert_eq!(
                left.view_state.vertical_scroll(),
                right.view_state.vertical_scroll()
            );
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
        assert_eq!(pager.view_state.vertical_scroll(), 0);
    }

    #[test]
    fn monochrome_theme_has_no_styles() {
        let theme = monochrome_theme();
        assert_eq!(theme.text, Style::new());
        assert_eq!(theme.heading, Style::new());
        assert_eq!(theme.inline_code, Style::new());
    }
}
