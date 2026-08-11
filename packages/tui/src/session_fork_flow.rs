//! Session fork/clone flow for the TUI.

use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionHistoryCursor, SessionHistoryDirection,
    SessionHistoryQuery,
};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::Modifier;
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing};

use super::TuiError;

/// State for selecting the user prompt that bounds a session fork.
#[derive(Debug, Clone)]
pub struct ForkPromptPicker {
    prompts: Vec<ForkPromptCandidate>,
    selected: usize,
}

/// Outcome from one fork-prompt picker key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkPromptPickerOutcome {
    /// Selection changed and the picker remains open.
    Handled,
    /// Activate the selected prompt.
    Select(ForkPromptCandidate),
    /// Close without selecting a prompt.
    Canceled,
    /// The key is not owned by this picker.
    Ignored,
}

impl ForkPromptPicker {
    /// Create picker state from available prompts.
    #[must_use]
    pub const fn new(prompts: Vec<ForkPromptCandidate>) -> Self {
        Self {
            prompts,
            selected: 0,
        }
    }

    /// Return available prompt rows.
    #[must_use]
    pub fn prompts(&self) -> &[ForkPromptCandidate] {
        &self.prompts
    }

    /// Return the selected row.
    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    /// Handle one picker key.
    pub fn handle_key(&mut self, stroke: bmux_keyboard::KeyStroke) -> ForkPromptPickerOutcome {
        match stroke.key {
            bmux_keyboard::KeyCode::Escape => ForkPromptPickerOutcome::Canceled,
            bmux_keyboard::KeyCode::Enter => self.prompts.get(self.selected).cloned().map_or(
                ForkPromptPickerOutcome::Ignored,
                ForkPromptPickerOutcome::Select,
            ),
            bmux_keyboard::KeyCode::Up if self.selected > 0 => {
                self.selected = self.selected.saturating_sub(1);
                ForkPromptPickerOutcome::Handled
            }
            bmux_keyboard::KeyCode::Down
                if self.selected.saturating_add(1) < self.prompts.len() =>
            {
                self.selected = self.selected.saturating_add(1);
                ForkPromptPickerOutcome::Handled
            }
            bmux_keyboard::KeyCode::Up | bmux_keyboard::KeyCode::Down => {
                ForkPromptPickerOutcome::Handled
            }
            _ => ForkPromptPickerOutcome::Ignored,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkPromptCandidate {
    pub sequence: u64,
    pub text: String,
}

pub async fn load_recent_user_prompts(
    client: &bcode_client::BcodeClient,
    session_id: bcode_session_models::SessionId,
) -> Result<Vec<ForkPromptCandidate>, TuiError> {
    let page = match client
        .session_history_page(
            session_id,
            SessionHistoryQuery {
                cursor: Some(SessionHistoryCursor { sequence: u64::MAX }),
                limit: 256,
                direction: SessionHistoryDirection::Backward,
            },
        )
        .await
    {
        Ok(page) => page,
        Err(error) => return Err(error.into()),
    };
    Ok(page
        .events
        .iter()
        .filter_map(user_prompt_candidate_from_event)
        .collect())
}

fn user_prompt_candidate_from_event(event: &SessionEvent) -> Option<ForkPromptCandidate> {
    let SessionEventKind::UserMessage { text, .. } = &event.kind else {
        return None;
    };
    Some(ForkPromptCandidate {
        sequence: event.sequence,
        text: text.clone(),
    })
}

pub fn render_prompt_picker(
    frame: &mut Frame<'_>,
    prompts: &[ForkPromptCandidate],
    selected: usize,
    theme: super::render::TuiTheme,
) {
    let modal = prompt_picker_modal(theme);
    modal.render(frame.area(), frame);
    let content = modal.content_area(frame.area());
    let mut row = content.y;
    render_picker_line(
        frame,
        &modal,
        content,
        &mut row,
        &Line::from_spans(vec![Span::styled(
            "Choose the prompt to edit in the forked session",
            theme.muted,
        )]),
    );
    for (index, prompt) in prompts.iter().take(10).enumerate() {
        render_picker_prompt_line(
            frame,
            &modal,
            content,
            &mut row,
            prompt,
            index == selected,
            theme,
        );
    }
    render_picker_help(frame, &modal, content, &mut row, theme);
}

fn prompt_picker_modal(theme: super::render::TuiTheme) -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(Size::new(72, 12), Size::new(96, 18), Insets::all(4)),
        theme.modal_theme(),
    )
    .title(" Select fork prompt ")
    .padding(Insets::new(1, 2, 1, 2))
    .placement(ModalPlacement::UpperThird)
}

fn render_picker_prompt_line(
    frame: &mut Frame<'_>,
    modal: &ModalFrame,
    content: Rect,
    row: &mut u16,
    prompt: &ForkPromptCandidate,
    selected: bool,
    theme: super::render::TuiTheme,
) {
    let selected_style = if selected {
        theme.selection.add_modifier(Modifier::BOLD)
    } else {
        theme.text
    };
    render_picker_line(
        frame,
        modal,
        content,
        row,
        &Line::from_spans(vec![
            Span::styled(format!("#{:<4} ", prompt.sequence), selected_style),
            Span::styled(one_line(&prompt.text), selected_style),
        ]),
    );
}

fn render_picker_help(
    frame: &mut Frame<'_>,
    modal: &ModalFrame,
    content: Rect,
    row: &mut u16,
    theme: super::render::TuiTheme,
) {
    render_picker_line(
        frame,
        modal,
        content,
        row,
        &Line::from_spans(vec![
            Span::styled("Enter", theme.text.add_modifier(Modifier::BOLD)),
            Span::styled(" select  ", theme.text),
            Span::styled("↑/↓", theme.text.add_modifier(Modifier::BOLD)),
            Span::styled(" move  ", theme.text),
            Span::styled("Esc", theme.text.add_modifier(Modifier::BOLD)),
            Span::styled(" cancel", theme.text),
        ]),
    );
}

fn render_picker_line(
    frame: &mut Frame<'_>,
    modal: &ModalFrame,
    content: Rect,
    row: &mut u16,
    line: &Line,
) {
    if *row >= content.bottom() {
        return;
    }
    modal.render_line(Rect::new(content.x, *row, content.width, 1), line, frame);
    *row = row.saturating_add(1);
}

fn one_line(text: &str) -> String {
    const MAX_CHARS: usize = 80;
    let mut output = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if output.chars().count() > MAX_CHARS {
        output = output.chars().take(MAX_CHARS).collect::<String>();
        output.push('…');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{ForkPromptCandidate, ForkPromptPicker, ForkPromptPickerOutcome};
    use bmux_keyboard::{KeyCode, KeyStroke};

    #[test]
    fn picker_state_owns_navigation_selection_and_cancel() {
        let mut picker = ForkPromptPicker::new(vec![
            ForkPromptCandidate {
                sequence: 1,
                text: "first".to_owned(),
            },
            ForkPromptCandidate {
                sequence: 2,
                text: "second".to_owned(),
            },
        ]);

        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Down)),
            ForkPromptPickerOutcome::Handled
        );
        assert_eq!(picker.selected(), 1);
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Enter)),
            ForkPromptPickerOutcome::Select(ForkPromptCandidate {
                sequence: 2,
                text: "second".to_owned(),
            })
        );
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Escape)),
            ForkPromptPickerOutcome::Canceled
        );
    }
}
