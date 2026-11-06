//! Pre-session adapter for the normal model picker. Discovery and admission stay application-owned.

use std::io::{self, Write};

use bcode_model::execution::{ExecutionModel, ExecutionModelSelection};
use bmux_tui::{damage::Damage, event::Event, paint::PaintCx, terminal::Terminal};
use bmux_tui_runtime::{Invalidation, Lifecycle, PresentReport, Program, RuntimeEvent, Update};

use crate::{TuiError, model_flow::ModelPickerAction, model_picker::ModelPickerApp};

/// Explicit navigation from the startup model inventory.
#[derive(Debug)]
pub enum ExecutionPickerOutcome {
    /// Exact model, backend and account intent; not execution authority.
    Selected(ExecutionModelSelection),
    /// Open existing connection/setup controls.
    Setup,
    /// Close without creating a session or changing defaults.
    Close,
}

struct ExecutionPicker {
    picker: ModelPickerApp,
    theme: crate::render::TuiTheme,
    keymap: crate::keymap::BmuxKeyMap,
    outcome: ExecutionPickerOutcome,
    presented_selection: Option<ExecutionModelSelection>,
}

enum Message {
    InputFailed(io::Error),
}

impl Program for ExecutionPicker {
    type Message = Message;
    type Error = TuiError;

    fn update(&mut self, event: RuntimeEvent<Message>) -> Result<Update<Message>, TuiError> {
        let mut lifecycle = Lifecycle::Continue;
        match event {
            RuntimeEvent::Message(Message::InputFailed(error)) => return Err(error.into()),
            RuntimeEvent::Terminal(Event::Key(key)) => {
                if self.keymap.action("quick_onboarding", key) == Some("tui.quick.advanced") {
                    self.outcome = ExecutionPickerOutcome::Setup;
                    lifecycle = Lifecycle::Exit;
                } else {
                    match crate::model_flow::handle_model_picker_key(
                        &mut self.picker,
                        &self.keymap,
                        None,
                        key,
                    ) {
                        ModelPickerAction::Select(_) => {
                            if let Some(selection) = self.picker.selected_execution()
                                && self.presented_selection.as_ref() == Some(&selection)
                            {
                                self.outcome = ExecutionPickerOutcome::Selected(selection);
                                lifecycle = Lifecycle::Exit;
                            }
                        }
                        ModelPickerAction::Cancel => lifecycle = Lifecycle::Exit,
                        ModelPickerAction::Check(_) | ModelPickerAction::Continue => {}
                    }
                }
            }
            RuntimeEvent::Terminal(Event::Mouse(mouse)) => {
                if self.picker.select_mouse_row(mouse).is_some()
                    && let Some(selection) = self.picker.selected_execution()
                    && self.presented_selection.as_ref() == Some(&selection)
                {
                    self.outcome = ExecutionPickerOutcome::Selected(selection);
                    lifecycle = Lifecycle::Exit;
                }
            }
            _ => {}
        }
        Ok(Update {
            invalidation: Invalidation::Redraw,
            lifecycle,
            ..Update::none()
        })
    }
}

fn present<W: Write>(
    program: &mut ExecutionPicker,
    terminal: &mut Terminal<W>,
) -> Result<PresentReport, io::Error> {
    let stats = terminal.draw_damage(Damage::Full, |frame: &mut PaintCx<'_, '_>| {
        crate::model_picker_render::render_model_picker_with_keymap(
            &mut program.picker,
            frame,
            program.theme,
            &program.keymap,
        );
    })?;
    program.presented_selection = program
        .picker
        .has_visible_rows()
        .then(|| program.picker.selected_execution())
        .flatten();
    Ok(PresentReport {
        changed_cells: stats.changed_cells,
        full_repaint: stats.full_repaint,
    })
}

/// Select a model using the ordinary picker without constructing a provider client.
/// The highlighted preference remains only an intent and is never automatically launched.
/// # Errors
/// Returns configuration, terminal, or input lifecycle errors without retrying an operation.
pub async fn run_execution_picker_in_shell(
    shell: &mut crate::TerminalShell,
    models: Vec<ExecutionModel>,
    remembered: Option<&ExecutionModelSelection>,
    status: &str,
) -> Result<ExecutionPickerOutcome, TuiError> {
    let config = bcode_config::load_config()?;
    let theme = crate::theme::resolve_configured_theme(&config.tui, std::path::Path::new("."));
    let keymap = crate::keymap::BmuxKeyMap::from_config(&config.tui);
    let mut picker = ModelPickerApp::from_execution_models(
        models,
        format!(
            "{status} · {} setup/sign in · Selecting does not verify access",
            keymap.hint("quick_onboarding", "tui.quick.advanced"),
        ),
    );
    if let Some(selection) = remembered {
        picker.highlight_execution(selection);
        if !picker.execution_preference_present(selection) {
            picker.set_status("Remembered route/model/account is not in this inventory. No replacement was selected automatically; choose explicitly or open Setup.");
        }
    }
    let program = ExecutionPicker {
        picker,
        theme: crate::render::TuiTheme::from_presented(&theme),
        keymap,
        outcome: ExecutionPickerOutcome::Close,
        presented_selection: None,
    };
    let mut terminal = shell.terminal()?;
    let (runtime, handle) = bmux_tui_runtime::Runtime::new(
        program,
        bmux_tui_runtime::terminal_presenter::BorrowedTerminalPresenter::new(
            &mut terminal,
            present,
        ),
        bmux_tui_runtime::RuntimeConfig::default(),
    );
    let mut input =
        bmux_tui_runtime::TerminalInput::start::<ExecutionPicker>(handle, Message::InputFailed);
    let result = Box::pin(runtime.run()).await;
    input.shutdown().await;
    let result = match result {
        Ok(output) => Ok(output.program.outcome),
        Err(bmux_tui_runtime::RuntimeError::Program {
            error,
            cleanup_error,
            ..
        }) => Err(crate::runtime_error_with_cleanup(error, cleanup_error)),
        Err(bmux_tui_runtime::RuntimeError::Presenter {
            error,
            cleanup_error,
            ..
        }) => Err(crate::runtime_error_with_cleanup(error, cleanup_error)),
    };
    if result.is_err() {
        shell.block_handoff();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::geometry::Rect;

    #[test]
    fn empty_execution_picker_contains_tiny_and_resized_frames() {
        let config = bcode_config::BcodeConfig::default();
        let theme = crate::theme::resolve_configured_theme(&config.tui, std::path::Path::new("."));
        let mut program = ExecutionPicker {
            picker: ModelPickerApp::from_execution_models(Vec::new(), "No models · setup/sign in"),
            theme: crate::render::TuiTheme::from_presented(&theme),
            keymap: crate::keymap::BmuxKeyMap::from_config(&config.tui),
            outcome: ExecutionPickerOutcome::Close,
            presented_selection: None,
        };
        for (width, height) in [(0, 0), (1, 1), (2, 3), (12, 5), (80, 24), (1, 1)] {
            let mut output = Vec::new();
            let mut terminal = Terminal::new(&mut output, Rect::new(0, 0, width, height));
            present(&mut program, &mut terminal).unwrap();
            assert!(program.picker.selected_execution().is_none());
        }
    }
}
