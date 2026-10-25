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
    text::{Line, Span, Text},
};
use std::io;

/// A short-lived provider-status surface. Drop restores terminal input before other readers run.
pub struct AuthStatus {
    terminal: InlineTerminal<io::Stderr>,
    theme: PresentedTheme,
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
        })
    }
    /// Paint a provider-supplied public status and cancellation hint.
    ///
    /// # Errors
    /// Returns terminal dimension or output errors.
    pub fn show(&mut self, title: &str, detail: &str) -> io::Result<()> {
        let size = bmux_tui::crossterm::terminal_size()?;
        let size = Size::new(
            size.width.saturating_sub(1),
            size.height.saturating_sub(1).min(10),
        );
        let root = Column::new()
            .child(TextBlock::new(Text::from_lines(vec![Line::from_spans(
                vec![Span::styled(
                    title,
                    bmux_tui::style::Style::new().fg(self.theme.accent),
                )],
            )])))
            .flex(Flex::new(1, TextBlock::new(detail)))
            .child(TextBlock::new(
                "Ctrl-C Cancel · browser/device flow remains provider-owned",
            ));
        let layout = root.layout(Constraints::tight(size), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, size.width, size.height));
        let mut frame = Frame::new(&mut buffer);
        root.paint(&layout, &mut bmux_tui::paint::PaintCx::new(&mut frame));
        if size.width > 0 && size.height > 0 {
            self.terminal.draw(&buffer)?;
        }
        Ok(())
    }
    /// Observe cancellation without blocking the async provider polling loop.
    ///
    /// # Errors
    /// Returns terminal input errors.
    pub fn cancelled(&mut self) -> io::Result<bool> {
        Ok(
            matches!(bmux_tui::crossterm::poll_event(std::time::Duration::ZERO)?,
            Some(Event::Key(key)) if key.modifiers.ctrl && matches!(key.key, KeyCode::Char('c' | 'd'))),
        )
    }
}
