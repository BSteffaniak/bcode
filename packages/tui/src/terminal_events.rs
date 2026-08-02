//! Managed bounded terminal input event stream.

use bmux_tui::event::Event;
use bmux_tui_runtime::ManagedTerminalInput;

use super::TuiError;

const TERMINAL_EVENT_CAPACITY: usize = 64;

/// Async receiver for terminal input events from one managed blocking reader.
pub struct TuiInput {
    input: ManagedTerminalInput,
}

impl TuiInput {
    /// Start a dedicated terminal input reader with bounded admission.
    #[must_use]
    pub fn start() -> Self {
        Self {
            input: ManagedTerminalInput::start(TERMINAL_EVENT_CAPACITY),
        }
    }

    /// Receive the next terminal event.
    ///
    /// # Errors
    ///
    /// Returns an error when the terminal reader fails or closes.
    pub async fn recv(&mut self) -> Result<Option<Event>, TuiError> {
        match self.input.recv().await {
            Some(Ok(event)) => Ok(event),
            Some(Err(error)) => Err(error.into()),
            None => Err(std::io::Error::other("terminal event stream closed").into()),
        }
    }

    /// Request terminal input shutdown.
    pub fn request_shutdown(&self) {
        self.input.request_shutdown();
    }
}

#[cfg(test)]
impl TuiInput {
    pub(crate) fn from_events(events: Vec<Event>) -> Self {
        Self {
            input: ManagedTerminalInput::from_events(events.into_iter().map(Some).map(Ok)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TuiInput;
    use bmux_tui::event::Event;

    #[tokio::test]
    async fn deterministic_input_preserves_bounded_event_order() {
        let mut input = TuiInput::from_events(vec![Event::Tick, Event::User("done".to_owned())]);
        assert_eq!(input.recv().await.expect("first event"), Some(Event::Tick));
        assert_eq!(
            input.recv().await.expect("second event"),
            Some(Event::User("done".to_owned()))
        );
    }
}
