//! Managed terminal input event stream.

use bmux_tui::crossterm::read_event;
use bmux_tui::event::Event;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use tokio::sync::mpsc;

use super::TuiError;

/// Async receiver for terminal input events from one managed blocking reader.
pub struct TuiInput {
    receiver: mpsc::UnboundedReceiver<Result<Option<Event>, std::io::Error>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TuiInput {
    /// Start a dedicated terminal input reader.
    #[must_use]
    pub fn start() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Relaxed) {
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
        Self {
            receiver,
            shutdown,
            thread: Some(thread),
        }
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

    /// Request terminal input shutdown.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl Drop for TuiInput {
    fn drop(&mut self) {
        self.request_shutdown();
        // `read_event` may be blocked in the OS terminal backend. Do not join
        // here, because that can hang TUI teardown. The thread exits after the
        // next terminal event or backend error observes shutdown/channel close.
        let _detached = self.thread.take();
    }
}
