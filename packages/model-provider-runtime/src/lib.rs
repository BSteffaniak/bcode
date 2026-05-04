#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shared turn lifecycle support for native model provider plugins.

use bcode_model::{ProviderError, ProviderErrorCategory, ProviderTurnEvent};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

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

    /// Remove a provider turn from the active store.
    pub fn finish(&mut self, provider_turn_id: &str) {
        self.turns.remove(provider_turn_id);
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

/// Build a current-thread Tokio runtime suitable for native plugin worker threads.
///
/// # Errors
///
/// Returns an error if Tokio cannot build the runtime.
pub fn current_thread_runtime() -> Result<tokio::runtime::Runtime, std::io::Error> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
}
