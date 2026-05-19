#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shared turn lifecycle support for native model provider plugins.

use bcode_model::{ProviderError, ProviderErrorCategory, ProviderTurnEvent};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use tokio::sync::{Notify, oneshot};

/// Outcome from a provider streaming turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOutcome {
    /// The model finished with a normal assistant response.
    Finished,
    /// The model requested one or more tool calls.
    ToolCall,
    /// The turn was cancelled by the host.
    Cancelled,
}

/// Queued event/cancellation state for one provider turn.
#[derive(Debug, Clone, Default)]
pub struct TurnState {
    events: Arc<Mutex<VecDeque<ProviderTurnEvent>>>,
    cancelled: Arc<AtomicBool>,
    cancel_notify: Arc<Notify>,
}

impl TurnState {
    /// Queue a provider event for the host to poll.
    pub fn push(&self, event: ProviderTurnEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push_back(event);
        }
    }

    /// Drain currently queued provider events.
    #[must_use]
    pub fn drain(&self) -> Vec<ProviderTurnEvent> {
        self.events
            .lock()
            .map_or_else(|_| Vec::new(), |mut events| events.drain(..).collect())
    }

    /// Mark the turn as cancelled and wake stream workers.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.cancel_notify.notify_waiters();
    }

    /// Return true once the host has requested cancellation.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Notify fired when the host requests cancellation.
    #[must_use]
    pub fn cancel_notify(&self) -> Arc<Notify> {
        self.cancel_notify.clone()
    }
}

/// In-memory active-turn store used by synchronous plugin entrypoints.
#[derive(Debug, Default)]
pub struct TurnStore {
    next_turn: u64,
    turns: BTreeMap<String, TurnState>,
}

impl TurnStore {
    /// Insert a new turn and return its provider turn id and state.
    pub fn insert_started(&mut self, id_prefix: &str) -> (String, TurnState) {
        self.next_turn += 1;
        let provider_turn_id = format!("{id_prefix}-{}", self.next_turn);
        let turn = TurnState::default();
        turn.push(ProviderTurnEvent::TurnStarted);
        self.turns.insert(provider_turn_id.clone(), turn.clone());
        (provider_turn_id, turn)
    }

    /// Drain queued events for a provider turn.
    #[must_use]
    pub fn drain(&self, provider_turn_id: &str) -> Vec<ProviderTurnEvent> {
        self.turns
            .get(provider_turn_id)
            .map_or_else(Vec::new, TurnState::drain)
    }

    /// Cancel a provider turn if it is active.
    pub fn cancel(&self, provider_turn_id: &str) {
        if let Some(turn) = self.turns.get(provider_turn_id) {
            turn.cancel();
        }
    }

    /// Cancel and remove a provider turn from the active store.
    pub fn finish(&mut self, provider_turn_id: &str) {
        if let Some(turn) = self.turns.remove(provider_turn_id) {
            turn.cancel();
        }
    }
}

/// Build a normalized provider error.
#[must_use]
pub fn provider_error(
    code: impl Into<String>,
    category: ProviderErrorCategory,
    message: impl Into<String>,
) -> ProviderError {
    ProviderError {
        code: code.into(),
        category,
        message: message.into(),
        retryable: matches!(
            category,
            ProviderErrorCategory::Network
                | ProviderErrorCategory::Timeout
                | ProviderErrorCategory::RateLimit
                | ProviderErrorCategory::ProviderInternal
        ),
        provider_message: None,
    }
}

/// Shared Tokio runtime for native model provider plugins.
///
/// The plugin service ABI is synchronous, but providers need async networking for
/// streaming turns, model discovery, and token refresh. This runtime keeps one
/// current-thread Tokio runtime alive on a dedicated background thread so plugins
/// can spawn long-lived async work without creating a new runtime per operation.
pub struct ProviderRuntime {
    handle: tokio::runtime::Handle,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl std::fmt::Debug for ProviderRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderRuntime")
            .finish_non_exhaustive()
    }
}

impl ProviderRuntime {
    /// Start a reusable provider runtime on a dedicated thread.
    ///
    /// # Errors
    ///
    /// Returns an error when the background thread or Tokio runtime cannot be
    /// created, or when the runtime thread exits before startup completes.
    pub fn new() -> Result<Self, ProviderRuntimeError> {
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let thread = thread::Builder::new()
            .name("bcode-provider-runtime".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_sender.send(Err(error));
                        return;
                    }
                };
                let handle = runtime.handle().clone();
                if ready_sender.send(Ok(handle)).is_err() {
                    return;
                }
                runtime.block_on(async {
                    let _ = shutdown_receiver.await;
                });
            })
            .map_err(ProviderRuntimeError::ThreadSpawn)?;
        let handle = ready_receiver
            .recv()
            .map_err(|_| ProviderRuntimeError::StartupDropped)?
            .map_err(ProviderRuntimeError::RuntimeBuild)?;
        Ok(Self {
            handle,
            shutdown: Some(shutdown_sender),
            thread: Some(thread),
        })
    }

    /// Spawn async provider work onto the shared runtime.
    ///
    /// The returned handle may be dropped when the caller does not need the task
    /// result, such as provider turn streaming where completion is reported via
    /// queued provider events.
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handle.spawn(future)
    }

    /// Run an async operation to completion from synchronous plugin code.
    ///
    /// This schedules the future on the background runtime and waits for its
    /// result without constructing a throwaway runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if the background runtime stops before the operation
    /// returns its result.
    pub fn block_on<F>(&self, future: F) -> Result<F::Output, ProviderRuntimeError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.handle.spawn(async move {
            let output = future.await;
            let _ = sender.send(output);
        });
        receiver
            .recv()
            .map_err(|_| ProviderRuntimeError::TaskDropped)
    }
}

impl Drop for ProviderRuntime {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Errors returned by [`ProviderRuntime`].
#[derive(Debug)]
pub enum ProviderRuntimeError {
    /// Tokio runtime construction failed on the background thread.
    RuntimeBuild(std::io::Error),
    /// Runtime thread creation failed.
    ThreadSpawn(std::io::Error),
    /// Runtime thread exited before reporting startup success or failure.
    StartupDropped,
    /// A scheduled operation did not return a result before the runtime stopped.
    TaskDropped,
}

impl std::fmt::Display for ProviderRuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RuntimeBuild(error) => write!(formatter, "runtime build failed: {error}"),
            Self::ThreadSpawn(error) => write!(formatter, "runtime thread spawn failed: {error}"),
            Self::StartupDropped => write!(formatter, "runtime thread exited during startup"),
            Self::TaskDropped => write!(formatter, "runtime task ended without returning a result"),
        }
    }
}

impl std::error::Error for ProviderRuntimeError {}
