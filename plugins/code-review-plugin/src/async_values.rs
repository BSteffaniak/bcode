//! Non-blocking async value cache for TUI service data.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use tokio::sync::mpsc;

/// Cached state for an asynchronously loaded value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsyncValue<T> {
    /// Value has not been requested.
    Missing,
    /// Value is being loaded in the background.
    Loading,
    /// Value is available.
    Ready(T),
    /// Value failed to load.
    Error(String),
}

/// Completion from a background async value load.
#[derive(Debug)]
pub struct AsyncValueUpdate<K, V> {
    key: K,
    result: Result<V, String>,
}

/// Deduplicating async value cache for TUI data.
#[derive(Debug)]
pub struct AsyncValueStore<K, V> {
    values: BTreeMap<K, AsyncValue<V>>,
    in_flight: BTreeSet<K>,
    sender: mpsc::UnboundedSender<AsyncValueUpdate<K, V>>,
    receiver: mpsc::UnboundedReceiver<AsyncValueUpdate<K, V>>,
}

impl<K, V> Default for AsyncValueStore<K, V>
where
    K: Clone + Ord + Send + 'static,
    V: Send + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> AsyncValueStore<K, V>
where
    K: Clone + Ord + Send + 'static,
    V: Send + 'static,
{
    /// Create an empty async value store.
    #[must_use]
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            values: BTreeMap::new(),
            in_flight: BTreeSet::new(),
            sender,
            receiver,
        }
    }

    /// Return cached state for `key`.
    #[must_use]
    pub fn get(&self, key: &K) -> AsyncValue<&V> {
        match self.values.get(key) {
            Some(AsyncValue::Missing) | None => AsyncValue::Missing,
            Some(AsyncValue::Loading) => AsyncValue::Loading,
            Some(AsyncValue::Ready(value)) => AsyncValue::Ready(value),
            Some(AsyncValue::Error(error)) => AsyncValue::Error(error.clone()),
        }
    }

    /// Ensure `key` is loading or ready without blocking the caller.
    pub fn ensure<F, Fut>(&mut self, key: K, load: F) -> bool
    where
        F: FnOnce(K) -> Fut + Send + 'static,
        Fut: Future<Output = Result<V, String>> + Send + 'static,
    {
        if matches!(self.values.get(&key), Some(AsyncValue::Ready(_)))
            || self.in_flight.contains(&key)
        {
            return false;
        }

        self.values.insert(key.clone(), AsyncValue::Loading);
        self.in_flight.insert(key.clone());
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = load(key.clone()).await;
            let _ = sender.send(AsyncValueUpdate { key, result });
        });
        true
    }

    /// Apply a completed async value update.
    pub fn apply(&mut self, update: AsyncValueUpdate<K, V>) {
        self.in_flight.remove(&update.key);
        let value = match update.result {
            Ok(value) => AsyncValue::Ready(value),
            Err(error) => AsyncValue::Error(error),
        };
        self.values.insert(update.key, value);
    }

    /// Try to receive one completed async value update without blocking.
    ///
    /// # Errors
    ///
    /// Returns an error when no update is currently available or the channel is closed.
    pub fn try_recv(&mut self) -> Result<AsyncValueUpdate<K, V>, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }
}
