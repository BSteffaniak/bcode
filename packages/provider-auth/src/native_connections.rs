//! Canonical non-secret native-account consent/publication in an explicitly selected user state root.
//! This store never launches processes, reads native credentials, or uses plugin disposable storage.
use bcode_harness_models::native_connections::{
    MAX_NATIVE_CONNECTION_BYTES, MAX_NATIVE_CONNECTIONS, NativeCandidate, NativeConnectionError,
    NativeConnectionRecord, NativePreparedConnection,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    record: NativeConnectionRecord,
    mutation_id: String,
    digest: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    schema_version: u16,
    entries: BTreeMap<String, Entry>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: 1,
            entries: BTreeMap::new(),
        }
    }
}
/// Explicitly located native connection authority. Derived catalogs never own consent.
#[derive(Clone, Debug)]
pub struct NativeConnectionStore {
    path: PathBuf,
}
impl NativeConnectionStore {
    /// Select the canonical Bcode user state root, not a plugin work/data directory.
    #[must_use]
    pub fn new(state_root: &Path) -> Self {
        Self {
            path: state_root.join("auth/native-connections.json"),
        }
    }
    fn read(&self) -> Result<State, NativeConnectionError> {
        let file = match bcode_confined_fs::open_file_path(
            &self.path,
            bcode_confined_fs::OpenOptions::new().read(true),
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(State::default());
            }
            Err(_) => return Err(NativeConnectionError::StateUnavailable),
        };
        let mut bytes = Vec::new();
        file.take(MAX_NATIVE_CONNECTION_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| NativeConnectionError::StateUnavailable)?;
        if bytes.len() > MAX_NATIVE_CONNECTION_BYTES {
            return Err(NativeConnectionError::BoundsExceeded);
        }
        let state: State =
            serde_json::from_slice(&bytes).map_err(|_| NativeConnectionError::StateUnavailable)?;
        if state.schema_version != 1
            || state.entries.len() > MAX_NATIVE_CONNECTIONS
            || state.entries.iter().any(|(key, entry)| {
                key != &entry.record.candidate.candidate_id
                    || entry.record.validate().is_err()
                    || !valid_id(&entry.mutation_id)
                    || entry.digest.len() != 64
            })
        {
            return Err(NativeConnectionError::StateUnavailable);
        }
        Ok(state)
    }
    fn lock(&self) -> Result<std::fs::File, NativeConnectionError> {
        let parent = self
            .path
            .parent()
            .ok_or(NativeConnectionError::StateUnavailable)?;
        match bcode_confined_fs::create_private_dir(parent) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                bcode_confined_fs::Dir::open(parent)
                    .map_err(|_| NativeConnectionError::StateUnavailable)?;
            }
            Err(_) => return Err(NativeConnectionError::StateUnavailable),
        }
        let file = bcode_confined_fs::open_file_path(
            &self.path.with_extension("lock"),
            bcode_confined_fs::OpenOptions::new()
                .read(true)
                .write(true)
                .private(true)
                .creation(bcode_confined_fs::Creation::IfMissing),
        )
        .map_err(|_| NativeConnectionError::StateUnavailable)?;
        file.try_lock()
            .map_err(|_| NativeConnectionError::StateUnavailable)?;
        Ok(file)
    }
    fn write(&self, state: &State) -> Result<(), NativeConnectionError> {
        let bytes =
            serde_json::to_vec(state).map_err(|_| NativeConnectionError::StateUnavailable)?;
        if bytes.len() > MAX_NATIVE_CONNECTION_BYTES || state.entries.len() > MAX_NATIVE_CONNECTIONS
        {
            return Err(NativeConnectionError::BoundsExceeded);
        }
        let parent = self
            .path
            .parent()
            .ok_or(NativeConnectionError::StateUnavailable)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|_| NativeConnectionError::StateUnavailable)?;
        temporary
            .write_all(&bytes)
            .map_err(|_| NativeConnectionError::StateUnavailable)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|_| NativeConnectionError::StateUnavailable)?;
        bcode_confined_fs::persist(temporary, &self.path)
            .map_err(|_| NativeConnectionError::StateUnavailable)?;
        bcode_confined_fs::sync_directory_path(parent)
            .map_err(|_| NativeConnectionError::StateUnavailable)
    }
    /// Read bounded records, including revocation tombstones, without creating state.
    /// # Errors
    /// Preserves and rejects unknown, corrupt, oversized or unavailable state.
    pub fn list(&self) -> Result<Vec<NativeConnectionRecord>, NativeConnectionError> {
        Ok(self
            .read()?
            .entries
            .into_values()
            .map(|entry| entry.record)
            .collect())
    }
    /// Approve one freshly rediscovered candidate for all local Bcode consumers.
    /// Caller must establish explicit consent and enabled plugin provenance first.
    /// # Errors
    /// Rejects malformed data, changed revisions, conflicting retries or unavailable state.
    pub fn approve(
        &self,
        candidate: &NativeCandidate,
        expected_revision: u64,
        mutation_id: &str,
        now: u64,
    ) -> Result<NativeConnectionRecord, NativeConnectionError> {
        candidate
            .validate()
            .map_err(|_| NativeConnectionError::InvalidRequest)?;
        self.update(
            &candidate.candidate_id,
            expected_revision,
            mutation_id,
            &("approve", candidate, expected_revision),
            |previous, revision| {
                if previous.is_some_and(|record| record.candidate != *candidate) {
                    return Err(NativeConnectionError::Conflict);
                }
                Ok(NativeConnectionRecord {
                    candidate: candidate.clone(),
                    revision,
                    approved: true,
                    approved_at_unix: now,
                    prepared: None,
                })
            },
        )
    }
    /// Atomically publish plugin-produced preparation only if exact consent is still current.
    /// Caller must authenticate plugin ownership; DTO validation is not compatibility proof.
    /// # Errors
    /// Rejects revoked/drifted consent, duplicate instance IDs or conflicting retries.
    pub fn publish(
        &self,
        prepared: &NativePreparedConnection,
        expected_revision: u64,
        mutation_id: &str,
    ) -> Result<NativeConnectionRecord, NativeConnectionError> {
        prepared
            .validate()
            .map_err(|_| NativeConnectionError::InvalidRequest)?;
        self.update(
            &prepared.candidate.candidate_id,
            expected_revision,
            mutation_id,
            &("publish", prepared, expected_revision),
            |previous, revision| {
                let mut record = previous
                    .filter(|record| record.approved)
                    .cloned()
                    .ok_or(NativeConnectionError::NotApproved)?;
                if record.candidate != prepared.candidate {
                    return Err(NativeConnectionError::Conflict);
                }
                record.revision = revision;
                record.prepared = Some(prepared.clone());
                Ok(record)
            },
        )
    }
    /// Revoke future use without cancelling existing work or touching native account state.
    /// # Errors
    /// Rejects missing records, stale revisions and conflicting retries.
    pub fn revoke(
        &self,
        candidate_id: &str,
        expected_revision: u64,
        mutation_id: &str,
    ) -> Result<NativeConnectionRecord, NativeConnectionError> {
        self.update(
            candidate_id,
            expected_revision,
            mutation_id,
            &("revoke", candidate_id, expected_revision),
            |previous, revision| {
                let mut record = previous
                    .cloned()
                    .ok_or(NativeConnectionError::NotApproved)?;
                record.approved = false;
                record.prepared = None;
                record.revision = revision;
                Ok(record)
            },
        )
    }
    fn update(
        &self,
        candidate_id: &str,
        expected_revision: u64,
        mutation_id: &str,
        operation: &impl Serialize,
        apply: impl FnOnce(
            Option<&NativeConnectionRecord>,
            u64,
        ) -> Result<NativeConnectionRecord, NativeConnectionError>,
    ) -> Result<NativeConnectionRecord, NativeConnectionError> {
        if !valid_id(candidate_id) || !valid_id(mutation_id) {
            return Err(NativeConnectionError::InvalidRequest);
        }
        let digest = hex::encode(Sha256::digest(
            serde_json::to_vec(operation).map_err(|_| NativeConnectionError::InvalidRequest)?,
        ));
        let _lock = self.lock()?;
        let mut state = self.read()?;
        let previous = state.entries.get(candidate_id);
        if let Some(entry) = previous
            && entry.mutation_id == mutation_id
        {
            return if entry.digest == digest {
                Ok(entry.record.clone())
            } else {
                Err(NativeConnectionError::Conflict)
            };
        }
        if previous.map_or(0, |entry| entry.record.revision) != expected_revision {
            return Err(NativeConnectionError::Conflict);
        }
        let revision = expected_revision
            .checked_add(1)
            .ok_or(NativeConnectionError::Conflict)?;
        let record = apply(previous.map(|entry| &entry.record), revision)?;
        if let Some(prepared) = &record.prepared
            && state.entries.iter().any(|(key, entry)| {
                key != candidate_id
                    && entry.record.prepared.as_ref().is_some_and(|other| {
                        other.instance.instance_id == prepared.instance.instance_id
                    })
            })
        {
            return Err(NativeConnectionError::Conflict);
        }
        state.entries.insert(
            candidate_id.to_owned(),
            Entry {
                record: record.clone(),
                mutation_id: mutation_id.to_owned(),
                digest,
            },
        );
        self.write(&state)?;
        Ok(record)
    }
}
fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests;
