//! Explicit terminal ownership across pre-session and session screens.

use std::future::Future;
use std::io::{self, Stdout};

use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::crossterm::CrosstermTerminalGuard;
use bmux_tui::damage::Damage;
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::LocalRect;
use bmux_tui::prelude::{Style, TextBlock, TextWrap};
use bmux_tui::terminal::Terminal;

use crate::{TuiError, TuiLaunchOptions};

/// Caller-owned recovery choice after a normalized error is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalRecoveryAction {
    /// Retry only when the domain operation can safely be repeated.
    Retry,
    /// Return to the preceding screen.
    Back,
    /// Close the interactive journey.
    Close,
}

/// One alternate-screen/raw-input lifetime, without requiring a session.
///
/// Screens borrow its writer sequentially and await input shutdown before returning.
/// Dropping the shell restores the terminal, including on errors.
pub struct TerminalShell {
    guard: CrosstermTerminalGuard<Stdout>,
    stop_requested: bool,
}

impl TerminalShell {
    /// Enter the terminal once for an interactive journey.
    ///
    /// # Errors
    /// Returns terminal setup errors.
    pub fn enter() -> Result<Self, TuiError> {
        Ok(Self {
            guard: CrosstermTerminalGuard::enter(io::stdout())?,
            stop_requested: false,
        })
    }

    /// Borrow a fresh screen presenter without leaving the alternate screen.
    ///
    /// # Errors
    /// Returns terminal dimension or writer errors.
    pub fn terminal(&mut self) -> Result<Terminal<&mut Stdout>, TuiError> {
        let area = crate::terminal_area()?;
        let writer = self
            .guard
            .writer_mut()
            .ok_or_else(|| io::Error::other("terminal shell writer unavailable"))?;
        Ok(Terminal::new(writer, area))
    }

    /// Whether the user requested exit during the most recent initialization wait.
    ///
    /// Callers retain/reconcile its actual result before honoring this request.
    #[must_use]
    pub const fn stop_requested(&self) -> bool {
        self.stop_requested
    }

    /// Run advanced setup without creating a session or restoring the terminal.
    ///
    /// # Errors
    /// Returns setup, configuration, or terminal errors.
    pub async fn run_setup(
        &mut self,
        allow_discovery: bool,
    ) -> Result<bcode_settings::SetupContinuation, TuiError> {
        crate::run_setup_in_shell(self, allow_discovery).await
    }

    /// Run chat in this terminal lifetime.
    ///
    /// # Errors
    /// Returns client, configuration, or terminal errors.
    #[allow(clippy::future_not_send)]
    pub async fn run_chat(
        &mut self,
        session_id: Option<bcode_session_models::SessionId>,
        plugins: &[bcode_plugin::StaticBundledPlugin],
        options: TuiLaunchOptions,
    ) -> Result<(), TuiError> {
        let Some(connection) = self
            .wait(
                "Connecting to the session…",
                crate::runtime::prepare_connection(session_id),
            )
            .await?
        else {
            return Ok(());
        };
        if self.stop_requested() {
            return Ok(());
        }
        let Some(presentation) = self
            .wait(
                "Loading chat interface…",
                crate::runtime::prepare_presentation(plugins),
            )
            .await?
        else {
            return Ok(());
        };
        if self.stop_requested() {
            return Ok(());
        }
        Box::pin(crate::runtime::run_prepared_chat_with_presentation(
            &mut self.terminal()?,
            connection,
            presentation,
            plugins,
            options,
        ))
        .await
    }

    /// Dispatch a supported plugin startup surface while borrowing this terminal owner.
    ///
    /// Review preparation uses progress/draining; other adapters retain their existing loading
    /// behavior but never enter a second terminal guard. Repository paths must be resolved by CLI.
    ///
    /// # Errors
    /// Returns unresolved path/options, unsupported surface, launcher, or terminal errors.
    #[allow(clippy::future_not_send)]
    pub async fn run_plugin_surface(
        &mut self,
        surface_kind: String,
        repo_path: Option<std::path::PathBuf>,
        options: std::collections::BTreeMap<String, String>,
    ) -> Result<(), TuiError> {
        let repo = crate::resolved_surface_repo_path(&surface_kind, repo_path)?;
        match surface_kind.as_str() {
            "code-review" => {
                if let Some(target) = options.get("target") {
                    self.run_review(repo, serde_json::from_str(target)?).await
                } else {
                    Box::pin(self.run_review_home(repo)).await
                }
            }
            "ralph-home" => {
                Box::pin(crate::runtime::run_event_loop_with_startup(
                    &mut self.terminal()?,
                    None,
                    crate::startup_action::StartupTuiAction::OpenRalphHome { repo_path: repo },
                ))
                .await
            }
            "eval-run-picker" => {
                Box::pin(crate::eval_launcher::run_picker(
                    &mut self.terminal()?,
                    repo,
                ))
                .await
            }
            "eval-run-viewer" => {
                Box::pin(crate::eval_launcher::run_viewer(
                    &mut self.terminal()?,
                    repo,
                    options.get("run").map(std::path::PathBuf::from),
                ))
                .await
            }
            "metrics-dashboard" => {
                Box::pin(crate::metrics_launcher::run_dashboard(
                    &mut self.terminal()?,
                    repo,
                    options.get("metrics_path").map(std::path::PathBuf::from),
                ))
                .await
            }
            _ => Err(TuiError::PluginService {
                code: "unsupported_startup_surface".to_owned(),
                message: format!(
                    "plugin surface `{surface_kind}` cannot be opened as a startup surface"
                ),
            }),
        }
    }

