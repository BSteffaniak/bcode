//! The model switcher on its own screen, for choices made outside a conversation: at startup
//! and when a choice needs a new conversation. Discovery and admission stay application-owned.

use std::io::{self, Write};

use bmux_tui::{damage::Damage, event::Event, paint::PaintCx, terminal::Terminal};
use bmux_tui_runtime::{
    Command, Invalidation, PresentReport, Program, RuntimeEvent, Subscription, SubscriptionKey,
    Update,
};

use crate::TuiError;
use crate::model_switcher::inventory::{CurrentChoice, SourceUpdate};
use crate::model_switcher::{Choice, ModelSwitcher, Notice, Outcome};

/// How the user left the switcher.
#[derive(Debug)]
pub enum SwitcherExit {
    /// Use this model. An intent, not execution authority.
    Chose(Box<Choice>),
    /// Open native tool setup.
    Setup,
    /// Leave without choosing or changing anything.
    Close,
}

const DISCOVERY: &str = "model-switcher.discovery";
const ANIMATION: &str = "model-switcher.animation";

struct SwitcherScreen {
    switcher: ModelSwitcher,
    theme: crate::model_switcher::SwitcherTheme,
    exit: SwitcherExit,
    /// Discovery starts once, after the first frame is on screen.
    started: bool,
}

enum Message {
    Source(Box<SourceUpdate>),
    Discovered,
    Approved(Result<(), String>),
    Tick,
    InputFailed(io::Error),
}

fn discovery() -> Subscription<Message> {
    Subscription::new(SubscriptionKey::new(DISCOVERY), |sender| async move {
        let (updates, mut received) = tokio::sync::mpsc::channel(16);
        let discover = tokio::spawn(crate::model_switcher::discovery::discover(updates));
        while let Some(update) = received.recv().await {
            if sender
                .send(Message::Source(Box::new(update)))
                .await
                .is_err()
            {
                discover.abort();
                return;
            }
        }
        let _ = sender.send(Message::Discovered).await;
    })
}

fn animation() -> Subscription<Message> {
    Subscription::new(SubscriptionKey::new(ANIMATION), |sender| async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            if sender.send(Message::Tick).await.is_err() {
                return;
            }
        }
    })
}

impl SwitcherScreen {
    fn act(&mut self, outcome: Outcome) -> Update<Message> {
        match outcome {
            Outcome::Ignored => Update::none(),
            Outcome::Redraw => Update::redraw(),
            Outcome::Close => Update::exit(),
            Outcome::Setup => {
                self.exit = SwitcherExit::Setup;
                Update::exit()
            }
            Outcome::Choose(choice) => {
                let choices = self.switcher.record_choice(&choice.entry.key);
                save_choices(choices);
                self.exit = SwitcherExit::Chose(choice);
                Update::exit()
            }
            Outcome::Connect(candidate) => {
                self.switcher.notify(Notice {
                    text: format!("Connecting {}…", candidate.application),
                    error: false,
                });
                Update::redraw().with_command(Command::concurrent(async move {
                    use bcode_provider_auth_models::external::ExternalAuthApplication as _;
                    let result = bcode_client::BcodeClient::default_endpoint()
                        .approve_external_auth(*candidate)
                        .await
                        .map(|_| ())
                        .map_err(|error| error.to_string());
                    Some(Message::Approved(result))
                }))
            }
            Outcome::SaveChoices(choices) => {
                save_choices(choices);
                Update::redraw()
            }
        }
    }
}

/// Persist recent and favorite choices off the input path; failure only loses the change.
fn save_choices(choices: bcode_config::model_choices::ModelChoices) {
    tokio::task::spawn_blocking(move || {
        let _ = bcode_config::model_choices::update_at(
            &bcode_config::model_choices::default_path(),
            |stored| *stored = choices,
        );
    });
}

impl Program for SwitcherScreen {
    type Message = Message;
    type Error = TuiError;

    fn presentation_committed(&mut self, _report: PresentReport) -> Update<Message> {
        if std::mem::replace(&mut self.started, true) {
            return Update::none();
        }
        Update::none()
            .with_subscription(discovery())
            .with_subscription(animation())
    }

    fn update(&mut self, event: RuntimeEvent<Message>) -> Result<Update<Message>, TuiError> {
        Ok(match event {
            RuntimeEvent::Message(Message::Source(update)) => {
                self.switcher.apply(*update);
                Update::redraw()
            }
            RuntimeEvent::Message(Message::Discovered) => Update::redraw()
                .cancel_subscription(SubscriptionKey::new(DISCOVERY))
                .cancel_subscription(SubscriptionKey::new(ANIMATION)),
            RuntimeEvent::Message(Message::Approved(result)) => {
                self.switcher.notify(match result {
                    Ok(()) => Notice {
                        text: "Connected. Its models are listed below.".into(),
                        error: false,
                    },
                    Err(_) => Notice {
                        text: "Could not connect those credentials. Nothing was changed.".into(),
                        error: true,
                    },
                });
                // Rediscover so the connection's models replace the offer.
                Update::redraw()
                    .with_subscription(discovery())
                    .with_subscription(animation())
            }
            RuntimeEvent::Message(Message::Tick) => {
                if self.switcher.loading() {
                    Update::redraw()
                } else {
                    Update::none()
                }
            }
            RuntimeEvent::Message(Message::InputFailed(error)) => return Err(error.into()),
            RuntimeEvent::Terminal(Event::Key(key)) => {
                let outcome = self.switcher.key(key);
                self.act(outcome)
            }
            RuntimeEvent::Terminal(Event::Paste(text)) => {
                let outcome = self.switcher.paste(&text);
                self.act(outcome)
            }
            RuntimeEvent::Terminal(event @ Event::Mouse(_)) => {
                let outcome = self.switcher.mouse(&event, &self.theme);
                self.act(outcome)
            }
            RuntimeEvent::Terminal(Event::Resize(_)) => Update {
                invalidation: Invalidation::Redraw,
                ..Update::none()
            },
            _ => Update::none(),
        })
    }
}

