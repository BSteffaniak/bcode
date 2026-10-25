//! Scoped Ctrl-C cancellation for blocking file recognition and model installation.
//!
//! A small Tokio reactor waits for Ctrl-C without polling a microphone, desktop or
//! provider. Dropping the guard stops and joins its thread. No process-global
//! callback retains an operation's cancellation token after the operation ends.
use crate::contracts::Cancellation;
use crate::{Error, Result};

pub(super) struct Interrupt {
    cancel: Cancellation,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Interrupt {
    /// Initialize a scoped interrupt listener before starting blocking work.
    ///
    /// # Errors
    /// Returns a backend error if the signal runtime or handler cannot be
    /// initialized, or an IO error if the listener thread cannot be spawned.
    pub(super) fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Error::Backend("could not initialize interrupt handling".into()))?;
        let cancel = Cancellation::new();
        let signal_cancel = cancel.clone();
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let (ready, registered) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("vox-interrupt".into())
            .spawn(move || {
                runtime.block_on(async move {
                    let signal = tokio::signal::ctrl_c();
                    tokio::pin!(signal);
                    // Poll once before admitting work: Ctrl-C registration is lazy.
                    // If registration fails, do not start a non-cancellable operation.
                    let registration =
                        std::future::poll_fn(|context| match signal.as_mut().poll(context) {
                            std::task::Poll::Pending => std::task::Poll::Ready(Ok(())),
                            std::task::Poll::Ready(Ok(())) => {
                                signal_cancel.cancel();
                                std::task::Poll::Ready(Ok(()))
                            }
                            std::task::Poll::Ready(Err(_)) => std::task::Poll::Ready(Err(())),
                        })
                        .await;
                    if registration.is_err() {
                        let _ = ready.send(false);
                        return;
                    }
                    if ready.send(true).is_err() || signal_cancel.is_cancelled() {
                        return;
                    }
                    tokio::select! {
                        result = &mut signal => {
                            // A later signal-handler failure also aborts safely.
                            let _ = result;
                            signal_cancel.cancel();
                        }
                        _ = &mut stopped => {}
                    }
                });
            })?;
        if registered.recv() != Ok(true) {
            let _ = thread.join();
            return Err(Error::Backend(
                "could not register interrupt handling".into(),
            ));
        }
        Ok(Self {
            cancel,
            stop: Some(stop),
            thread: Some(thread),
        })
    }

    #[must_use]
    pub(super) fn cancellation(&self) -> Cancellation {
        self.cancel.clone()
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

use std::future::Future;