    /// Open review home, then its selected workspace and chat without terminal teardown.
    ///
    /// # Errors
    /// Returns review plugin, client, or terminal errors.
    #[allow(clippy::future_not_send)]
    pub async fn run_review_home(&mut self, repo_path: std::path::PathBuf) -> Result<(), TuiError> {
        self.stop_requested = false;
        loop {
            let result = {
                let prepared = match self
                    .wait(
                        "Opening review home…",
                        crate::code_review_launcher::prepare_home(repo_path.clone()),
                    )
                    .await
                {
                    Ok(Some(prepared)) if !self.stop_requested() => prepared,
                    Ok(_) => return Ok(()),
                    Err(_) => {
                        self.review_error().await?;
                        return Ok(());
                    }
                };
                Box::pin(crate::code_review_launcher::run_prepared_home(
                    &mut self.terminal()?,
                    prepared,
                ))
                .await
            };
            match result {
                Ok(crate::code_review_launcher::ReviewHomeOutcome::OpenWorkspace {
                    workspace,
                    build_mode,
                }) => {
                    Box::pin(self.run_review_workspace(workspace, build_mode)).await?;
                    if self.stop_requested() {
                        return Ok(());
                    }
                }
                Ok(crate::code_review_launcher::ReviewHomeOutcome::Exit) => return Ok(()),
                Err(_) => {
                    self.review_error().await?;
                    return Ok(());
                }
            }
        }
    }

    /// Open a review workspace and its requested chat without terminal teardown.
    ///
    /// # Errors
    /// Returns terminal or recovery presentation errors. Review failures are acknowledged in-app.
    #[allow(clippy::future_not_send)]
    pub async fn run_review_workspace(
        &mut self,
        workspace: bcode_code_review_models::ReviewWorkspace,
        build_mode: bool,
    ) -> Result<(), TuiError> {
        self.stop_requested = false;
        let result = {
            let prepared = match self
                .wait(
                    "Opening review workspace…",
                    crate::code_review_launcher::prepare_workspace(workspace, build_mode),
                )
                .await
            {
                Ok(Some(prepared)) if !self.stop_requested() => prepared,
                Ok(_) => return Ok(()),
                Err(_) => return self.review_error().await,
            };
            Box::pin(crate::code_review_launcher::run_prepared_review(
                &mut self.terminal()?,
                prepared,
            ))
            .await
        };
        match result {
            Ok(session_id) => self.run_review_chat(session_id).await,
            Err(_) => self.review_error().await,
        }
    }

    /// Open a review target and its requested chat without terminal teardown.
    ///
    /// # Errors
    /// Returns terminal or recovery presentation errors. Review failures are acknowledged in-app.
    #[allow(clippy::future_not_send)]
    pub async fn run_review(
        &mut self,
        repo_path: std::path::PathBuf,
        target: bcode_code_review_models::ReviewTarget,
    ) -> Result<(), TuiError> {
        self.stop_requested = false;
        let result = {
            let prepared = match self
                .wait(
                    "Opening review target…",
                    crate::code_review_launcher::prepare_review(repo_path, target),
                )
                .await
            {
                Ok(Some(prepared)) if !self.stop_requested() => prepared,
                Ok(_) => return Ok(()),
                Err(_) => return self.review_error().await,
            };
            Box::pin(crate::code_review_launcher::run_prepared_review(
                &mut self.terminal()?,
                prepared,
            ))
            .await
        };
        match result {
            Ok(session_id) => self.run_review_chat(session_id).await,
            Err(_) => self.review_error().await,
        }
    }