fn present<W: Write>(
    program: &mut SwitcherScreen,
    terminal: &mut Terminal<W>,
) -> Result<PresentReport, io::Error> {
    let stats = terminal.draw_damage(Damage::Full, |cx: &mut PaintCx<'_, '_>| {
        let local = cx.area();
        let area = bmux_tui::geometry::Rect::new(0, 0, local.width, local.height);
        program.switcher.paint(&program.theme, area, cx);
    })?;
    Ok(PresentReport {
        changed_cells: stats.changed_cells,
        full_repaint: stats.full_repaint,
    })
}

/// Choose a model on its own screen.
///
/// The switcher opens at once; sources fill in as they are checked, and Setup and close stay
/// available meanwhile. Nothing is launched or changed here except approving credentials the
/// user chose to connect and saving recent and favorite choices.
/// # Errors
/// Returns configuration, terminal, or input lifecycle errors.
pub async fn run_model_switcher_in_shell(
    shell: &mut crate::TerminalShell,
    current: CurrentChoice,
    current_effort: Option<String>,
) -> Result<SwitcherExit, TuiError> {
    let config = bcode_config::load_config()?;
    let theme = crate::theme::resolve_configured_theme(&config.tui, std::path::Path::new("."));
    crate::render::set_chrome_theme(&theme);
    let choices =
        bcode_config::model_choices::load_from(&bcode_config::model_choices::default_path())
            .unwrap_or_default();
    let keymap = crate::keymap::BmuxKeyMap::from_config(&config.tui);
    let mut switcher = ModelSwitcher::new(current, choices, keymap);
    switcher.set_current_effort(current_effort);
    let program = SwitcherScreen {
        switcher,
        theme: crate::model_switcher::SwitcherTheme::from_presented(&theme),
        exit: SwitcherExit::Close,
        started: false,
    };
    run(shell, program).await
}

async fn run(
    shell: &mut crate::TerminalShell,
    program: SwitcherScreen,
) -> Result<SwitcherExit, TuiError> {
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
        bmux_tui_runtime::TerminalInput::start::<SwitcherScreen>(handle, Message::InputFailed);
    let result = Box::pin(runtime.run()).await;
    input.shutdown().await;
    let result = match result {
        Ok(output) => Ok(output.program.exit),
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
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::geometry::Rect;
    use bmux_tui_runtime::Lifecycle;

    fn program() -> SwitcherScreen {
        SwitcherScreen {
            switcher: ModelSwitcher::new(
                CurrentChoice::default(),
                bcode_config::model_choices::ModelChoices::default(),
                crate::keymap::BmuxKeyMap::from_config(&bcode_config::BcodeConfig::default().tui),
            ),
            theme: crate::model_switcher::SwitcherTheme::from_presented(
                &crate::render::semantic_state_theme(),
            ),
            exit: SwitcherExit::Close,
            started: false,
        }
    }

    fn report() -> PresentReport {
        PresentReport {
            changed_cells: 0,
            full_repaint: false,
        }
    }

    #[test]
    fn discovery_starts_once_after_the_first_frame_and_never_blocks_close_or_setup() {
        for (key, setup) in [(KeyCode::Escape, false), (KeyCode::F(2), true)] {
            let mut program = program();
            assert_eq!(
                program.presentation_committed(report()).subscriptions.len(),
                2
            );
            assert!(
                program
                    .presentation_committed(report())
                    .subscriptions
                    .is_empty()
            );
            let update = program
                .update(RuntimeEvent::Terminal(Event::Key(KeyStroke::simple(key))))
                .unwrap();
            assert_eq!(update.lifecycle, Lifecycle::Exit);
            assert_eq!(matches!(program.exit, SwitcherExit::Setup), setup);
        }
    }

    #[test]
    fn failed_connections_are_reported_inline_and_rediscover() {
        let mut program = program();
        let update = program
            .update(RuntimeEvent::Message(Message::Approved(Err(
                "private provider detail".into(),
            ))))
            .unwrap();
        assert_eq!(update.lifecycle, Lifecycle::Continue);
        assert_eq!(update.subscriptions.len(), 2);
        assert!(matches!(program.exit, SwitcherExit::Close));
    }

    #[test]
    fn empty_switcher_contains_tiny_and_resized_frames() {
        let mut program = program();
        for (width, height) in [(0, 0), (1, 1), (2, 3), (12, 5), (80, 24), (1, 1)] {
            let mut output = Vec::new();
            let mut terminal = Terminal::new(&mut output, Rect::new(0, 0, width, height));
            present(&mut program, &mut terminal).unwrap();
            assert!(program.switcher.active_entry().is_none());
        }
    }
}
