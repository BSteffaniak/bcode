//! Host-side resources bound to one selected activation, with an owner-driven
//! release window.
//!
//! The runtime retains a registered resource until it is explicitly released,
//! and rejects release while any operation, task or child scope of the
//! activation is outstanding. A holder that closes early therefore cannot always
//! release immediately. Instead of retrying on a timer, an unreleasable entry
//! stays in this ledger and the activation owner retires and releases it in its
//! teardown window: after the activation has drained and the author's cleanup
//! hook has settled, and before the activation finishes.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use bmux_plugin_runtime::{ActivationScope, PluginResourceId, ScopeError};

/// A host-owned resource charged to the selected activation.
pub trait ActivationResource: Send + Sync + 'static {
    /// Revoke the resource before the owner releases it at teardown. Called at
    /// most once, and never for a resource its holder already released.
    fn retire(&self);
}

type Entries = BTreeMap<PluginResourceId, Arc<dyn ActivationResource>>;

/// Ledger of host resources registered on one actual activation scope.
///
/// Clones share the ledger. Only the activation owner calls
/// [`Self::retire_all`]; holders register and release their own entries.
#[derive(Clone)]
pub struct ActivationResources {
    scope: ActivationScope,
    entries: Arc<Mutex<Entries>>,
}

impl std::fmt::Debug for ActivationResources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivationResources")
            .field("activation", &self.scope.id())
            .finish_non_exhaustive()
    }
}

impl ActivationResources {
    /// Bind a ledger to the activation scope that owns its resources.
    #[must_use]
    pub fn new(scope: ActivationScope) -> Self {
        Self {
            scope,
            entries: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// The activation whose capacity and lifetime the resources share.
    #[must_use]
    pub const fn scope(&self) -> &ActivationScope {
        &self.scope
    }

    /// Register `resource` on the activation, charged to its resource capacity.
    ///
    /// # Errors
    /// Returns the scope's admission or capacity error; nothing is retained.
    pub fn register<T: ActivationResource>(
        &self,
        resource: Arc<T>,
    ) -> Result<PluginResourceId, ScopeError> {
        let mut entries = self.entries.lock().map_err(|_| ScopeError::Poisoned)?;
        let id = self.scope.register_resource(Arc::clone(&resource))?;
        entries.insert(id, resource);
        drop(entries);
        Ok(id)
    }

    /// Release a holder's resource now when the activation permits it.
    ///
    /// Returns `true` once released, or `false` when outstanding activation
    /// work defers the release to the owner's teardown window.
    ///
    /// # Errors
    /// Returns any scope failure other than outstanding work.
    pub fn release(&self, id: PluginResourceId) -> Result<bool, ScopeError> {
        let mut entries = self.entries.lock().map_err(|_| ScopeError::Poisoned)?;
        if !entries.contains_key(&id) {
            return Ok(true);
        }
        match self.scope.release_resource(id) {
            Ok(owner) => {
                let entry = entries.remove(&id);
                drop(entries);
                drop((owner, entry));
                Ok(true)
            }
            Err(ScopeError::Outstanding(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Owner teardown: retire and release every remaining entry.
    ///
    /// Call after the activation drained and its cleanup hook settled, before
    /// finishing it. Release stays strict; an entry that cannot be released is
    /// retained (fail closed) and its error returned for explicit recovery.
    ///
    /// # Errors
    /// Returns the first release failure; later entries remain retained.
    pub fn retire_all(&self) -> Result<(), ScopeError> {
        let pending = self
            .entries
            .lock()
            .map_err(|_| ScopeError::Poisoned)?
            .iter()
            .map(|(id, resource)| (*id, Arc::clone(resource)))
            .collect::<Vec<_>>();
        for (id, resource) in pending {
            // Holder code runs outside the ledger lock.
            resource.retire();
            let mut entries = self.entries.lock().map_err(|_| ScopeError::Poisoned)?;
            if !entries.contains_key(&id) {
                continue;
            }
            let owner = self.scope.release_resource(id)?;
            let entry = entries.remove(&id);
            drop(entries);
            drop((owner, entry, resource));
        }
        Ok(())
    }

    /// Number of resources still awaiting release.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |entries| entries.len())
    }

    /// Whether every registered resource has been released.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin_runtime::{InstanceScope, PluginConcurrencyConfig, PluginRuntimeScopeId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Probe(AtomicUsize);
    impl ActivationResource for Probe {
        fn retire(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn active() -> (InstanceScope, ActivationScope) {
        let instance = InstanceScope::new(PluginRuntimeScopeId::new());
        let activation = instance
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        (instance, activation)
    }

    #[test]
    fn idle_release_is_immediate_and_never_retired() {
        let (instance, activation) = active();
        let resources = ActivationResources::new(activation.clone());
        let probe = Arc::new(Probe::default());
        let id = resources.register(Arc::clone(&probe)).unwrap();
        assert_eq!(resources.len(), 1);
        assert!(resources.release(id).unwrap());
        assert!(resources.is_empty());
        resources.retire_all().unwrap();
        assert_eq!(probe.0.load(Ordering::SeqCst), 0);
        activation.close().unwrap();
        activation.finish().unwrap();
        instance.close().unwrap();
        instance.finish().unwrap();
    }

    #[test]
    fn busy_release_defers_to_owner_teardown_window() {
        let (instance, activation) = active();
        let resources = ActivationResources::new(activation.clone());
        let probe = Arc::new(Probe::default());
        let id = resources.register(Arc::clone(&probe)).unwrap();
        let admission = activation.try_admit().unwrap();
        assert!(
            !resources.release(id).unwrap(),
            "outstanding work defers release"
        );
        assert_eq!(resources.len(), 1);
        activation.close().unwrap();
        // Finishing before the owner's teardown release must fail closed.
        drop(admission);
        assert!(activation.finish().is_err());
        resources.retire_all().unwrap();
        assert_eq!(probe.0.load(Ordering::SeqCst), 1);
        assert!(resources.is_empty());
        activation.finish().unwrap();
        instance.close().unwrap();
        instance.finish().unwrap();
    }

    #[test]
    fn teardown_release_stays_strict_while_work_is_outstanding() {
        let (instance, activation) = active();
        let resources = ActivationResources::new(activation.clone());
        resources.register(Arc::new(Probe::default())).unwrap();
        let admission = activation.try_admit().unwrap();
        assert!(matches!(
            resources.retire_all(),
            Err(ScopeError::Outstanding(_))
        ));
        assert_eq!(resources.len(), 1, "unreleased entries stay retained");
        drop(admission);
        resources.retire_all().unwrap();
        activation.close().unwrap();
        activation.finish().unwrap();
        instance.close().unwrap();
        instance.finish().unwrap();
    }
}
