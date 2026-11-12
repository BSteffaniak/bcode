//! What the review asks of its run loop after handling input or polling.

use bcode_session_models::SessionId;
use bmux_tui::event::EventOutcome;

/// Effect of one input event or poll on the run loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do.
    None,
    /// Present a new frame.
    Redraw,
    /// Leave the current screen (the review returns to home; home quits the app).
    Close {
        /// The review asks to edit its sources instead of returning home.
        edit_sources: bool,
    },
    /// Open a Bcode session, handing it the terminal, and return to the review afterwards.
    OpenSession {
        /// Session to open.
        session_id: SessionId,
    },
}

impl Action {
    /// A close that returns home.
    pub const CLOSE: Self = Self::Close {
        edit_sources: false,
    };

    /// Whether this action presents a new frame.
    #[must_use]
    pub const fn requests_redraw(&self) -> bool {
        matches!(self, Self::Redraw | Self::OpenSession { .. })
    }
}

/// Routing outcome and effect of one input event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventResult {
    /// Whether the event was consumed and whether a frame is needed.
    pub outcome: EventOutcome,
    /// Effect for the run loop; `None` does not imply that the event was ignored.
    pub action: Action,
}

impl EventResult {
    /// Result of `action`: `None` is ignored input, `Redraw` asks for a frame, and every other
    /// action consumes the event.
    #[must_use]
    pub const fn from_action(action: Action) -> Self {
        let outcome = match &action {
            Action::None => EventOutcome::Ignored,
            Action::Redraw => EventOutcome::Redraw,
            _ => EventOutcome::Handled,
        };
        Self { outcome, action }
    }

    /// Consume the event without an effect.
    #[must_use]
    pub const fn consumed() -> Self {
        Self {
            outcome: EventOutcome::Handled,
            action: Action::None,
        }
    }
}
