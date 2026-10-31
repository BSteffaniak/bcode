//! Domain notification primitives. Read-only access never invalidates a projection.

use std::{
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{Mutex, MutexGuard, broadcast};

#[derive(Debug, Clone)]
pub struct ActivityChanges {
    sender: broadcast::Sender<Option<bcode_session_models::SessionId>>,
    session_id: Option<bcode_session_models::SessionId>,
}

impl Default for ActivityChanges {
    fn default() -> Self {
        Self {
            sender: broadcast::channel(1024).0,
            session_id: None,
        }
    }
}
impl ActivityChanges {
    pub fn notify(&self) {
        let _ = self.sender.send(self.session_id);
    }
    #[must_use]
    pub fn for_session(&self, session_id: bcode_session_models::SessionId) -> Self {
        Self {
            sender: self.sender.clone(),
            session_id: Some(session_id),
        }
    }
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Option<bcode_session_models::SessionId>> {
        self.sender.subscribe()
    }
}

#[derive(Debug)]
pub struct ActivityMutex<T> {
    value: Mutex<T>,
    changes: Option<ActivityChanges>,
}
impl<T: Default> Default for ActivityMutex<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}
impl<T> ActivityMutex<T> {
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            value: Mutex::new(value),
            changes: None,
        }
    }
    #[must_use]
    pub fn observed(value: T, changes: ActivityChanges) -> Self {
        Self {
            value: Mutex::new(value),
            changes: Some(changes),
        }
    }
    pub async fn lock_for(
        &self,
        session_id: bcode_session_models::SessionId,
    ) -> ActivityGuard<'_, T> {
        ActivityGuard {
            guard: Some(self.value.lock().await),
            changes: self
                .changes
                .as_ref()
                .map(|changes| changes.for_session(session_id)),
            dirty: false,
        }
    }
    pub async fn lock(&self) -> ActivityGuard<'_, T> {
        ActivityGuard {
            guard: Some(self.value.lock().await),
            changes: self.changes.clone(),
            dirty: false,
        }
    }
}
pub struct ActivityGuard<'a, T> {
    guard: Option<MutexGuard<'a, T>>,
    changes: Option<ActivityChanges>,
    dirty: bool,
}
impl<T> Deref for ActivityGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.guard.as_deref().expect("guard retained")
    }
}
impl<T> DerefMut for ActivityGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.dirty = true;
        self.guard.as_deref_mut().expect("guard retained")
    }
}
impl<T> Drop for ActivityGuard<'_, T> {
    fn drop(&mut self) {
        // Publish only after releasing the lock: a woken snapshot sees the mutation.
        self.guard.take();
        if self.dirty
            && let Some(changes) = &self.changes
        {
            changes.notify();
        }
    }
}

/// Scheduler counters publish mutations at their owning boundary, not at query sites.
#[derive(Debug)]
pub struct ActivityCounter {
    value: AtomicUsize,
    changes: Option<ActivityChanges>,
}
impl ActivityCounter {
    #[cfg(test)]
    #[must_use]
    pub const fn new(value: usize) -> Self {
        Self {
            value: AtomicUsize::new(value),
            changes: None,
        }
    }
    #[must_use]
    pub const fn observed(value: usize, changes: ActivityChanges) -> Self {
        Self {
            value: AtomicUsize::new(value),
            changes: Some(changes),
        }
    }
    pub fn fetch_add(&self, value: usize, order: Ordering) -> usize {
        let previous = self.value.fetch_add(value, order);
        if value != 0
            && let Some(changes) = &self.changes
        {
            changes.notify();
        }
        previous
    }
    pub fn fetch_sub(&self, value: usize, order: Ordering) -> usize {
        let previous = self.value.fetch_sub(value, order);
        if value != 0
            && let Some(changes) = &self.changes
        {
            changes.notify();
        }
        previous
    }
    #[must_use]
    pub fn change_guard(&self) -> ActivityChangeGuard {
        ActivityChangeGuard(self.changes.clone())
    }
}
impl Deref for ActivityCounter {
    type Target = AtomicUsize;
    fn deref(&self) -> &AtomicUsize {
        &self.value
    }
}

pub struct ActivityChangeGuard(Option<ActivityChanges>);
impl Drop for ActivityChangeGuard {
    fn drop(&mut self) {
        if let Some(changes) = &self.0 {
            changes.notify();
        }
    }
}
