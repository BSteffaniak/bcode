//! Terminal adaptation of caller-owned startup choices.

use std::io::{self, Write};

use bmux_keyboard::KeyCode;
use bmux_tui::damage::Damage;
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Style};
use bmux_tui::terminal::Terminal;
use bmux_tui_components::selectable_list::{
    SelectableList, SelectableListItem, SelectableListState, SelectableListStyles,
};
use bmux_tui_runtime::{
    Invalidation, Lifecycle, PresentReport, Presenter, Program, RuntimeEvent, Update,
};

use crate::{TuiError, setup_connection_form::ConnectionForm, theme::PresentedTheme};

/// A display-only startup candidate. Its index refers to the caller's selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickConnectionChoice {
    /// User-facing account/model label, without credentials.
    pub label: String,
    /// Explanation supplied by the discovery owner.
    pub detail: String,
}

/// Caller-owned candidates and targeted recovery guidance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuickOnboardingInput {
    /// Choices in caller-defined order; the UI does not rank or discover them.
    pub choices: Vec<QuickConnectionChoice>,
    /// Optional safe recovery text.
    pub recovery: Option<String>,
}

/// Explicit startup action, returned after restoring the terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuickOnboardingOutcome {
    /// Use the candidate at this index.
    Selected(usize),
    /// Enrollment completed successfully; the caller resolves the model.
    Connected {
        /// Authentication provider reference.
        provider_id: String,
        /// Enrolled authentication profile reference.
        profile: String,
    },
    /// Open the existing advanced setup board.
    Advanced,
    /// Close without selecting or enrolling.
    Close,
}

struct QuickProgram {
    input: QuickOnboardingInput,
    selected: usize,
    list: SelectableListState,
    area: Rect,
    outcome: QuickOnboardingOutcome,
    form: Option<ConnectionForm>,
    theme: PresentedTheme,
}

enum QuickMessage {
    Progress,
    InputFailed(io::Error),
}

impl QuickProgram {
    const fn new(input: QuickOnboardingInput, theme: &PresentedTheme) -> Self {
        Self {
            input,
            selected: 0,
            list: SelectableListState::new(Some(0)),
            area: Rect::new(0, 0, 0, 0),
            outcome: QuickOnboardingOutcome::Close,
            form: None,
            theme: *theme,
        }
    }

