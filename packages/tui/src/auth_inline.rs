//! Main-screen auth choices using BMUX's existing inline terminal ownership.

use crate::theme::PresentedTheme;
use bmux_keyboard::KeyCode;
use bmux_tui::{
    buffer::Buffer,
    component::{Component, Constraints, LayoutCx, LayoutNode},
    composition::{Column, Flex, TextBlock},
    event::Event,
    frame::Frame,
    geometry::{Rect, Size},
    inline::InlineTerminal,
    text::{Line, Span, Text},
};
use bmux_tui_components::{
    key_hint_bar::{KeyHint, KeyHintBarComponent},
    selectable_list::{
        SelectableListComponent, SelectableListItem, SelectableListState, SelectableListStyles,
    },
};
mod status;
pub use status::AuthStatus;

use std::{
    cell::Cell,
    io::{self, IsTerminal as _, Write as _},
};

/// Result of a terminal choice. Back never authorizes provider side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthChoiceOutcome {
    /// Index into the original immutable choice list, not the filtered rows.
    Selected(usize),
    /// Return to the preceding step.
    Back,
    /// Abort the journey.
    Cancelled,
}

/// Whether the current terminal supports the rich inline auth surface.
#[must_use]
pub fn supported() -> bool {
    io::stdin().is_terminal()
        && io::stderr().is_terminal()
        && std::env::var("TERM").is_ok_and(|term| term != "dumb")
}

/// Select from a bounded auth catalog without entering the alternate screen.
///
/// # Errors
/// Returns terminal, input, or output errors. The BMUX guard restores terminal state.
pub fn choose(
    title: &str,
    labels: &[String],
    config: &bcode_config::TuiConfig,
) -> io::Result<AuthChoiceOutcome> {
    if labels.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no auth choices available",
        ));
    }
    let cwd = std::env::current_dir()?;
    let mut theme = crate::theme::resolve_configured_theme(config, &cwd);
    if std::env::var_os("NO_COLOR").is_some() {
        theme.text = bmux_tui::style::Style::default();
        theme.muted = bmux_tui::style::Style::default();
        theme.selection =
            bmux_tui::style::Style::new().add_modifier(bmux_tui::style::Modifier::REVERSED);
        theme.focused = theme.selection;
        theme.surfaces.raised = bmux_tui::style::Style::default();
        theme.accent = bmux_tui::style::Color::Default;
    }
    let mut terminal = InlineTerminal::enter(io::stderr())?;
    let mut choice = Choice::new(labels);
    let mut previous_size = None;
    loop {
        let size = bmux_tui::crossterm::terminal_size()?;
        if previous_size != Some(size) {
            terminal.clear()?;
            previous_size = Some(size);
        }
        if size.width > 1 && size.height > 1 {
            terminal.draw(&render(
                title,
                labels,
                &choice,
                Size::new(size.width - 1, size.height.saturating_sub(1).min(16)),
                &theme,
            ))?;
        }
        let Some(event) = bmux_tui::crossterm::read_event()? else {
            return Ok(AuthChoiceOutcome::Cancelled);
        };
        if let Some(outcome) = choice.update(&event, labels) {
            terminal.finish()?;
            if let AuthChoiceOutcome::Selected(index) = outcome {
                writeln!(io::stderr(), "{title}: {}", labels[index])?;
            }
            return Ok(outcome);
        }
    }
}

struct Choice {
    filter: String,
    indices: Vec<usize>,
    selected: usize,
}
impl Choice {
    fn new(labels: &[String]) -> Self {
        Self {
            filter: String::new(),
            indices: (0..labels.len()).collect(),
            selected: 0,
        }
    }
    fn update(&mut self, event: &Event, labels: &[String]) -> Option<AuthChoiceOutcome> {
        if let Event::Paste(text) = event {
            self.filter.extend(
                text.chars()
                    .filter(|character| !character.is_control())
                    .take(256usize.saturating_sub(self.filter.chars().count())),
            );
            self.refilter(labels);
            return None;
        }
        let Event::Key(key) = event else {
            return None;
        };
        if key.modifiers.ctrl && matches!(key.key, KeyCode::Char('c' | 'd')) {
            return Some(AuthChoiceOutcome::Cancelled);
        }
        match key.key {
            KeyCode::Escape => return Some(AuthChoiceOutcome::Back),
            KeyCode::Enter => {
                return self
                    .indices
                    .get(self.selected)
                    .copied()
                    .map(AuthChoiceOutcome::Selected);
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.indices.len().saturating_sub(1));
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.indices.len().saturating_sub(1),
            KeyCode::Char(character) if !key.modifiers.ctrl && !key.modifiers.alt => {
                if self.filter.len() < 1024 {
                    self.filter.push(character);
                }
                self.refilter(labels);
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.refilter(labels);
            }
            _ => {}
        }
        None
    }
    fn refilter(&mut self, labels: &[String]) {
        let query = self.filter.to_lowercase();
        self.indices = labels
            .iter()
            .enumerate()
            .filter(|(_, label)| label.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
    }
}

struct RevealedList<'a, 'state>(SelectableListComponent<'a, 'state>);
impl Component for RevealedList<'_, '_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.0.layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut bmux_tui::paint::PaintCx<'_, '_>) {
        self.0.reveal_focus(layout);
        self.0.paint(layout, cx);
    }
}

