//! Isolated no-crash storage for the real auth state implementation.
use super::{AuthState, AuthStorage, AuthStore, AuthStoreError};
use std::{
    io::{Read as _, Write as _},
    sync::Arc,
};
use switchy_fs::simulator::{
    Filesystem,
    sync::{self, ExclusiveFileLock, File},
    try_with_filesystem,
};

/// An isolated auth storage namespace. Clones reopen the same simulated store.
///
/// No native files, OS permissions, or crash durability are modeled. The namespace
/// is private so callers cannot replace the lock or mutate files outside the owner.
#[derive(Clone)]
pub struct AuthSimulation {
    filesystem: Arc<Filesystem>,
}

impl std::fmt::Debug for AuthSimulation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthSimulation").finish_non_exhaustive()
    }
}

impl AuthSimulation {
    /// Create a fresh namespace and initialize an exclusively owned auth store.
    ///
    /// # Errors
    /// Returns simulated creation, ownership, or publication errors.
    pub fn create() -> Result<(Self, AuthStore), AuthStoreError> {
        let simulation = Self {
            filesystem: Arc::new(Filesystem::new()),
        };
        let lock = try_with_filesystem(&simulation.filesystem, || {
            sync::create_dir("/auth")?;
            File::create("/auth/owner.lock")?.try_lock_exclusive()
        })
        .map_err(AuthStoreError::Io)?;
        let store = AuthStore {
            storage: Box::new(Storage {
                simulation: simulation.clone(),
                _lock: lock,
            }),
        };
        store.commit(&AuthState::default())?;
        Ok((simulation, store))
    }

    /// Reopen the namespace without initializing or repairing its state.
    ///
    /// # Errors
    /// Returns an ownership error while another store is alive, or snapshot errors.
    pub fn open(&self) -> Result<AuthStore, AuthStoreError> {
        let lock = try_with_filesystem(&self.filesystem, || {
            File::open("/auth/owner.lock")?.try_lock_exclusive()
        })
        .map_err(|_| AuthStoreError::OwnershipUnavailable)?;
        AuthStore::from_storage(Box::new(Storage {
            simulation: self.clone(),
            _lock: lock,
        }))
    }
}

struct Storage {
    simulation: AuthSimulation,
    _lock: ExclusiveFileLock,
}

impl std::fmt::Debug for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Storage").finish_non_exhaustive()
    }
}

impl AuthStorage for Storage {
    fn read_snapshot(&self, limit: usize) -> Result<Vec<u8>, AuthStoreError> {
        try_with_filesystem(&self.simulation.filesystem, || {
            let mut bytes = Vec::new();
            File::open("/auth/state.json")?
                .take(limit as u64)
                .read_to_end(&mut bytes)?;
            Ok(bytes)
        })
        .map_err(AuthStoreError::Io)
    }

    fn publish_snapshot(&self, bytes: &[u8]) -> Result<(), AuthStoreError> {
        try_with_filesystem(&self.simulation.filesystem, || {
            let mut file = sync::create_private_file("/auth/pending.json")?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            sync::rename_file("/auth/pending.json", "/auth/state.json")?;
            sync::sync_directory("/auth")
        })
        .map_err(AuthStoreError::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_resolution_reads_owned_updates_without_native_credentials() {
        let (_simulation, mut store) = AuthSimulation::create().unwrap();
        store
            .update(|state| {
                state.subscriptions.pools.insert(
                    "pool".into(),
                    bcode_config::RuntimeAuthSubscriptionPool {
                        profiles: vec![bcode_config::RuntimeAuthSubscriptionProfile {
                            auth_profile: "owned-profile".into(),
                            storage_profile: "stored".into(),
                            vault: "/not-a-native-vault".into(),
                            provider: "openai".into(),
                            owner_plugin_id: Some("bcode.openai-compatible".into()),
                            scheme: "api_key".into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                );
                Ok(())
            })
            .unwrap();
        let config = bcode_config::BcodeConfig::default();
        let mut calls = 0;
        let context = store
            .resolve_provider_context(
                crate::ProviderRequestContextResolution {
                    config: &config,
                    selection: bcode_config::ResolvedModelSelection {
                        auth_pool: Some("pool".into()),
                        provider_plugin_id: Some("bcode.openai-compatible".into()),
                        ..Default::default()
                    },
                },
                |name, profile| {
                    calls += 1;
                    assert_eq!(name, "owned-profile");
                    crate::ResolvedProviderAuth {
                        auth: bcode_model::ProviderAuthContext {
                            scheme: profile.scheme.clone(),
                            ..Default::default()
                        },
                        env: std::collections::BTreeMap::new(),
                    }
                },
            )
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(context.auth_profile.as_deref(), Some("owned-profile"));
    }

    #[test]
    fn owner_moves_between_threads_and_releases_after_failed_mutation() {
        let (simulation, store) = AuthSimulation::create().unwrap();
        let expected = store.snapshot().unwrap();
        std::thread::spawn(move || {
            let mut store = store;
            assert!(
                store
                    .update(|_| Err(AuthStoreError::InvalidPreference))
                    .is_err()
            );
        })
        .join()
        .unwrap();
        assert_eq!(simulation.open().unwrap().snapshot().unwrap(), expected);
    }

    #[test]
    fn real_store_updates_are_isolated_and_reopen_after_release() {
        let (first, mut store) = AuthSimulation::create().unwrap();
        let (_second, other) = AuthSimulation::create().unwrap();
        assert!(matches!(
            first.open(),
            Err(AuthStoreError::OwnershipUnavailable)
        ));
        store
            .update(|state| {
                state.routing.pools.insert(
                    "pool".into(),
                    crate::auth_pool_state::AuthPoolRoutingState::default(),
                );
                Ok(())
            })
            .unwrap();
        let snapshot = store.snapshot().unwrap();
        assert_ne!(snapshot, other.snapshot().unwrap());
        drop(store);
        assert_eq!(first.open().unwrap().snapshot().unwrap(), snapshot);
    }
}
