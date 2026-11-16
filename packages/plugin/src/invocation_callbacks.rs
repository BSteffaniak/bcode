//! Host-owned lifetime fencing for native plugin invocation callbacks.
//!
//! ABI callback `user_data` carries only a monotonically allocated numeric handle. Raw pointers to
//! invocation-local state never cross the plugin boundary. Closing removes the handle before it
//! waits for callbacks already admitted through that handle, so late callbacks fail closed.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

const SHARD_COUNT: usize = 64;

type RegistryShard = Mutex<BTreeMap<u64, Arc<Entry>>>;

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);
static REGISTRY: OnceLock<[RegistryShard; SHARD_COUNT]> = OnceLock::new();

struct Entry {
    state_address: usize,
    // Admission holds the registry shard until this count has been incremented.
    // Drain predicates and decrements share this mutex with the condition variable.
    in_flight: Mutex<usize>,
    callback_gate: Mutex<()>,
    invocation: Option<bmux_plugin_runtime::InvocationScope>,
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
        Self::new_inner(state, None)
    }

    pub(super) fn scoped<T>(
        state: &mut T,
        invocation: &bmux_plugin_runtime::InvocationScope,
    ) -> Self {
        Self::new_inner(state, Some(invocation.clone()))
    }

    fn new_inner<T>(
        state: &mut T,
        invocation: Option<bmux_plugin_runtime::InvocationScope>,
    ) -> Self {
        let handle = NEXT_HANDLE
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
                    .filter(|value| usize::try_from(*value).is_ok())
            })
            .expect("native callback handle space exhausted");
        let entry = Arc::new(Entry {
            state_address: std::ptr::from_mut(state).expose_provenance(),
            in_flight: Mutex::new(0),
            callback_gate: Mutex::new(()),
            invocation,
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

impl CallbackRegistration {
    fn close_with(&self, before_wait: impl FnOnce()) {
        let entry = {
            let mut shard = registry()[shard_index(self.handle)]
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(entry) = shard.remove(&self.handle) else {
                return;
            };
            drop(shard);
            entry
        };

        let mut drained = entry
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut before_wait = Some(before_wait);
        while *drained != 0 {
            if let Some(before_wait) = before_wait.take() {
                before_wait();
            }
            drained = entry
                .drained
                .wait(drained)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        drop(drained);
    }
}

impl Drop for CallbackRegistration {
    fn drop(&mut self) {
        self.close_with(|| {});
    }
}

/// One admitted callback. Keeps callback state alive and serializes mutable callback access.
pub struct CallbackGuard {
    entry: Arc<Entry>,
    _runtime_admission: Option<bmux_plugin_runtime::AdmissionGuard>,
}

impl CallbackGuard {
    /// Resolve an opaque callback handle. Unknown and closing handles fail closed.
    pub(super) fn acquire(user_data: *mut c_void) -> Option<Self> {
        Self::acquire_with(user_data, || {})
    }

    fn acquire_with(user_data: *mut c_void, after_lookup: impl FnOnce()) -> Option<Self> {
        let handle = user_data.addr() as u64;
        if handle == 0 {
            return None;
        }
        let (entry, runtime_admission) = {
            let shard = registry()[shard_index(handle)]
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = Arc::clone(shard.get(&handle)?);
            let runtime_admission = match &entry.invocation {
                Some(invocation) => Some(invocation.try_admit().ok()?),
                None => None,
            };
            after_lookup();
            let mut in_flight = entry
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *in_flight = in_flight.checked_add(1).expect("callback count exhausted");
            drop(in_flight);
            // Closure cannot remove the entry until admission is fully recorded.
            drop(shard);
            (entry, runtime_admission)
        };
        Some(Self {
            entry,
            _runtime_admission: runtime_admission,
        })
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
            .try_lock()
            .expect("callback capability busy: recursive/concurrent mutable access rejected");
        // SAFETY: The caller supplies the registered type. CallbackRegistration is dropped before
        // the registered stack value and waits for this guard before returning.
        let state =
            unsafe { &mut *std::ptr::with_exposed_provenance_mut(self.entry.state_address) };
        callback(state)
    }
}

impl Drop for CallbackGuard {
    fn drop(&mut self) {
        let mut in_flight = self
            .entry
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *in_flight -= 1;
        if *in_flight == 0 {
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

        let (closing_tx, closing_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();
        let closer = thread::spawn(move || {
            let registration = registration;
            registration.close_with(|| {
                closing_tx.send(()).expect("closing");
            });
            closed_tx.send(()).expect("closed");
        });
        closing_rx
            .recv()
            .expect("closure observed admitted callback");
        assert!(closed_rx.try_recv().is_err());
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
    fn closure_cannot_overtake_admission_after_lookup() {
        let mut state = 41_u64;
        let registration = CallbackRegistration::new(&mut state);
        let handle = registration.user_data().addr();
        let (lookup_tx, lookup_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let (admitted_tx, admitted_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (closing_tx, closing_rx) = mpsc::channel();
        thread::scope(|scope| {
            let callback = scope.spawn(move || {
                let guard =
                    CallbackGuard::acquire_with(std::ptr::without_provenance_mut(handle), || {
                        lookup_tx.send(()).expect("lookup observed");
                        resume_rx.recv().expect("resume admission");
                    })
                    .expect("lookup must finish admission before closure");
                admitted_tx.send(()).expect("admission observed");
                release_rx.recv().expect("release callback");
                // SAFETY: This is the exact registered type; closure drains this guard.
                unsafe { guard.with_state::<u64, _>(|value| *value += 1) };
            });
            lookup_rx.recv().expect("paused after lookup");
            // Lookup still holds the same lock used to remove admissions.
            assert!(registry()[shard_index(handle as u64)].try_lock().is_err());
            let closer = scope.spawn(move || {
                registration.close_with(|| {
                    closing_tx.send(()).expect("closure observed");
                });
            });
            resume_tx.send(()).expect("resume");
            admitted_rx.recv().expect("admitted");
            closing_rx.recv().expect("closure waits for admitted work");
            assert!(CallbackGuard::acquire(std::ptr::without_provenance_mut(handle)).is_none());
            release_tx.send(()).expect("finish callback");
            callback.join().expect("callback");
            closer.join().expect("closer");
        });
        assert_eq!(state, 42);
    }

    #[test]
    fn final_drop_between_drain_predicate_and_wait_is_not_lost() {
        let mut state = ();
        let registration = CallbackRegistration::new(&mut state);
        let guard = CallbackGuard::acquire(registration.user_data()).expect("admitted");
        let (predicate_tx, predicate_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let (dropping_tx, dropping_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();
        thread::scope(|scope| {
            let closer = scope.spawn(move || {
                registration.close_with(|| {
                    predicate_tx.send(()).expect("nonzero predicate");
                    resume_rx.recv().expect("allow atomic unlock and wait");
                });
                closed_tx.send(()).expect("closed");
            });
            predicate_rx.recv().expect("paused immediately before wait");
            // The last callback cannot decrement the predicate outside the drain mutex.
            assert!(guard.entry.in_flight.try_lock().is_err());
            let callback = scope.spawn(move || {
                dropping_tx.send(()).expect("dropping callback");
                drop(guard);
            });
            dropping_rx.recv().expect("drop started");
            resume_tx.send(()).expect("allow wait");
            closed_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("final decrement wakes drain");
            callback.join().expect("callback");
            closer.join().expect("closer");
        });
    }

    #[test]
    fn recursive_mutable_callback_access_rejects_without_waiting() {
        let mut state = 0_u64;
        let registration = CallbackRegistration::new(&mut state);
        let first = CallbackGuard::acquire(registration.user_data()).expect("first");
        let second = CallbackGuard::acquire(registration.user_data()).expect("second");
        // SAFETY: Both guards refer to this exact state; recursive mutation must
        // fail before obtaining a second mutable reference, never wait on itself.
        unsafe {
            first.with_state::<u64, _>(|value| {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    second.with_state::<u64, _>(|_| panic!("must not enter nested state"));
                }));
                assert!(result.is_err());
                *value = 1;
            });
        }
        drop(second);
        drop(first);
        drop(registration);
        assert_eq!(state, 1);
    }

    #[test]
    fn scoped_callbacks_reject_after_runtime_closure_and_retain_admission() {
        let instance = bmux_plugin_runtime::InstanceScope::new(
            bmux_plugin_runtime::PluginRuntimeScopeId::new(),
        );
        let activation = instance
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .expect("activation");
        activation.mark_active().expect("active");
        let invocation = activation.begin_invocation().expect("invocation");
        let mut state = ();
        let registration = CallbackRegistration::scoped(&mut state, &invocation);
        let guard = CallbackGuard::acquire(registration.user_data()).expect("admitted");
        invocation.close().expect("close");
        assert!(CallbackGuard::acquire(registration.user_data()).is_none());
        assert!(
            invocation.finish().is_err(),
            "admitted runtime callback prevents retirement"
        );
        drop(guard);
        drop(registration);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime
            .block_on(invocation.drain())
            .expect("callback drain");
        invocation.finish().expect("retire");
        activation.close().expect("activation close");
        runtime
            .block_on(activation.drain())
            .expect("activation drain");
        activation.finish().expect("activation retire");
        instance.close().expect("instance close");
        runtime.block_on(instance.drain()).expect("instance drain");
        instance.finish().expect("instance retire");
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
