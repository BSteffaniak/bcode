//! Dedicated terminal input event stream.

use bmux_tui::crossterm::read_event;
use bmux_tui::event::Event;
use tokio::sync::mpsc;

use super::TuiError;

/// Async receiver for terminal input events from one blocking reader.
pub struct TerminalEventStream {
    receiver: mpsc::UnboundedReceiver<Result<Option<Event>, std::io::Error>>,
}

impl TerminalEventStream {
    /// Spawn a dedicated blocking terminal event reader.
    #[must_use]
    pub fn spawn() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            loop {
                match read_event() {
                    Ok(event) => {
                        if sender.send(Ok(event)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        });
        Self { receiver }
    }

    /// Receive the next terminal event.
    ///
    /// # Errors
    ///
    /// Returns an error when the terminal reader fails or closes.
    pub async fn recv(&mut self) -> Result<Option<Event>, TuiError> {
        match self.receiver.recv().await {
            Some(Ok(event)) => Ok(event),
            Some(Err(error)) => Err(error.into()),
            None => Err(std::io::Error::other("terminal event stream closed").into()),
        }
    }
}