    fn progress() -> bmux_tui_runtime::Command<QuickMessage> {
        bmux_tui_runtime::Command::concurrent(async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Some(QuickMessage::Progress)
        })
    }

    fn key(&mut self, key: KeyCode) -> Lifecycle {
        match key {
            KeyCode::Escape | KeyCode::Char('q') => return Lifecycle::Exit,
            KeyCode::F(2) => {
                self.outcome = QuickOnboardingOutcome::Advanced;
                return Lifecycle::Exit;
            }
            KeyCode::Char('c') => self.form = Some(ConnectionForm::quick()),
            KeyCode::Up | KeyCode::Down => {
                let items = self.items();
                let area = self.list_area();
                let _ = SelectableList::new(&items).handle_event(
                    area,
                    &mut self.list,
                    &Event::Key(bmux_keyboard::KeyStroke::simple(key)),
                );
                self.selected = self.list.focused().unwrap_or(0);
                self.list.set_selected(Some(self.selected));
            }
            KeyCode::Enter if self.input.choices.is_empty() => {
                self.form = Some(ConnectionForm::quick());
            }
            KeyCode::Enter => {
                self.outcome = QuickOnboardingOutcome::Selected(self.selected);
                return Lifecycle::Exit;
            }
            _ => {}
        }
        Lifecycle::Continue
    }

    fn items(&self) -> Vec<SelectableListItem> {
        self.input
            .choices
            .iter()
            .enumerate()
            .map(|(index, choice)| SelectableListItem::new(index.to_string(), choice.label.clone()))
            .collect()
    }

    fn handle_connection_event(&mut self, event: &Event) -> Lifecycle {
        if !self
            .form
            .as_mut()
            .is_some_and(|form| form.handle_event(event))
        {
            return Lifecycle::Continue;
        }
        if let Some((provider_id, profile)) =
            self.form.as_mut().and_then(ConnectionForm::completion)
        {
            self.outcome = QuickOnboardingOutcome::Connected {
                provider_id,
                profile,
            };
            Lifecycle::Exit
        } else {
            self.form = None;
            Lifecycle::Continue
        }
    }

    const fn list_area(&self) -> Rect {
        Rect::new(0, 3, self.area.width, self.area.height.saturating_sub(8))
    }

    fn render(&mut self, frame: &mut PaintCx<'_, '_>) {
        // SelectableList's area is interpreted relative to the supplied PaintCx.
        // Retain the local extent, not the terminal-space clip/origin.
        let local = frame.area();
        self.area = Rect::new(0, 0, local.width, local.height);
        if let Some(form) = &mut self.form {
            form.render(frame, &self.theme);
            return;
        }
        // Explicit overflow policy: each line is clipped by BMUX's bounded viewport;
        // an indivisible wide grapheme is omitted rather than spilling or being split.
        let line = |frame: &mut PaintCx<'_, '_>, row: u16, text: &str, style: Style| {
            if row < frame.area().height {
                frame.write_line_with_fallback_style(
                    LocalRect::new(0, i64::from(row), frame.area().width, 1),
                    &Line::from(text),
                    style,
                );
            }
        };
        line(frame, 0, "Welcome to Bcode", self.theme.focused);
        line(
            frame,
            1,
            self.input
                .recovery
                .as_deref()
                .unwrap_or("Choose an account to start coding."),
            self.theme.text,
        );
        if self.input.choices.is_empty() {
            line(frame, 3, "No usable account is available.", self.theme.text);
            line(frame, 4, "Enter: Sign in / Add key", self.theme.focused);
        } else {
            let items = self.items();
            SelectableList::new(&items)
                .styles(SelectableListStyles {
                    normal: self.theme.text,
                    focused: self.theme.focused,
                    selected: self.theme.focused,
                    background: self.theme.canvas,
                    ..SelectableListStyles::default()
                })
                .paint(self.list_area(), &self.list, self.theme.text, frame);
            let height = self.area.height;
            line(
                frame,
                height.saturating_sub(4),
                &self.input.choices[self.selected].detail,
                self.theme.muted,
            );
            line(
                frame,
                height.saturating_sub(3),
                "Enter: Use this and start",
                self.theme.focused,
            );
        }
        let height = self.area.height;
        line(
            frame,
            height.saturating_sub(2),
            "c: Connect another (Sign in / Add key) • F2: Advanced",
            self.theme.text,
        );
        line(
            frame,
            height.saturating_sub(1),
            "↑/↓: choose • Esc: Close",
            self.theme.muted,
        );
    }
}

impl Program for QuickProgram {
    type Message = QuickMessage;
    type Error = TuiError;

    fn update(
        &mut self,
        event: RuntimeEvent<Self::Message>,
    ) -> Result<Update<Self::Message>, Self::Error> {
        let poll =
            matches!(event, RuntimeEvent::Message(QuickMessage::Progress)) || self.form.is_none();
        let mut lifecycle = Lifecycle::Continue;
        match event {
            RuntimeEvent::Message(QuickMessage::InputFailed(error)) => return Err(error.into()),
            RuntimeEvent::Terminal(Event::Key(key)) if key.key == KeyCode::F(2) => {
                self.outcome = QuickOnboardingOutcome::Advanced;
                lifecycle = Lifecycle::Exit;
            }
            RuntimeEvent::Terminal(ref event) if self.form.is_some() => {
                lifecycle = self.handle_connection_event(event);
            }
            RuntimeEvent::Terminal(Event::Key(key)) => lifecycle = self.key(key.key),
            _ => {}
        }
        if let Some((provider_id, profile)) =
            self.form.as_mut().and_then(ConnectionForm::completion)
        {
            self.outcome = QuickOnboardingOutcome::Connected {
                provider_id,
                profile,
            };
            lifecycle = Lifecycle::Exit;
        }
        Ok(Update {
            lifecycle,
            invalidation: Invalidation::Redraw,
            commands: if poll && self.form.is_some() && lifecycle == Lifecycle::Continue {
                vec![Self::progress()]
            } else {
                Vec::new()
            },
            ..Update::none()
        })
    }
}