    async fn review_error(&mut self) -> Result<(), TuiError> {
        if self.stop_requested() {
            // Preparation has settled; honor the already requested exit without admitting
            // a second screen or repeating an operation whose effects may have committed.
            return Ok(());
        }
        // Do not expose plugin-private errors or offer to repeat an operation whose effects
        // may already have committed. Returning home performs navigation, not mutation retry.
        let action = self.show_error_without_retry(
            "Review could not be opened or completed. Its operations may already have taken effect. Return to inspect the current state, or close.",
        ).await?;
        self.stop_requested = action == TerminalRecoveryAction::Close;
        Ok(())
    }

    #[allow(clippy::future_not_send)]
    async fn run_review_chat(
        &mut self,
        session_id: Option<bcode_session_models::SessionId>,
    ) -> Result<(), TuiError> {
        if let Some(session_id) = session_id {
            if self
                .run_chat(
                    Some(session_id),
                    &crate::static_bundled_plugins(),
                    TuiLaunchOptions::default(),
                )
                .await
                .is_err()
            {
                self.review_error().await?;
            } else {
                self.stop_requested = true;
            }
        } else {
            // A successful workspace close ends the journey, rather than reopening home.
            self.stop_requested = true;
        }
        Ok(())
    }

    /// Display caller-owned initialization progress while awaiting an operation.
    ///
    /// Escape or Ctrl-C requests a safe stop: progress changes while the operation drains.
    /// The actual result is returned (including a created session identity); this method never
    /// drops mutating work or claims committed effects were cancelled. `None` is reserved for
    /// verified cancellation and is not currently returned. Check `stop_requested` after retaining
    /// the operation's result. The future must itself finish or reconcile ambiguous effects.
    ///
    /// # Errors
    /// Returns operation errors. Input/presentation failures request a stop while the operation
    /// drains; on success the actual result remains available to the caller for reconciliation.
    pub async fn wait<T>(
        &mut self,
        label: &str,
        future: impl Future<Output = Result<T, TuiError>>,
    ) -> Result<Option<T>, TuiError> {
        self.stop_requested = false;
        let mut terminal = self.terminal()?;
        let mut input = bmux_tui_runtime::ManagedTerminalInput::start(32);
        let (result, stop) = wait_operation(&mut terminal, &mut input, label, future).await;
        self.stop_requested = stop;
        result.map(Some)
    }

    /// Show a caller-normalized, secret-safe error until an explicit recovery choice.
    ///
    /// This method does not classify errors or decide whether retry is safe.
    ///
    /// # Errors
    /// Returns input or presentation errors.
    pub async fn show_error(
        &mut self,
        safe_message: &str,
    ) -> Result<TerminalRecoveryAction, TuiError> {
        self.show_recovery(safe_message, true).await
    }

    /// Show an error for which retry is unsafe, offering only back or close.
    ///
    /// # Errors
    /// Returns input or presentation errors.
    pub async fn show_error_without_retry(
        &mut self,
        safe_message: &str,
    ) -> Result<TerminalRecoveryAction, TuiError> {
        self.show_recovery(safe_message, false).await
    }

    async fn show_recovery(
        &mut self,
        safe_message: &str,
        allow_retry: bool,
    ) -> Result<TerminalRecoveryAction, TuiError> {
        let mut terminal = self.terminal()?;
        let instructions = if allow_retry {
            "Enter / R: retry   Esc / B: back   Ctrl-C / Q: close"
        } else {
            "Esc / B: back   Ctrl-C / Q: close"
        };
        let text = format!("{safe_message}\n\n{instructions}");
        paint_text(&mut terminal, &text)?;
        let mut input = bmux_tui_runtime::ManagedTerminalInput::start(32);
        read_recovery(&mut terminal, &mut input, &text, allow_retry).await
    }

    /// Restore the terminal after the entire journey.
    ///
    /// # Errors
    /// Returns terminal restoration errors.
    pub fn finish(self) -> Result<(), TuiError> {
        self.guard.leave()?;
        Ok(())
    }
}

