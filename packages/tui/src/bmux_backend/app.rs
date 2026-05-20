//! BMUX backend app state.

use bcode_session_models::SessionId;
use bmux_text_edit::TextEditBuffer;

/// State owned by the BMUX-native backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BmuxApp {
    session_id: Option<SessionId>,
    composer: TextEditBuffer,
    should_exit: bool,
}

impl BmuxApp {
    /// Create BMUX backend state.
    #[must_use]
    pub(super) fn new(session_id: Option<SessionId>) -> Self {
        Self {
            session_id,
            composer: TextEditBuffer::new(),
            should_exit: false,
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
    pub(super) fn composer_mut(&mut self) -> &mut TextEditBuffer {
        &mut self.composer
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