struct QuickPresenter<'a, 'b, W> {
    terminal: &'a mut Terminal<&'b mut W>,
}

impl<W: Write> Presenter<QuickProgram> for QuickPresenter<'_, '_, W> {
    type Error = io::Error;
    fn resize(&mut self, size: bmux_tui::geometry::Size) {
        self.terminal
            .resize(Rect::new(0, 0, size.width, size.height));
    }
    fn reset(&mut self, _reason: bmux_tui_runtime::ResetReason) {
        self.terminal.reset();
    }
    fn present(&mut self, program: &mut QuickProgram) -> Result<PresentReport, Self::Error> {
        let stats = self
            .terminal
            .draw_damage(Damage::Full, |frame| program.render(frame))?;
        Ok(PresentReport {
            changed_cells: stats.changed_cells,
            full_repaint: stats.full_repaint,
        })
    }
}

/// Present a quick startup choice or enroll through the existing secure provider lifecycle.
/// No candidate discovery, model ranking, or startup-state persistence occurs here.
///
/// # Errors
/// Returns configuration or terminal errors, after attempting to restore the terminal.
pub async fn run_quick_onboarding(
    input: QuickOnboardingInput,
) -> Result<QuickOnboardingOutcome, TuiError> {
    let config = bcode_config::load_config()?;
    let theme = crate::theme::resolve_configured_theme(&config.tui, std::path::Path::new("."));
    let mut guard = crate::CrosstermTerminalGuard::enter(io::stdout())?;
    let result = {
        let mut terminal = Terminal::new(
            guard
                .writer_mut()
                .ok_or_else(|| io::Error::other("terminal writer unavailable"))?,
            crate::terminal_area()?,
        );
        let (runtime, handle) = bmux_tui_runtime::Runtime::new(
            QuickProgram::new(input, &theme),
            QuickPresenter {
                terminal: &mut terminal,
            },
            bmux_tui_runtime::RuntimeConfig::default(),
        );
        let input = bmux_tui_runtime::TerminalInput::start::<QuickProgram>(
            handle,
            QuickMessage::InputFailed,
        );
        let result = Box::pin(runtime.run()).await;
        input.request_shutdown();
        match result {
            Ok(output) => Ok(output.program.outcome),
            Err(bmux_tui_runtime::RuntimeError::Program { error, .. }) => Err(error),
            Err(bmux_tui_runtime::RuntimeError::Presenter { error, .. }) => Err(error.into()),
        }
    };
    let _writer = guard.leave()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn program() -> QuickProgram {
        QuickProgram::new(
            QuickOnboardingInput {
                choices: vec![QuickConnectionChoice {
                    label: "账户 🦀 e\u{301}".to_owned(),
                    detail: "Ready".to_owned(),
                }],
                recovery: None,
            },
            &crate::theme::resolve_configured_theme(
                &bcode_config::TuiConfig::default(),
                std::path::Path::new("."),
            ),
        )
    }
    #[test]
    fn one_confirmation_selects_without_enrollment() {
        let mut program = program();
        assert_eq!(program.key(KeyCode::Enter), Lifecycle::Exit);
        assert_eq!(program.outcome, QuickOnboardingOutcome::Selected(0));
        assert!(program.form.is_none());
    }
    #[test]
    fn close_and_advanced_do_not_enroll() {
        let mut program = program();
        assert_eq!(program.key(KeyCode::Escape), Lifecycle::Exit);
        assert_eq!(program.outcome, QuickOnboardingOutcome::Close);
        assert!(program.form.is_none());
        assert_eq!(program.key(KeyCode::F(2)), Lifecycle::Exit);
        assert_eq!(program.outcome, QuickOnboardingOutcome::Advanced);
    }
    #[test]
    fn unicode_and_recovery_render_inside_zero_and_tiny_frames() {
        for (width, height) in [(0, 0), (1, 1), (2, 4), (18, 12), (80, 24)] {
            let mut program = program();
            program.input.recovery = Some("重新连接 e\u{301} 🦀".to_owned());
            let area = Rect::new(0, 0, width, height);
            let mut buffer = bmux_tui::buffer::Buffer::empty(area);
            program.render(&mut bmux_tui::paint::PaintCx::new(
                &mut bmux_tui::frame::Frame::new(&mut buffer),
            ));
            if width == 80 {
                let rows = (0..height)
                    .map(|row| buffer.row_symbols(row).unwrap())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(rows.contains("账户"));
                assert!(rows.contains("重新连接"));
                assert!(rows.contains("Esc: Close"));
            }
        }
    }

    #[test]
    fn wide_graphemes_are_omitted_in_one_cell_viewport_without_spilling() {
        let mut program = program();
        program.input.recovery = Some("界🦀e\u{301}".to_owned());
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 4, 24));
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
        PaintCx::new(&mut frame)
            .with_child(0, 0, LocalRect::new(0, 0, 1, 24), |cx| program.render(cx));
        let recovery = buffer.row_symbols(1).unwrap();
        assert!(!recovery.contains('界'));
        assert!(!recovery.contains('🦀'));
        assert!(recovery.chars().skip(1).all(|character| character == ' '));
    }

    #[test]
    fn compact_chooser_shows_multiple_accounts_and_primary_actions() {
        let mut program = program();
        program.input.choices.push(QuickConnectionChoice {
            label: "Second account".to_owned(),
            detail: "Ready too".to_owned(),
        });
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        program.render(&mut PaintCx::new(&mut bmux_tui::frame::Frame::new(
            &mut buffer,
        )));
        let rows = (0..24)
            .map(|row| buffer.row_symbols(row).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rows.contains("账户"));
        assert!(rows.contains("Second account"));
        assert!(rows.contains("Use this and start"));
        assert!(rows.contains("Connect another"));
        assert!(rows.contains("Advanced"));
        program.key(KeyCode::Down);
        assert_eq!(program.key(KeyCode::Enter), Lifecycle::Exit);
        assert_eq!(program.outcome, QuickOnboardingOutcome::Selected(1));
    }

    #[test]
    fn no_account_visibly_offers_sign_in_without_implicitly_enrolling() {
        let mut program = program();
        program.input.choices.clear();
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 80, 24));
        program.render(&mut PaintCx::new(&mut bmux_tui::frame::Frame::new(
            &mut buffer,
        )));
        let rows = (0..24)
            .map(|row| buffer.row_symbols(row).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rows.contains("Sign in / Add key"));
        assert!(program.form.is_none());
    }

    #[test]
    fn progress_never_implicitly_enrolls() {
        let mut program = program();
        let update = program
            .update(RuntimeEvent::Message(QuickMessage::Progress))
            .unwrap();
        assert!(program.form.is_none());
        assert!(update.commands.is_empty());
        assert_eq!(program.outcome, QuickOnboardingOutcome::Close);
    }

    #[test]
    fn navigation_is_bounded() {
        let mut program = program();
        for _ in 0..10 {
            program.key(KeyCode::Down);
        }
        assert_eq!(program.selected, 0);
        program.key(KeyCode::Up);
        assert_eq!(program.selected, 0);
    }
}