fn render(
    title: &str,
    labels: &[String],
    choice: &Choice,
    size: Size,
    theme: &PresentedTheme,
) -> Buffer {
    let rows = choice
        .indices
        .iter()
        .map(|&index| SelectableListItem::new(index.to_string(), labels[index].clone()))
        .collect::<Vec<_>>();
    let state = SelectableListState::new((!rows.is_empty()).then_some(choice.selected));
    // BMUX owns scroll/reveal mechanics and all cell measurement.
    let styles = SelectableListStyles {
        normal: theme.text,
        focused: theme.selection,
        selected: theme.selection,
        hovered: theme.focused,
        pressed: theme.selection,
        disabled: theme.muted,
        background: theme.surfaces.raised,
        ..Default::default()
    };
    let state = Cell::new(state);
    let hints = [
        KeyHint::new("↑↓", "Move"),
        KeyHint::new("Enter", "Continue"),
        KeyHint::new("Esc", "Back"),
        KeyHint::new("Ctrl-C", "Cancel"),
    ];
    let list = SelectableListComponent::new("auth.choices", &rows, &state).styles(styles);
    let root = Column::new()
        .child(TextBlock::new(Text::from_lines(vec![Line::from_spans(
            vec![Span::styled(
                title,
                bmux_tui::style::Style::new().fg(theme.accent),
            )],
        )])))
        .child(TextBlock::new(format!("Filter: {}", choice.filter)))
        .flex(Flex::new(1, RevealedList(list)))
        .child(KeyHintBarComponent::new("auth.keys", &hints));
    let layout = root.layout(Constraints::tight(size), &mut LayoutCx::new());
    let mut buffer = Buffer::empty(Rect::new(0, 0, size.width, size.height));
    let mut frame = Frame::new(&mut buffer);
    root.paint(&layout, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_keyboard::{KeyStroke, Modifiers};
    fn key(code: KeyCode) -> Event {
        Event::Key(KeyStroke::simple(code))
    }
    #[test]
    fn filtering_and_navigation_preserve_original_choice_identity() {
        let labels = vec!["first".to_owned(), "second".to_owned()];
        let mut choice = Choice::new(&labels);
        choice.update(&key(KeyCode::Char('s')), &labels);
        assert_eq!(choice.indices, [0, 1]);
        choice.update(&key(KeyCode::Char('e')), &labels);
        assert_eq!(choice.indices, [1]);
        assert_eq!(
            choice.update(&key(KeyCode::Enter), &labels),
            Some(AuthChoiceOutcome::Selected(1))
        );
        choice.update(&key(KeyCode::Char('x')), &labels);
        assert_eq!(choice.update(&key(KeyCode::Enter), &labels), None);
        assert_eq!(
            choice.update(&key(KeyCode::Escape), &labels),
            Some(AuthChoiceOutcome::Back)
        );
        assert_eq!(
            choice.update(
                &Event::Key(KeyStroke::with_modifiers(
                    KeyCode::Char('c'),
                    Modifiers {
                        ctrl: true,
                        ..Modifiers::NONE
                    }
                )),
                &labels
            ),
            Some(AuthChoiceOutcome::Cancelled)
        );
    }
    #[test]
    fn unicode_tiny_and_resized_frames_stay_bounded_and_reveal_selected_row() {
        let labels = (0..50)
            .map(|index| format!("Account {index} 中文 👩‍💻 e\u{301} "))
            .collect::<Vec<_>>();
        let mut choice = Choice::new(&labels);
        choice.selected = 49;
        let resolved = crate::theme::resolve_initial_theme();
        let theme = resolved.presented(resolved.accent);
        for width in [0, 1, 2, 8, 30, 80] {
            for height in [0, 1, 2, 5, 12] {
                let buffer = render(
                    "Authentication",
                    &labels,
                    &choice,
                    Size::new(width, height),
                    &theme,
                );
                assert_eq!(buffer.area(), Rect::new(0, 0, width, height));
                assert_eq!(
                    buffer.cells().len(),
                    usize::from(width) * usize::from(height)
                );
            }
        }
        let buffer = render(
            "Authentication",
            &labels,
            &choice,
            Size::new(80, 12),
            &theme,
        );
        let text = buffer
            .cells()
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        assert!(
            text.contains("Account 49"),
            "selected row not visible: {text}"
        );
    }
}
