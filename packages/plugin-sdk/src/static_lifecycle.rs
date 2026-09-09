//! Lifecycle coordination for process-shared concurrent static plugin instances.

use crate::{ConcurrentRustPlugin, PluginError};
use std::sync::{Arc, RwLock, RwLockReadGuard};

/// A static registration shared by hosts, with leases and fresh instances after shutdown.
#[doc(hidden)]
pub struct StaticConcurrentInstance<P> {
    state: RwLock<State<P>>,
}
struct State<P> {
    plugin: Option<Arc<P>>,
    hosts: usize,
}

impl<P: ConcurrentRustPlugin> StaticConcurrentInstance<P> {
    /// Construct an inactive registration.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: RwLock::new(State {
                plugin: None,
                hosts: 0,
            }),
        }
    }

    /// Acquire a host lease, initializing only the first host's instance.
    ///
    /// # Errors
    /// Returns an error for poisoned coordination, activation failure, or lease overflow.
    pub fn activate(&self) -> Result<(), PluginError> {
        let mut state = self
            .state
            .write()
            .map_err(|_| PluginError::failed("static plugin lifecycle unavailable"))?;
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
            .write()
            .map_err(|_| PluginError::failed("static plugin lifecycle unavailable"))?;
        if state.hosts == 0 {
            return Err(PluginError::failed("static plugin is not active"));
        }
        if state.hosts > 1 {
            state.hosts -= 1;
            return Ok(());
        }
        if let Some(plugin) = &state.plugin {
            plugin.deactivate_concurrent()?;
        }
        state.hosts = 0;
        state.plugin = None;
        drop(state);
        Ok(())
    }

    /// Borrow the current instance while fencing final shutdown.
    ///
    /// # Errors
    /// Returns an error when inactive or lifecycle coordination is poisoned.
    pub fn acquire(&self) -> Result<StaticConcurrentLease<'_, P>, PluginError> {
        let state = self
            .state
            .read()
            .map_err(|_| PluginError::failed("static plugin lifecycle unavailable"))?;
        if state.hosts == 0 || state.plugin.is_none() {
            return Err(PluginError::failed("static plugin is not active"));
        }
        Ok(StaticConcurrentLease { state })
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
    state: RwLockReadGuard<'a, State<P>>,
}

impl<P> StaticConcurrentLease<'_, P> {
    /// Active instance, valid for the lease lifetime.
    #[must_use]
    pub fn plugin(&self) -> &Arc<P> {
        self.state
            .plugin
            .as_ref()
            .expect("active lease holds an instance")
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
