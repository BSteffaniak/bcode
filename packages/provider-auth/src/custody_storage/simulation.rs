//! Isolated no-crash custody storage. Never use real secrets in simulation.
use super::{AuthCustodyStorage, CustodyStorageError, MAX_BYTES, validate};
use crate::operations::AuthProvisioningIntent;
use std::{
    io::{Read as _, Write as _},
    sync::Arc,
};
use switchy_fs::simulator::{
    Filesystem,
    sync::{self, ExclusiveFileLock, File},
    try_with_filesystem,
};

/// Private simulated custody namespace, retained independently of an active owner.
///
/// Uses real vault validation and lifecycle policy, but models neither OS permissions nor
/// crashes. Ciphertext and intent are published together in one versioned simulation snapshot.
/// Native custody files are never read or created. Clones refer to the same namespace.
#[derive(Clone)]
pub struct CustodySimulation {
    filesystem: Arc<Filesystem>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    ciphertext: Vec<u8>,
    intent: Option<AuthProvisioningIntent>,
}

impl CustodySimulation {
    /// Initialize a fresh private namespace with validated vault ciphertext.
    ///
    /// # Errors
    /// Rejects invalid ciphertext or unavailable simulated storage, without native fallback.
    pub fn create(
        ciphertext: &[u8],
    ) -> Result<(Self, Box<dyn AuthCustodyStorage>), CustodyStorageError> {
        validate(ciphertext)?;
        let simulation = Self {
            filesystem: Arc::new(Filesystem::new()),
        };
        let lock = try_with_filesystem(&simulation.filesystem, || {
            sync::create_dir("/custody")?;
            sync::create_private_file("/custody/owner")?.try_lock_exclusive()
        })
        .map_err(CustodyStorageError::Io)?;
        let storage = Storage {
            simulation: simulation.clone(),
            _lock: lock,
        };
        storage.publish(&Snapshot {
            version: 1,
            ciphertext: ciphertext.to_vec(),
            intent: None,
        })?;
        Ok((simulation, Box::new(storage)))
    }

    /// Acquire an existing namespace without initialization or repair.
    ///
    /// # Errors
    /// Rejects competing ownership, invalid snapshots, and unavailable storage.
    pub fn open(&self) -> Result<Box<dyn AuthCustodyStorage>, CustodyStorageError> {
        let lock = try_with_filesystem(&self.filesystem, || {
            File::open("/custody/owner")?.try_lock_exclusive()
        })
        .map_err(|_| CustodyStorageError::Ownership)?;
        let storage = Storage {
            simulation: self.clone(),
            _lock: lock,
        };
        storage.snapshot()?;
        Ok(Box::new(storage))
    }
}

struct Storage {
    simulation: CustodySimulation,
    _lock: ExclusiveFileLock,
}

impl Storage {
    fn snapshot(&self) -> Result<Snapshot, CustodyStorageError> {
        let bytes = try_with_filesystem(&self.simulation.filesystem, || {
            match sync::symlink_metadata("/custody/pending") {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(std::io::Error::other("custody requires maintenance")),
            }
            let mut bytes = Vec::new();
            File::open("/custody/state")?
                .take((MAX_BYTES * 4 + 65536 + 1) as u64)
                .read_to_end(&mut bytes)?;
            Ok(bytes)
        })
        .map_err(CustodyStorageError::Io)?;
        if bytes.len() > MAX_BYTES * 4 + 65536 {
            return Err(CustodyStorageError::MaintenanceRequired);
        }
        let state: Snapshot =
            serde_json::from_slice(&bytes).map_err(|_| CustodyStorageError::MaintenanceRequired)?;
        if state.version != 1 {
            return Err(CustodyStorageError::MaintenanceRequired);
        }
        validate(&state.ciphertext)?;
        if let Some(intent) = &state.intent {
            validate_intent(intent)?;
        }
        Ok(state)
    }

