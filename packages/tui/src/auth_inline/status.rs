//! Inline presentation for provider-owned effects; credentials never enter retained frames.

use crate::theme::PresentedTheme;
use bmux_keyboard::KeyCode;
use bmux_tui::{
    buffer::Buffer,
    component::{Component, Constraints, LayoutCx},
    composition::{Column, Flex, TextBlock},
    event::Event,
    frame::Frame,
    geometry::{Rect, Size},
    inline::InlineTerminal,
    text::{Line, Span, Text, TextWrap},
};
use std::io;
use zeroize::Zeroizing;

/// Upper bound on hidden pasted input, matching the auth contract's text bound.
const MAX_PASTE_BYTES: usize = 4096;

/// Input observed by the status surface since the previous poll.
#[derive(Debug, PartialEq, Eq)]
pub enum AuthStatusInput {
    /// Nothing actionable happened.
    Idle,
    /// The user asked to cancel sign-in.
    Cancelled,
    /// The user submitted the hidden paste field.
    Submitted(Zeroizing<String>),
}

/// Hidden paste field. The value is never painted; only its length is shown.
#[derive(Default)]
struct PasteField {
    message: String,
    value: Zeroizing<String>,
}

impl PasteField {
    fn apply(&mut self, event: &Event) -> Option<AuthStatusInput> {
        match event {
            Event::Paste(text) => {
                self.extend(text);
                Some(AuthStatusInput::Idle)
            }
            Event::Key(key) if key.modifiers.ctrl || key.modifiers.alt => None,
            Event::Key(key) => match key.key {
                KeyCode::Enter if !self.value.trim().is_empty() => {
                    Some(AuthStatusInput::Submitted(std::mem::take(&mut self.value)))
                }
                KeyCode::Backspace => {
                    self.value.pop();
                    Some(AuthStatusInput::Idle)
                }
                KeyCode::Escape => {
                    self.value.clear();
                    Some(AuthStatusInput::Idle)
                }
                KeyCode::Char(character) => {
                    self.extend(&character.to_string());
                    Some(AuthStatusInput::Idle)
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn extend(&mut self, text: &str) {
        for character in text.chars().filter(|character| !character.is_control()) {
            if self.value.len() + character.len_utf8() > MAX_PASTE_BYTES {
                break;
            }
            self.value.push(character);
        }
    }

    fn status_line(&self) -> String {
        match self.value.chars().count() {
            0 => "Paste: (nothing yet) · Enter submits".to_owned(),
            count => format!("Paste: {count} characters, hidden · Enter submits · Esc clears"),
        }
    }
}

/// A short-lived provider-status surface. Drop restores terminal input before other readers run.
pub struct AuthStatus {
    terminal: InlineTerminal<io::Stderr>,
    theme: PresentedTheme,
    title: String,
    detail: String,
    paste: Option<PasteField>,
}
impl AuthStatus {
    /// Open a main-screen status panel.
    ///
    /// # Errors
    /// Returns terminal mode, configuration-directory, or output errors.
    pub fn enter(config: &bcode_config::TuiConfig) -> io::Result<Self> {
        let theme = crate::theme::resolve_configured_theme(config, &std::env::current_dir()?);
        Ok(Self {
            terminal: InlineTerminal::enter(io::stderr())?,
            theme,
            title: String::new(),
            detail: String::new(),
            paste: None,
        })
    }

    /// Paint a provider-supplied public status and cancellation hint.
    ///
    /// # Errors
    /// Returns terminal dimension or output errors.
    pub fn show(&mut self, title: &str, detail: &str) -> io::Result<()> {
        title.clone_into(&mut self.title);
        detail.clone_into(&mut self.detail);
        self.redraw()
    }

    /// Offer a hidden paste field alongside the status. Repeated offers keep typed input.
    ///
    /// # Errors
    /// Returns terminal dimension or output errors.
    pub fn offer_paste(&mut self, message: &str) -> io::Result<()> {
        let field = self.paste.get_or_insert_with(PasteField::default);
        if field.message == message {
            return Ok(());
        }
        message.clone_into(&mut field.message);
        self.redraw()
    }

    /// Drain pending input without blocking the async provider polling loop.
    ///
    /// # Errors
    /// Returns terminal input or output errors.
    pub fn poll_input(&mut self) -> io::Result<AuthStatusInput> {
        let mut changed = false;
        while let Some(event) = bmux_tui::terminal_io::poll_event(std::time::Duration::ZERO)? {
            if matches!(&event, Event::Key(key) if key.modifiers.ctrl && matches!(key.key, KeyCode::Char('c' | 'd')))
            {
                return Ok(AuthStatusInput::Cancelled);
            }
            match self.paste.as_mut().and_then(|field| field.apply(&event)) {
                Some(AuthStatusInput::Idle) => changed = true,
                Some(submitted) => {
                    self.redraw()?;
                    return Ok(submitted);
                }
                None => {}
            }
        }
        if changed {
            self.redraw()?;
        }
        Ok(AuthStatusInput::Idle)
    }

    fn redraw(&mut self) -> io::Result<()> {
        let size = bmux_tui::terminal_io::terminal_size()?;
        let size = Size::new(
            size.width.saturating_sub(1),
            size.height.saturating_sub(1).min(16),
        );
        let buffer = render(
            &self.title,
            &self.detail,
            self.paste.as_ref(),
            size,
            &self.theme,
        );
        if size.width > 0 && size.height > 0 {
            self.terminal.draw(&buffer)?;
        }
        Ok(())
    }
}

fn render(
    title: &str,
    detail: &str,
    paste: Option<&PasteField>,
    size: Size,
    theme: &PresentedTheme,
) -> Buffer {
    let mut root = Column::new()
        .child(TextBlock::new(Text::from_lines(vec![Line::from_spans(
            vec![Span::styled(
                title,
                bmux_tui::style::Style::new().fg(theme.accent),
            )],
        )])))
        .flex(Flex::new(1, TextBlock::new(detail).wrap(TextWrap::Word)));
    if let Some(field) = paste {
        root = root
            .child(
                TextBlock::new(field.message.as_str())
                    .style(theme.muted)
                    .wrap(TextWrap::Word),
            )
            .child(TextBlock::new(field.status_line()).style(theme.text));
    }
    let root = root.child(TextBlock::new(
        "Ctrl-C Cancel · browser/device flow remains provider-owned",
    ));
    let layout = root.layout(Constraints::tight(size), &mut LayoutCx::new());
    let mut buffer = Buffer::empty(Rect::new(0, 0, size.width, size.height));
    let mut frame = Frame::new(&mut buffer);
    root.paint(&layout, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_keyboard::KeyStroke;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyStroke::simple(code))
    }

    fn painted(buffer: &Buffer) -> String {
        buffer
            .cells()
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect()
    }

    #[test]
    fn paste_field_collects_hidden_input_and_submits_only_nonblank_values() {
        let mut field = PasteField::default();
        assert_eq!(field.apply(&key(KeyCode::Enter)), None);
        field.apply(&Event::Paste(
            "http://localhost/cb?code=s3cret\n".to_owned(),
        ));
        field.apply(&key(KeyCode::Char('x')));
        field.apply(&key(KeyCode::Backspace));
        let Some(AuthStatusInput::Submitted(value)) = field.apply(&key(KeyCode::Enter)) else {
            panic!("expected submission");
        };
        assert_eq!(value.as_str(), "http://localhost/cb?code=s3cret");
        assert!(field.value.is_empty());
        field.apply(&Event::Paste("abc".to_owned()));
        field.apply(&key(KeyCode::Escape));
        assert!(field.value.is_empty());
    }

    #[test]
    fn paste_field_is_bounded_on_character_boundaries() {
        let mut field = PasteField::default();
        field.apply(&Event::Paste("é".repeat(MAX_PASTE_BYTES)));
        assert!(field.value.len() <= MAX_PASTE_BYTES);
        assert!(field.value.len() > MAX_PASTE_BYTES - 2);
    }

    #[test]
    fn rendered_frames_never_show_pasted_value_and_stay_bounded() {
        let resolved = crate::theme::resolve_initial_theme();
        let theme = resolved.presented(resolved.accent);
        let mut field = PasteField {
            message: "Paste the redirected address 中文 👩‍💻 e\u{301}".to_owned(),
            ..PasteField::default()
        };
        field.apply(&Event::Paste("code=s3cret".to_owned()));
        for width in [0, 1, 2, 8, 30, 80] {
            for height in [0, 1, 3, 8, 16] {
                let buffer = render(
                    "OpenAI",
                    "Open the link printed above on any device.",
                    Some(&field),
                    Size::new(width, height),
                    &theme,
                );
                assert_eq!(buffer.area(), Rect::new(0, 0, width, height));
                assert!(!painted(&buffer).contains("s3cret"));
            }
        }
        let text = painted(&render(
            "OpenAI",
            "detail",
            Some(&field),
            Size::new(80, 16),
            &theme,
        ));
        assert!(text.contains("11 characters, hidden"), "{text}");
    }
}
