//! Host-owned lifetime fencing for native plugin invocation callbacks.
//!
//! ABI callback `user_data` carries only a monotonically allocated numeric handle. Raw pointers to
//! invocation-local state never cross the plugin boundary. Closing removes the handle before it
//! waits for callbacks already admitted through that handle, so late callbacks fail closed.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

const SHARD_COUNT: usize = 64;
const OPEN: u8 = 0;
const CLOSING: u8 = 1;

type RegistryShard = Mutex<BTreeMap<u64, Arc<Entry>>>;

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);
static REGISTRY: OnceLock<[RegistryShard; SHARD_COUNT]> = OnceLock::new();

struct Entry {
    state_address: usize,
    lifecycle: AtomicU8,
    in_flight: AtomicUsize,
    callback_gate: Mutex<()>,
    drained_mutex: Mutex<()>,
    drained: Condvar,
}

fn registry() -> &'static [RegistryShard; SHARD_COUNT] {
    REGISTRY.get_or_init(|| std::array::from_fn(|_| Mutex::new(BTreeMap::new())))
}

fn shard_index(handle: u64) -> usize {
    usize::try_from(handle % SHARD_COUNT as u64).expect("shard index fits usize")
}

/// RAII registration for one invocation-local callback state value.
pub struct CallbackRegistration {
    handle: u64,
}

impl CallbackRegistration {
    /// Register callback state for the duration of this value.
    pub(super) fn new<T>(state: &mut T) -> Self {
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        assert_ne!(handle, 0, "native callback handle space exhausted");
        let entry = Arc::new(Entry {
            state_address: std::ptr::from_mut(state).addr(),
            lifecycle: AtomicU8::new(OPEN),
            in_flight: AtomicUsize::new(0),
            callback_gate: Mutex::new(()),
            drained_mutex: Mutex::new(()),
            drained: Condvar::new(),
        });
        let mut shard = registry()[shard_index(handle)]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let inserted = shard.insert(handle, entry).is_none();
        drop(shard);
        assert!(inserted, "native callback handles must never be reused");
        Self { handle }
    }

    /// Return the opaque ABI value. The plugin may copy it but must not dereference it.
    pub(super) fn user_data(&self) -> *mut c_void {
        std::ptr::without_provenance_mut(usize::try_from(self.handle).expect("handle fits pointer"))
    }
}

impl Drop for CallbackRegistration {
    fn drop(&mut self) {
        let entry = {
            let mut shard = registry()[shard_index(self.handle)]
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(entry) = shard.remove(&self.handle) else {
                return;
            };
            entry.lifecycle.store(CLOSING, Ordering::Release);
            drop(shard);
            entry
        };

        let mut drained = entry
            .drained_mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while entry.in_flight.load(Ordering::Acquire) != 0 {
            drained = entry
                .drained
                .wait(drained)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        drop(drained);
    }
}

/// One admitted callback. Keeps callback state alive and serializes mutable callback access.
pub struct CallbackGuard {
    entry: Arc<Entry>,
}

impl CallbackGuard {
    /// Resolve an opaque callback handle. Unknown and closing handles fail closed.
    pub(super) fn acquire(user_data: *mut c_void) -> Option<Self> {
        let handle = user_data.addr() as u64;
        if handle == 0 {
            return None;
        }
        let entry = {
            let shard = registry()[shard_index(handle)]
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = Arc::clone(shard.get(&handle)?);
            drop(shard);
            if entry.lifecycle.load(Ordering::Acquire) != OPEN {
                return None;
            }
            entry.in_flight.fetch_add(1, Ordering::AcqRel);
            entry
        };
        Some(Self { entry })
    }

    /// Invoke a callback against its invocation-local state under the per-invocation gate.
    ///
    /// # Safety
    ///
    /// `T` must be the exact type registered by [`CallbackRegistration::new`]. The registration
    /// must be declared after the state value so it is dropped first. Acquisition and closure
    /// fencing guarantee that the state remains alive for this call.
    pub(super) unsafe fn with_state<T, R>(&self, callback: impl FnOnce(&mut T) -> R) -> R {
        let _gate = self
            .entry
            .callback_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: The caller supplies the registered type. CallbackRegistration is dropped before
        // the registered stack value and waits for this guard before returning.
        let state =
            unsafe { &mut *std::ptr::with_exposed_provenance_mut(self.entry.state_address) };
        callback(state)
    }
}

impl Drop for CallbackGuard {
    fn drop(&mut self) {
        if self.entry.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.entry.drained.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn close_rejects_late_callbacks_and_drains_admitted_callbacks() {
        let mut state = 41_u64;
        let registration = CallbackRegistration::new(&mut state);
        let user_data = registration.user_data();
        let guard = CallbackGuard::acquire(user_data).expect("open callback");

        let (closed_tx, closed_rx) = mpsc::channel();
        let registration_address = std::ptr::from_mut(Box::leak(Box::new(registration))).addr();
        let closer = thread::spawn(move || {
            // SAFETY: Test transfers exclusive ownership of the leaked registration to this
            // thread and reconstructs it exactly once.
            drop(unsafe { Box::from_raw(registration_address as *mut CallbackRegistration) });
            closed_tx.send(()).expect("closed");
        });
        assert!(closed_rx.recv_timeout(Duration::from_millis(25)).is_err());
        assert!(CallbackGuard::acquire(user_data).is_none());
        // SAFETY: `state` is the exact registered type and remains live.
        unsafe { guard.with_state::<u64, _>(|value| *value += 1) };
        drop(guard);
        closed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("close drained callback");
        closer.join().expect("closer");
        assert!(CallbackGuard::acquire(user_data).is_none());
        assert_eq!(state, 42);
    }

    #[test]
    fn handles_are_not_reused() {
        let mut first_state = ();
        let first = CallbackRegistration::new(&mut first_state);
        let first_handle = first.user_data().addr();
        drop(first);
        let mut second_state = ();
        let second = CallbackRegistration::new(&mut second_state);
        assert_ne!(first_handle, second.user_data().addr());
    }
}
