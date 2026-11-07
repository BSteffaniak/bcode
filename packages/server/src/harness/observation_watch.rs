//! Daemon-owned native observation ingestion for attached interactive clients.
//!
//! An attached client asks once; the daemon then drives the existing attachment poll in a
//! loop, asking adapters to hold each page until native output arrives. Committed pages reach
//! clients through their ordinary session event subscription, so clients never poll. Every
//! iteration re-admits the client, so a disconnected client or changed owner ends the watch.
#[cfg(test)]
#[path = "observation_watch_tests.rs"]
mod tests;

use super::ManagedHarnessError;
use crate::ServerState;
use bcode_agent_runtime::CancellationToken;
use bcode_ipc::harness::ManagedSessionPollRequest;
use bcode_session_models::{ClientId, SessionId};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// Adapter hold for one page; bounded below the harness transport call deadline.
const WATCH_WAIT_MILLIS: u32 = 20_000;
/// Sessions that may hold a long adapter wait at once. Each wait occupies a shared plugin
/// invocation slot, so excess watches fall back to short, backed-off pages.
const MAX_LONG_WAITS: usize = 16;
/// Live watches across all sessions and clients.
const MAX_WATCHES: usize = 256;
const MIN_IDLE_BACKOFF: Duration = Duration::from_millis(50);
const MAX_IDLE_BACKOFF: Duration = Duration::from_millis(500);
const MIN_FAILURE_BACKOFF: Duration = Duration::from_millis(250);
const MAX_FAILURE_BACKOFF: Duration = Duration::from_secs(5);

type WatchKey = (SessionId, ClientId);

struct Watch {
    generation: u64,
    cancellation: CancellationToken,
}

/// Live attachment watches, keyed by session and client.
pub struct ObservationWatches {
    live: Arc<Mutex<BTreeMap<WatchKey, Arc<Watch>>>>,
    long_waits: Arc<tokio::sync::Semaphore>,
}

impl Default for ObservationWatches {
    fn default() -> Self {
        Self {
            live: Arc::default(),
            long_waits: Arc::new(tokio::sync::Semaphore::new(MAX_LONG_WAITS)),
        }
    }
}

impl ObservationWatches {
    /// Start (or keep) the watch for this client's attachment. A repeated request for the
    /// same generation is idempotent; a new generation replaces the previous watch.
    pub fn start(
        &self,
        state: Weak<ServerState>,
        client: ClientId,
        query: ManagedSessionPollRequest,
    ) {
        let key = (query.session_id, client);
        let watch = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            if let Some(existing) = live.get(&key) {
                if existing.generation == query.generation && !existing.cancellation.is_cancelled()
                {
                    return;
                }
                existing.cancellation.cancel();
            }
            if !live.contains_key(&key) && live.len() >= MAX_WATCHES {
                tracing::warn!(target: "bcode_server::harness", category = "capacity", "managed observation watch rejected");
                return;
            }
            let watch = Arc::new(Watch {
                generation: query.generation,
                cancellation: CancellationToken::new(),
            });
            live.insert(key, Arc::clone(&watch));
            watch
        };
        let live = Arc::clone(&self.live);
        let long_waits = Arc::clone(&self.long_waits);
        tokio::spawn(async move {
            let stopped = drive(&state, client, &query, &watch, &long_waits).await;
            tracing::debug!(target: "bcode_server::harness", category = stopped, "managed observation watch stopped");
            if let Ok(mut live) = live.lock()
                && live
                    .get(&key)
                    .is_some_and(|current| Arc::ptr_eq(current, &watch))
            {
                live.remove(&key);
            }
        });
    }

    /// Stop every watch. In-flight pages finish and commit; no new page starts.
    pub fn shutdown(&self) {
        if let Ok(mut live) = self.live.lock() {
            for watch in std::mem::take(&mut *live).into_values() {
                watch.cancellation.cancel();
            }
        }
    }
}

/// Whether a failed page ends the watch rather than being retried.
const fn terminal(error: ManagedHarnessError) -> bool {
    matches!(
        error,
        ManagedHarnessError::StaleOwner
            | ManagedHarnessError::Invalid
            | ManagedHarnessError::Unsupported
            | ManagedHarnessError::Conflict
    )
}

/// Delay before the next page: none after progress or a full adapter wait, growing backoff
/// after an empty page the adapter did not hold (adapters may ignore `wait_millis`).
fn next_delay(progress: usize, waited: bool, previous: Duration) -> Duration {
    if progress > 0 || waited {
        Duration::ZERO
    } else {
        (previous * 2).clamp(MIN_IDLE_BACKOFF, MAX_IDLE_BACKOFF)
    }
}

async fn drive(
    state: &Weak<ServerState>,
    client: ClientId,
    query: &ManagedSessionPollRequest,
    watch: &Watch,
    long_waits: &Arc<tokio::sync::Semaphore>,
) -> &'static str {
    let caller = crate::harness_dispatch::ManagedCaller::Client(client);
    let mut delay = Duration::ZERO;
    loop {
        if !delay.is_zero() {
            tokio::select! {
                biased;
                () = watch.cancellation.cancelled() => return "cancelled",
                () = tokio::time::sleep(delay) => {}
            }
        }
        if watch.cancellation.is_cancelled() {
            return "cancelled";
        }
        let Some(state) = state.upgrade() else {
            return "state_unavailable";
        };
        if state
            .shutdown_requested
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return "shutdown";
        }
        let Some(backend) = state.managed_sessions.get().cloned() else {
            return "backend_unavailable";
        };
        drop(state);
        let permit = Arc::clone(long_waits).try_acquire_owned().ok();
        let wait = if permit.is_some() {
            WATCH_WAIT_MILLIS
        } else {
            0
        };
        let started = Instant::now();
        // Never abandon an in-flight page: the adapter may already have offered it, and the
        // canonical commit must complete before the next page acknowledges it.
        let result = backend.poll_observations(&caller, query, wait).await;
        drop(permit);
        match result {
            Ok(progress) => {
                let waited =
                    wait > 0 && started.elapsed() >= Duration::from_millis(u64::from(wait / 2));
                delay = next_delay(progress, waited, delay);
            }
            Err(error) if terminal(error) => return "owner_or_caller_changed",
            Err(error) => {
                tracing::debug!(target: "bcode_server::harness", category = %error, "managed observation watch page failed");
                delay = (delay * 2).clamp(MIN_FAILURE_BACKOFF, MAX_FAILURE_BACKOFF);
            }
        }
    }
}
