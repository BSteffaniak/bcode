//! BMUX backend app state.

use std::time::Instant;

use bcode_session_models::SessionId;
use bmux_text_edit::TextEditBuffer;

use super::IDLE_REDRAW_INTERVAL;

/// State owned by the BMUX-native backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BmuxApp {
    session_id: Option<SessionId>,
    composer: TextEditBuffer,
    pending_submission: Option<String>,
    status: String,
    should_exit: bool,
    cursor_visible: bool,
    last_cursor_toggle: Instant,
}

impl BmuxApp {
    /// Create BMUX backend state.
    #[must_use]
    pub(super) fn new(session_id: Option<SessionId>) -> Self {
        Self {
            session_id,
            composer: TextEditBuffer::new(),
            pending_submission: None,
            status: String::from("BMUX backend connected. Enter submits; Esc/Ctrl-C exits."),
            should_exit: false,
            cursor_visible: true,
            last_cursor_toggle: Instant::now(),
        }
    }

    /// Return the active session id, if one was provided.
    #[must_use]
    pub(super) const fn session_id(&self) -> Option<SessionId> {
        self.session_id
    }

    /// Return the composer buffer.
    #[must_use]
    pub(super) const fn composer(&self) -> &TextEditBuffer {
        &self.composer
    }

    /// Return the composer buffer mutably.
    pub(super) const fn composer_mut(&mut self) -> &mut TextEditBuffer {
        &mut self.composer
    }

    /// Return the current status line.
    #[must_use]
    pub(super) fn status(&self) -> &str {
        &self.status
    }

    /// Replace the current status line.
    pub(super) fn set_status(&mut self, status: String) {
        self.status = status;
    }

    /// Store the current composer text as a pending submission and clear input.
    pub(super) fn stage_submission(&mut self) {
        self.pending_submission = Some(self.composer.text().to_owned());
        self.composer.clear();
    }

    /// Return the currently pending submission.
    pub(super) fn take_pending_submission(&mut self) -> String {
        self.pending_submission.take().unwrap_or_default()
    }

    /// Restore pending submission text into the composer after send failure.
    pub(super) fn restore_pending_submission(&mut self) {
        if let Some(text) = self.pending_submission.take() {
            self.composer.insert_str(&text);
        }
        self.wake_cursor();
    }

    /// Return whether the composer cursor should be visible.
    #[must_use]
    pub(super) const fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Reset cursor blink state after input.
    pub(super) fn wake_cursor(&mut self) {
        self.cursor_visible = true;
        self.last_cursor_toggle = Instant::now();
    }

    /// Advance time-based UI state.
    pub(super) fn tick(&mut self) -> bool {
        if self.last_cursor_toggle.elapsed() < IDLE_REDRAW_INTERVAL {
            return false;
        }
        self.cursor_visible = !self.cursor_visible;
        self.last_cursor_toggle = Instant::now();
        true
    }

    /// Return whether the backend should exit.
    #[must_use]
    pub(super) const fn should_exit(&self) -> bool {
        self.should_exit
    }

    /// Request backend shutdown.
    pub(super) const fn request_exit(&mut self) {
        self.should_exit = true;
    }
}
