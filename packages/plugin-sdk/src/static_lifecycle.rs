//! Lifecycle coordination for process-shared concurrent static plugin instances.

use crate::{ConcurrentRustPlugin, PluginError};
use std::sync::{Arc, Condvar, Mutex};

/// A static registration shared by hosts, with leases and fresh instances after shutdown.
#[doc(hidden)]
pub struct StaticConcurrentInstance<P> {
    state: Mutex<State<P>>,
    idle: Condvar,
}
struct State<P> {
    plugin: Option<Arc<P>>,
    hosts: usize,
    callbacks: usize,
    stopping: bool,
}

impl<P: ConcurrentRustPlugin> StaticConcurrentInstance<P> {
    /// Construct an inactive registration.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(State {
                plugin: None,
                hosts: 0,
                callbacks: 0,
                stopping: false,
            }),
            idle: Condvar::new(),
        }
    }

    /// Acquire a host lease, initializing only the first host's instance.
    ///
    /// # Errors
    /// Returns an error for poisoned coordination, shutdown in progress, activation failure, or lease overflow.
    pub fn activate(&self) -> Result<(), PluginError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| PluginError::failed("static plugin lifecycle unavailable"))?;
        if state.stopping {
            return Err(PluginError::failed("static plugin is shutting down"));
        }
        if state.hosts == 0 {
            let plugin = Arc::new(P::default());
            plugin.activate_concurrent()?;
            state.plugin = Some(plugin);
        }
        state.hosts = state
            .hosts
            .checked_add(1)
            .ok_or_else(|| PluginError::failed("static plugin lease overflow"))?;
        drop(state);
        Ok(())
    }

    /// Release a host lease; wait for callbacks before final shutdown.
    ///
    /// # Errors
    /// Returns an error for inactive registrations, poisoned coordination, or shutdown failure.
    pub fn deactivate(&self) -> Result<(), PluginError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| PluginError::failed("static plugin lifecycle unavailable"))?;
        if state.hosts == 0 || state.stopping {
            return Err(PluginError::failed("static plugin is not active"));
        }
        if state.hosts > 1 {
            state.hosts -= 1;
            return Ok(());
        }
        state.stopping = true;
        while state.callbacks != 0 {
            state = self
                .idle
                .wait(state)
                .map_err(|_| PluginError::failed("static plugin lifecycle unavailable"))?;
        }
        let plugin = state.plugin.clone();
        drop(state);
        // Keep stopping set while plugin-owned shutdown runs, but do not hold
        // coordination hostage to that potentially blocking callback.
        let result = plugin
            .as_ref()
            .map_or(Ok(()), |plugin| plugin.deactivate_concurrent());
        let mut state = self
            .state
            .lock()
            .map_err(|_| PluginError::failed("static plugin lifecycle unavailable"))?;
        if let Err(error) = result {
            state.stopping = false;
            return Err(error);
        }
        state.stopping = false;
        state.hosts = 0;
        state.plugin = None;
        drop(state);
        Ok(())
    }

    /// Borrow the current instance while fencing final shutdown.
    ///
    /// # Errors
    /// Returns an error when inactive, shutting down, callback leases overflow, or coordination is poisoned.
    pub fn acquire(&self) -> Result<StaticConcurrentLease<'_, P>, PluginError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| PluginError::failed("static plugin lifecycle unavailable"))?;
        if state.hosts == 0 || state.stopping || state.plugin.is_none() {
            return Err(PluginError::failed("static plugin is not active"));
        }
        state.callbacks = state
            .callbacks
            .checked_add(1)
            .ok_or_else(|| PluginError::failed("static plugin callback lease overflow"))?;
        let plugin = Arc::clone(state.plugin.as_ref().expect("active registration"));
        drop(state);
        Ok(StaticConcurrentLease {
            registration: self,
            plugin,
        })
    }
}
impl<P: ConcurrentRustPlugin> Default for StaticConcurrentInstance<P> {
    fn default() -> Self {
        Self::new()
    }
}

/// Invocation lease preventing final deactivation during a callback.
#[doc(hidden)]
pub struct StaticConcurrentLease<'a, P> {
    registration: &'a StaticConcurrentInstance<P>,
    plugin: Arc<P>,
}

impl<P> StaticConcurrentLease<'_, P> {
    /// Active instance, valid for the lease lifetime.
    #[must_use]
    pub const fn plugin(&self) -> &Arc<P> {
        &self.plugin
    }
}