// The production and offline-test paths share the exact input/drain loop. No terminal guard
// is created here: all frames belong to the caller's existing terminal lifetime.
async fn wait_operation<T, W: std::io::Write>(
    terminal: &mut Terminal<&mut W>,
    input: &mut bmux_tui_runtime::ManagedTerminalInput,
    label: &str,
    future: impl Future<Output = Result<T, TuiError>>,
) -> (Result<T, TuiError>, bool) {
    let initial_paint_failed = paint_text(terminal, label).is_err();
    let mut future = std::pin::pin!(future);
    let mut ticks = tokio::time::interval(std::time::Duration::from_millis(100));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut phase = 0_usize;
    let mut stop = initial_paint_failed;
    let mut input_open = true;
    let mut paint = !initial_paint_failed;
    let result = loop {
        tokio::select! {
            result = &mut future => {
                // Preserve exit requests already queued at settlement. Work stays bounded
                // even if input continuously arrives; the operation is never starved.
                for _ in 0..32 {
                    tokio::select! {
                        biased;
                        event = input.recv(), if input_open => match event {
                            Some(Ok(Some(Event::Key(key)))) if wants_stop(key.key, key.modifiers.ctrl) => stop = true,
                            Some(Err(_)) | None => { stop = true; input_open = false; }
                            _ => {}
                        },
                        () = std::future::ready(()) => break,
                    }
                }
                break result;
            },
            event = input.recv(), if input_open => match event {
                Some(Ok(Some(Event::Key(key)))) if wants_stop(key.key, key.modifiers.ctrl) => stop = true,
                Some(Ok(Some(Event::Resize(size)))) => terminal.resize(Rect::new(0, 0, size.width, size.height)),
                Some(Err(_)) | None => { stop = true; input_open = false; }
                _ => {}
            },
            _ = ticks.tick(), if paint => {
                let status = if stop { "Finishing initialization safely before closing…" } else { label };
                let text = format!("{} {status}\n\nEsc / Ctrl-C to request exit", ["⠋", "⠙", "⠹", "⠸"][phase % 4]);
                phase = phase.wrapping_add(1);
                if paint_text(terminal, &text).is_err() { stop = true; paint = false; }
            }
        }
    };
    input.shutdown().await;
    (result, stop)
}

async fn read_recovery<W: std::io::Write>(
    terminal: &mut Terminal<&mut W>,
    input: &mut bmux_tui_runtime::ManagedTerminalInput,
    text: &str,
    allow_retry: bool,
) -> Result<TerminalRecoveryAction, TuiError> {
    let result = async {
        loop {
            match input.recv().await {
                Some(Ok(Some(Event::Key(key)))) => {
                    if let Some(action) = recovery_action(key.key, key.modifiers.ctrl)
                        .filter(|action| allow_retry || *action != TerminalRecoveryAction::Retry)
                    {
                        return Ok(action);
                    }
                }
                Some(Ok(Some(Event::Resize(size)))) => {
                    terminal.resize(Rect::new(0, 0, size.width, size.height));
                    paint_text(terminal, text)?;
                }
                Some(Err(error)) => return Err(error.into()),
                None => {
                    return Err(io::Error::other("terminal input closed during recovery").into());
                }
                _ => {}
            }
        }
    }
    .await;
    input.shutdown().await;
    result
}

fn wants_stop(key: bmux_keyboard::KeyCode, ctrl: bool) -> bool {
    key == bmux_keyboard::KeyCode::Escape || (key == bmux_keyboard::KeyCode::Char('c') && ctrl)
}

const fn recovery_action(
    key: bmux_keyboard::KeyCode,
    ctrl: bool,
) -> Option<TerminalRecoveryAction> {
    use bmux_keyboard::KeyCode;
    match key {
        KeyCode::Char('c') if ctrl => Some(TerminalRecoveryAction::Close),
        KeyCode::Char('q') => Some(TerminalRecoveryAction::Close),
        KeyCode::Escape | KeyCode::Char('b') => Some(TerminalRecoveryAction::Back),
        KeyCode::Enter | KeyCode::Char('r') => Some(TerminalRecoveryAction::Retry),
        _ => None,
    }
}

