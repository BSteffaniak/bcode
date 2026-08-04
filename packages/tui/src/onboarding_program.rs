//! Runtime-owned first-run onboarding screen.

use std::io::Write;

use bmux_keyboard::KeyCode;
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;
use bmux_tui_runtime::{
    Invalidation, Lifecycle, PresentReport, Presenter, Program, RuntimeEvent, Update,
};

use super::{TuiError, current_time_ms, onboarding, onboarding_render};

/// Runtime-owned onboarding messages.
pub enum OnboardingMessage {
    /// Terminal input backend failure.
    InputFailed(std::io::Error),
}

/// Serialized onboarding state owned by the BMUX runtime.
pub struct OnboardingProgram {
    store: bcode_settings::SettingsStore,
    shell: onboarding::OnboardingShell,
    health: bcode_settings::SettingsDbHealth,
    readiness: Option<bcode_settings::SetupReadinessReport>,
    theme: super::theme::PresentedTheme,
    area: Rect,
}

impl OnboardingProgram {
    /// Create onboarding state for the current terminal area.
    pub fn new(
        store: bcode_settings::SettingsStore,
        shell: onboarding::OnboardingShell,
        theme: &super::theme::PresentedTheme,
        area: Rect,
    ) -> Result<Self, TuiError> {
        let health = store.health();
        let readiness = store.readiness_report()?;
        Ok(Self {
            store,
            shell,
            health,
            readiness,
            theme: *theme,
            area,
        })
    }

    fn refresh_persisted_state(&mut self) -> Result<(), TuiError> {
        self.health = self.store.health();
        self.readiness = self.store.readiness_report()?;
        Ok(())
    }

    fn handle_key(&mut self, code: KeyCode) -> Result<Lifecycle, TuiError> {
        match code {
            KeyCode::Escape | KeyCode::Char('q') => {
                self.shell.handle_action(
                    onboarding::OnboardingInputAction::CancelConfirmation,
                    &self.store,
                    current_time_ms(),
                )?;
                Ok(Lifecycle::Abort)
            }
            KeyCode::Right | KeyCode::Down | KeyCode::Char('j') => {
                self.shell.focus_next();
                Ok(Lifecycle::Continue)
            }
            KeyCode::Left | KeyCode::Up | KeyCode::Char('k') => {
                self.shell.focus_previous();
                Ok(Lifecycle::Continue)
            }
            _ => {
                if let Some(action) = onboarding_action_for_key(code) {
                    self.shell
                        .handle_action(action, &self.store, current_time_ms())?;
                }
                Ok(Lifecycle::Continue)
            }
        }
    }
}

impl Program for OnboardingProgram {
    type Message = OnboardingMessage;
    type Error = TuiError;

    fn update(
        &mut self,
        event: RuntimeEvent<Self::Message>,
    ) -> Result<Update<Self::Message>, Self::Error> {
        let mut lifecycle = Lifecycle::Continue;
        let invalidation = match event {
            RuntimeEvent::Terminal(Event::Resize(size)) => {
                self.area = Rect::new(0, 0, size.width, size.height);
                Invalidation::Redraw
            }
            RuntimeEvent::Terminal(Event::Key(key)) => {
                lifecycle = self.handle_key(key.key)?;
                Invalidation::Redraw
            }
            RuntimeEvent::Terminal(event @ Event::Mouse(_)) => {
                let board_area = onboarding_render::onboarding_board_area(self.area);
                let _outcome = self.shell.handle_board_event(board_area, &event);
                Invalidation::Redraw
            }
            RuntimeEvent::Terminal(
                Event::Paste(_) | Event::Focus(_) | Event::Tick | Event::User(_),
            )
            | RuntimeEvent::Timer(_) => Invalidation::None,
            RuntimeEvent::Message(OnboardingMessage::InputFailed(error)) => {
                return Err(error.into());
            }
        };
        self.refresh_persisted_state()?;
        Ok(Update {
            invalidation,
            lifecycle,
            ..Update::none()
        })
    }
}

const fn onboarding_action_for_key(code: KeyCode) -> Option<onboarding::OnboardingInputAction> {
    match code {
        KeyCode::Enter => Some(onboarding::OnboardingInputAction::Select),
        KeyCode::Char('p') => Some(onboarding::OnboardingInputAction::ToggleProvider),
        KeyCode::Char('a') => Some(onboarding::OnboardingInputAction::ToggleAuthProfile),
        KeyCode::Char('m') => Some(onboarding::OnboardingInputAction::SelectModelProfile),
        KeyCode::Char('r') => Some(onboarding::OnboardingInputAction::CyclePermissionPreset),
        KeyCode::Char('i') => Some(onboarding::OnboardingInputAction::ReviewSessionImport),
        KeyCode::Char('g') => Some(onboarding::OnboardingInputAction::ReviewPlugins),
        KeyCode::Char('x') => Some(onboarding::OnboardingInputAction::ApplyPlan),
        KeyCode::Char('y') => Some(onboarding::OnboardingInputAction::Confirm),
        KeyCode::Char('n') => Some(onboarding::OnboardingInputAction::CancelConfirmation),
        KeyCode::Char('c') => Some(onboarding::OnboardingInputAction::Complete),
        KeyCode::Char('s') => Some(onboarding::OnboardingInputAction::Skip),
        KeyCode::Char('l') => Some(onboarding::OnboardingInputAction::Launch),
        _ => None,
    }
}

/// Onboarding presenter at the terminal-specific boundary.
pub struct OnboardingPresenter<'a, 'b, W> {
    terminal: &'a mut Terminal<&'b mut W>,
}

impl<'a, 'b, W> OnboardingPresenter<'a, 'b, W> {
    /// Create a presenter around the caller-owned terminal.
    pub const fn new(terminal: &'a mut Terminal<&'b mut W>) -> Self {
        Self { terminal }
    }
}

impl<W: Write> Presenter<OnboardingProgram> for OnboardingPresenter<'_, '_, W> {
    type Error = std::io::Error;

    fn resize(&mut self, size: bmux_tui::geometry::Size) {
        self.terminal
            .resize(Rect::new(0, 0, size.width, size.height));
    }

    fn reset(&mut self, _reason: bmux_tui_runtime::ResetReason) {
        self.terminal.reset();
    }

    fn present(&mut self, program: &mut OnboardingProgram) -> Result<PresentReport, Self::Error> {
        let stats = self.terminal.draw(|frame| {
            onboarding_render::render_onboarding(
                &program.shell,
                frame,
                &program.health,
                program.readiness.clone(),
                &program.theme,
            );
        })?;
        Ok(PresentReport {
            changed_cells: stats.changed_cells,
            full_repaint: stats.full_repaint,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::onboarding_action_for_key;
    use bmux_keyboard::KeyCode;

    #[test]
    fn onboarding_runtime_maps_product_actions_without_terminal_types() {
        assert!(onboarding_action_for_key(KeyCode::Enter).is_some());
        assert!(onboarding_action_for_key(KeyCode::Char('p')).is_some());
        assert!(onboarding_action_for_key(KeyCode::Char('l')).is_some());
        assert_eq!(onboarding_action_for_key(KeyCode::F(1)), None);
    }
}