impl<P> Drop for StaticConcurrentLease<'_, P> {
    fn drop(&mut self) {
        let mut state = self
            .registration
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.callbacks -= 1;
        if state.callbacks == 0 {
            self.registration.idle.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct ShutdownPlugin {
        stopped: AtomicBool,
    }
    impl crate::RustPlugin for ShutdownPlugin {}
    impl ConcurrentRustPlugin for ShutdownPlugin {
        fn deactivate_concurrent(&self) -> Result<(), PluginError> {
            self.stopped.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Default)]
    struct PausedShutdown {
        barrier: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
        fail: AtomicBool,
    }
    impl crate::RustPlugin for PausedShutdown {}
    impl ConcurrentRustPlugin for PausedShutdown {
        fn deactivate_concurrent(&self) -> Result<(), PluginError> {
            let barrier = self.barrier.lock().unwrap().take();
            if let Some((started, release)) = barrier {
                started.send(()).unwrap();
                release
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            }
            if self.fail.load(Ordering::SeqCst) {
                Err(PluginError::failed("shutdown failed"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn shutdown_body_rejects_new_work_without_blocking_and_failure_retains_instance() {
        for fail in [false, true] {
            let registration = StaticConcurrentInstance::<PausedShutdown>::new();
            registration.activate().unwrap();
            let lease = registration.acquire().unwrap();
            let old = Arc::clone(lease.plugin());
            old.fail.store(fail, Ordering::SeqCst);
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            *old.barrier.lock().unwrap() = Some((started_tx, release_rx));
            drop(lease);
            std::thread::scope(|scope| {
                let shutdown = scope.spawn(|| registration.deactivate());
                started_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                let registration = &registration;
                scope.spawn(move || {
                    assert!(registration.activate().is_err());
                    assert!(registration.acquire().is_err());
                    assert!(registration.deactivate().is_err());
                    done_tx.send(()).unwrap();
                });
                let responsive = done_rx.recv_timeout(std::time::Duration::from_secs(2));
                release_tx.send(()).unwrap();
                assert!(responsive.is_ok());
                assert_eq!(shutdown.join().unwrap().is_err(), fail);
            });
            if fail {
                let lease = registration.acquire().unwrap();
                assert!(Arc::ptr_eq(lease.plugin(), &old));
                old.fail.store(false, Ordering::SeqCst);
                drop(lease);
                registration.deactivate().unwrap();
            }
            assert!(registration.acquire().is_err());
            registration.activate().unwrap();
            let fresh = registration.acquire().unwrap();
            assert!(!Arc::ptr_eq(fresh.plugin(), &old));
            drop(fresh);
            registration.deactivate().unwrap();
        }
    }

    #[test]
    fn host_changes_do_not_wait_for_pending_callbacks() {
        let registration = StaticConcurrentInstance::<ShutdownPlugin>::new();
        registration.activate().unwrap();
        let pending = registration.acquire().unwrap();
        std::thread::scope(|scope| {
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let registration = &registration;
            scope.spawn(move || {
                registration.activate().unwrap();
                registration.deactivate().unwrap();
                assert!(registration.acquire().is_ok());
                done_tx.send(()).unwrap();
            });
            let result = done_rx.recv_timeout(std::time::Duration::from_secs(2));
            drop(pending);
            result.expect("non-final host changes must not wait for a callback");
        });
        registration.deactivate().unwrap();
    }

    #[test]
    fn reopening_after_picker_shutdown_creates_fresh_instance() {
        let registration = StaticConcurrentInstance::<ShutdownPlugin>::new();
        registration.activate().unwrap();
        let old = registration.acquire().unwrap().plugin().clone();
        registration.deactivate().unwrap();
        assert!(old.stopped.load(Ordering::SeqCst));
        registration.activate().unwrap();
        let new = registration.acquire().unwrap().plugin().clone();
        assert!(!Arc::ptr_eq(&old, &new));
        assert!(!new.stopped.load(Ordering::SeqCst));
        registration.deactivate().unwrap();
    }

    #[test]
    fn one_host_cannot_shut_down_another_hosts_provider() {
        let registration = StaticConcurrentInstance::<ShutdownPlugin>::new();
        registration.activate().unwrap();
        registration.activate().unwrap();
        let plugin = registration.acquire().unwrap().plugin().clone();
        registration.deactivate().unwrap();
        assert!(!plugin.stopped.load(Ordering::SeqCst));
        assert!(registration.acquire().is_ok());
        registration.deactivate().unwrap();
        assert!(plugin.stopped.load(Ordering::SeqCst));
        assert!(registration.acquire().is_err());
    }
}