fn paint_text<W: std::io::Write>(
    terminal: &mut Terminal<&mut W>,
    text: &str,
) -> Result<(), io::Error> {
    terminal.draw_damage(Damage::Full, |frame| {
        let block = TextBlock::new(text).wrap(TextWrap::Word);
        let layout = block.layout(
            Constraints::new(0, u64::from(frame.area().width), 0, None),
            &mut LayoutCx::new(),
        );
        for (index, row) in block
            .projection(&layout)
            .iter()
            .take(usize::from(frame.area().height))
            .enumerate()
        {
            frame.write_line_with_fallback_style(
                LocalRect::new(
                    0,
                    i64::try_from(index).unwrap_or(i64::MAX),
                    frame.area().width,
                    1,
                ),
                &row.line,
                Style::new(),
            );
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_keyboard::{KeyCode, KeyStroke};

    struct ProgressWriter {
        bytes: Vec<u8>,
        safe_stop: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl std::io::Write for ProgressWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            if self
                .bytes
                .windows(b"Finishing initialization safely".len())
                .any(|window| window == b"Finishing initialization safely")
                && let Some(sender) = self.safe_stop.take()
            {
                let _ = sender.send(());
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn delayed_loader_drains_once_then_back_or_close_without_retry() {
        use bmux_keyboard::Modifiers;
        use std::cell::Cell;
        for close in [false, true] {
            let (safe_stop, observed_stop) = tokio::sync::oneshot::channel();
            let mut writer = ProgressWriter {
                bytes: Vec::new(),
                safe_stop: Some(safe_stop),
            };
            let mut terminal = Terminal::new(&mut writer, Rect::new(0, 0, 72, 8));
            let attempts = Cell::new(0);
            let committed = Cell::new(false);
            let mut input = bmux_tui_runtime::ManagedTerminalInput::from_events([Ok(Some(
                Event::Key(KeyStroke::simple(KeyCode::Escape)),
            ))]);
            let (result, stopped) =
                wait_operation(&mut terminal, &mut input, "Opening review…", async {
                    attempts.set(attempts.get() + 1);
                    // Do not commit until the same foreground terminal has presented the
                    // stop request. This proves input/progress remain live during preparation.
                    tokio::time::timeout(std::time::Duration::from_secs(2), observed_stop)
                        .await
                        .expect("responsive safe-stop progress")
                        .expect("progress writer alive");
                    committed.set(true);
                    Err::<(), _>(std::io::Error::other("offline loader failed after commit").into())
                })
                .await;
            assert!(result.is_err());
            assert!(stopped);
            assert!(committed.get());
            let safe_error = "Review unavailable; operations may have taken effect.";
            paint_text(&mut terminal, safe_error).unwrap();
            let choice = if close {
                KeyStroke {
                    key: KeyCode::Char('c'),
                    modifiers: Modifiers {
                        ctrl: true,
                        ..Modifiers::NONE
                    },
                }
            } else {
                KeyStroke::simple(KeyCode::Escape)
            };
            let mut recovery = bmux_tui_runtime::ManagedTerminalInput::from_events([
                // Retry keys must not replay the already-committed operation.
                Ok(Some(Event::Key(KeyStroke::simple(KeyCode::Enter)))),
                Ok(Some(Event::Key(KeyStroke::simple(KeyCode::Char('r'))))),
                Ok(Some(Event::Key(choice))),
            ]);
            let action = read_recovery(&mut terminal, &mut recovery, safe_error, false)
                .await
                .unwrap();
            assert_eq!(
                action,
                if close {
                    TerminalRecoveryAction::Close
                } else {
                    TerminalRecoveryAction::Back
                }
            );
            assert_eq!(attempts.get(), 1);
            paint_text(&mut terminal, "Returned without reopening terminal").unwrap();
            // Inspect emitted terminal behavior, not implementation/source names.
            assert!(
                !writer
                    .bytes
                    .windows(b"\x1b[?1049l".len())
                    .any(|window| window == b"\x1b[?1049l")
            );
        }
    }

    #[tokio::test]
    async fn ready_loader_retains_already_queued_close_request_and_result() {
        let mut bytes = Vec::new();
        let mut terminal = Terminal::new(&mut bytes, Rect::new(0, 0, 40, 4));
        let mut input = bmux_tui_runtime::ManagedTerminalInput::from_events([Ok(Some(
            Event::Key(KeyStroke::simple(KeyCode::Escape)),
        ))]);
        let (result, stopped) =
            wait_operation(&mut terminal, &mut input, "Loading…", async { Ok(17) }).await;
        assert_eq!(result.unwrap(), 17);
        assert!(stopped);
    }

    #[test]
    fn exit_requests_are_explicit() {
        assert!(wants_stop(KeyCode::Escape, false));
        assert!(wants_stop(KeyCode::Char('c'), true));
        assert!(!wants_stop(KeyCode::Char('c'), false));
        assert!(!wants_stop(KeyCode::Enter, false));
    }

    #[test]
    fn recovery_maps_retry_back_and_close_separately() {
        assert_eq!(
            recovery_action(KeyCode::Enter, false),
            Some(TerminalRecoveryAction::Retry)
        );
        assert_eq!(
            recovery_action(KeyCode::Escape, false),
            Some(TerminalRecoveryAction::Back)
        );
        assert_eq!(
            recovery_action(KeyCode::Char('c'), true),
            Some(TerminalRecoveryAction::Close)
        );
        assert_eq!(recovery_action(KeyCode::Char('c'), false), None);
    }
}