    fn publish(&self, state: &Snapshot) -> Result<(), CustodyStorageError> {
        let bytes =
            serde_json::to_vec(state).map_err(|_| CustodyStorageError::MaintenanceRequired)?;
        try_with_filesystem(&self.simulation.filesystem, || {
            let mut pending = sync::create_private_file("/custody/pending")?;
            pending.write_all(&bytes)?;
            pending.sync_all()?;
            drop(pending);
            sync::rename_file("/custody/pending", "/custody/state")?;
            sync::sync_directory("/custody")
        })
        .map_err(CustodyStorageError::Io)
    }
}

fn validate_intent(intent: &AuthProvisioningIntent) -> Result<(), CustodyStorageError> {
    if intent.version != 2
        || intent.source.is_empty()
        || intent.operation.is_empty()
        || serde_json::to_vec(intent)
            .map_err(|_| CustodyStorageError::MaintenanceRequired)?
            .len()
            > 65536
    {
        return Err(CustodyStorageError::MaintenanceRequired);
    }
    Ok(())
}

impl AuthCustodyStorage for Storage {
    fn read(&self) -> Result<Vec<u8>, CustodyStorageError> {
        Ok(self.snapshot()?.ciphertext)
    }
    fn compare_and_publish(
        &mut self,
        expected: &[u8],
        ciphertext: &[u8],
    ) -> Result<(), CustodyStorageError> {
        validate(ciphertext)?;
        let mut state = self.snapshot()?;
        if state.ciphertext != expected {
            return Err(CustodyStorageError::Conflict);
        }
        state.ciphertext = ciphertext.to_vec();
        self.publish(&state)
    }
    fn ensure_no_provisioning(&self) -> Result<(), CustodyStorageError> {
        if self.snapshot()?.intent.is_some() {
            return Err(CustodyStorageError::MaintenanceRequired);
        }
        Ok(())
    }
    fn begin_provisioning(
        &self,
        intent: &AuthProvisioningIntent,
    ) -> Result<(), CustodyStorageError> {
        validate_intent(intent)?;
        let mut state = self.snapshot()?;
        if state.intent.is_some() {
            return Err(CustodyStorageError::MaintenanceRequired);
        }
        state.intent = Some(intent.clone());
        self.publish(&state)
    }
    fn provisioning_intent(&self) -> Result<AuthProvisioningIntent, CustodyStorageError> {
        self.snapshot()?
            .intent
            .ok_or(CustodyStorageError::MaintenanceRequired)
    }
    fn finish_provisioning(&self) -> Result<(), CustodyStorageError> {
        let mut state = self.snapshot()?;
        if state.intent.take().is_none() {
            return Err(CustodyStorageError::MaintenanceRequired);
        }
        self.publish(&state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_fences_and_publication_survive_release_without_native_fallback() {
        let root = tempfile::tempdir().unwrap();
        let public =
            crate::security::ensure_vault_recipient_key(&root.path().join("identity")).unwrap();
        let (vault, key) = sshenv_vault::Vault::create(&public).unwrap();
        let mut bytes = Vec::new();
        vault
            .save_with_storage(&key, |value, _| {
                bytes = value.to_vec();
                Ok(())
            })
            .unwrap();
        let (simulation, mut owner) = CustodySimulation::create(&bytes).unwrap();
        let (_other, other) = CustodySimulation::create(&bytes).unwrap();
        assert!(simulation.open().is_err());
        assert!(owner.compare_and_publish(b"stale", &bytes).is_err());
        assert_eq!(owner.read().unwrap(), bytes);
        let intent = AuthProvisioningIntent {
            version: 2,
            source: "test".into(),
            operation: "operation".into(),
            profile_binding: "binding".into(),
        };
        owner.begin_provisioning(&intent).unwrap();
        assert!(owner.begin_provisioning(&intent).is_err());
        assert!(owner.ensure_no_provisioning().is_err());
        other.ensure_no_provisioning().unwrap();
        std::thread::spawn(move || drop(owner)).join().unwrap();
        let mut owner = simulation.open().unwrap();
        assert!(owner.provisioning_intent().unwrap() == intent);
        owner.compare_and_publish(&bytes, &bytes).unwrap();
        assert!(owner.ensure_no_provisioning().is_err());
        owner.finish_provisioning().unwrap();
        assert!(owner.finish_provisioning().is_err());
        drop(owner);
        simulation.open().unwrap().ensure_no_provisioning().unwrap();
    }
}
