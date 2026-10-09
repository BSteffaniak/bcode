//! Bounded evidence persistence and centralized scope reconciliation.
pub use bcode_model_catalog_models::{
    EVIDENCE_SCHEMA_VERSION, EvidenceCapability, EvidenceDocument, EvidenceOutcome,
    EvidenceResolution, EvidenceScope, ModelSupportEvidence,
};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_BYTES: u64 = 1_048_576;
const MAX_OBSERVATIONS: usize = 4096;
const MAX_LIFETIME_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Evidence failure, deliberately excluding document contents and provider errors.
#[derive(Debug)]
pub enum EvidenceError {
    /// Another writer owns the store; retry later. Stale locks require explicit maintenance.
    Busy,
    /// Invalid or secret-unsafe normalized input/document.
    Invalid,
    /// Unknown document contract; existing state is preserved.
    UnsupportedVersion,
    /// Bounded storage limit reached.
    Limit,
    /// Filesystem operation failed.
    Io(std::io::Error),
}
impl std::fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Busy => "model evidence store busy; retry or inspect stale writer lock",
            Self::Invalid => "invalid model evidence; existing state preserved",
            Self::UnsupportedVersion => "unsupported model evidence version; maintenance required",
            Self::Limit => "model evidence storage limit reached",
            Self::Io(_) => "model evidence filesystem operation failed",
        })
    }
}
impl std::error::Error for EvidenceError {}
impl From<std::io::Error> for EvidenceError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// Domain-owned optional store rooted explicitly outside the canonical session DB.
#[derive(Debug, Clone)]
pub struct EvidenceStore {
    directory: PathBuf,
}
impl EvidenceStore {
    /// Select a state root without reading or creating state.
    #[must_use]
    pub fn new(state_root: &Path) -> Self {
        Self {
            directory: state_root.join("model-support-evidence"),
        }
    }
    /// Read a bounded snapshot; missing state is empty, damaged state is never overwritten.
    ///
    /// # Errors
    /// Returns a normalized I/O, version, validation, or size-limit failure.
    pub fn snapshot(&self, _now_ms: u64) -> Result<EvidenceDocument, EvidenceError> {
        let path = self.directory.join("evidence-v1.json");
        let mut file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(EvidenceDocument {
                    schema_version: EVIDENCE_SCHEMA_VERSION,
                    observations: Vec::new(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(EvidenceError::Invalid);
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(EvidenceError::Limit);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| EvidenceError::Invalid)?;
        if value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            != Some(u64::from(EVIDENCE_SCHEMA_VERSION))
        {
            return Err(EvidenceError::UnsupportedVersion);
        }
        let document: EvidenceDocument =
            serde_json::from_value(value).map_err(|_| EvidenceError::Invalid)?;
        validate_document(&document)?;
        Ok(document)
    }
    /// Publish an observation atomically under an exclusive create-new writer lock.
    /// Equal timestamps with conflicting facts are rejected; delayed older facts cannot replace newer ones.
    ///
    /// # Errors
    /// Returns busy, invalid/future/expired input, bounded capacity, or storage failures.
    pub fn record(
        &self,
        evidence: &ModelSupportEvidence,
        now_ms: u64,
    ) -> Result<(), EvidenceError> {
        validate_evidence(evidence)?;
        if evidence.observed_at_ms > now_ms || evidence.expires_at_ms <= now_ms {
            return Err(EvidenceError::Invalid);
        }
        fs::create_dir_all(&self.directory)?;
        let lock_path = self.directory.join("writer.lock");
        let lock = private_create(&lock_path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                EvidenceError::Busy
            } else {
                EvidenceError::Io(error)
            }
        })?;
        let _guard = WriterGuard {
            lock,
            path: lock_path,
        };
        let mut document = self.snapshot(now_ms)?;
        for capability in &evidence.capabilities {
            let mut observation = evidence.clone();
            observation.capabilities = BTreeSet::from([*capability]);
            if let Some(old) = document.observations.iter_mut().find(|old| {
                old.scope == observation.scope
                    && old.model_id == observation.model_id
                    && old.capabilities == observation.capabilities
            }) {
                if attempt_order(old) == attempt_order(&observation) && old != &observation {
                    return Err(EvidenceError::Invalid);
                }
                if attempt_order(old) < attempt_order(&observation) {
                    *old = observation;
                }
            } else {
                document.observations.push(observation);
            }
        }
        validate_document(&document)?;
        let bytes = serde_json::to_vec(&document).map_err(|_| EvidenceError::Invalid)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(EvidenceError::Limit);
        }
        let temporary = self.directory.join("publication.tmp");
        // A crashed publication is never reused or overwritten implicitly.
        let mut file = private_create(&temporary)?;
        let result = (|| {
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, self.directory.join("evidence-v1.json"))?;
            fs::File::open(&self.directory)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(Into::into)
    }
    /// Reconcile a current capability for the exact account and serving surface.
    ///
    /// # Errors
    /// Returns the same read/validation failures as `snapshot`.
    pub fn resolve(
        &self,
        scope: &EvidenceScope,
        model_id: &str,
        capability: EvidenceCapability,
        now_ms: u64,
    ) -> Result<EvidenceResolution, EvidenceError> {
        Ok(resolve_evidence(
            &self.snapshot(now_ms)?,
            scope,
            model_id,
            capability,
            now_ms,
        ))
    }
}
struct WriterGuard {
    lock: fs::File,
    path: PathBuf,
}
impl Drop for WriterGuard {
    fn drop(&mut self) {
        let _ = self.lock.sync_all();
        let _ = fs::remove_file(&self.path);
    }
}
fn private_create(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
/// Resolve the latest relevant fact, including expired tombstones to prevent old evidence resurrection.
/// Simultaneous contradictory observations fail closed to unverified.
#[must_use]
pub fn resolve_evidence(
    document: &EvidenceDocument,
    scope: &EvidenceScope,
    model_id: &str,
    capability: EvidenceCapability,
    now_ms: u64,
) -> EvidenceResolution {
    let relevant: Vec<_> = document
        .observations
        .iter()
        .filter(|e| {
            e.model_id == model_id
                && e.scope.target == scope.target
                && e.scope.endpoint_fingerprint == scope.endpoint_fingerprint
                && (e.scope.account_fingerprint == scope.account_fingerprint
                    || e.outcome == EvidenceOutcome::ProviderSurfaceUnsupported)
                && e.capabilities.contains(&capability)
                && e.observed_at_ms <= now_ms
        })
        .collect();
    let Some(latest) = relevant.iter().max_by_key(|e| attempt_order(e)) else {
        return EvidenceResolution::Unverified;
    };
    if latest.expires_at_ms <= now_ms
        || relevant
            .iter()
            .any(|e| attempt_order(e) == attempt_order(latest) && e.outcome != latest.outcome)
    {
        return EvidenceResolution::Unverified;
    }
    match latest.outcome {
        EvidenceOutcome::Success => EvidenceResolution::Supported,
        EvidenceOutcome::ProviderSurfaceUnsupported => EvidenceResolution::Unsupported,
        EvidenceOutcome::AccountDenied => EvidenceResolution::AccountDenied,
    }
}
pub(crate) const fn attempt_order(e: &ModelSupportEvidence) -> (u64, u64) {
    (e.attempt_started_at_ms, e.attempt_ordinal)
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-:/".contains(&b))
        && !value.contains("://")
}
fn fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn validate_evidence(e: &ModelSupportEvidence) -> Result<(), EvidenceError> {
    let target = &e.scope.target;
    if !identifier(&e.model_id)
        || !identifier(&target.provider)
        || !identifier(&target.auth_mode)
        || !identifier(&target.api_surface)
        || target.integration.as_ref().is_some_and(|v| !identifier(v))
        || !fingerprint(&e.scope.endpoint_fingerprint)
        || !fingerprint(&e.scope.account_fingerprint)
        || e.capabilities.is_empty()
        || e.attempt_started_at_ms > e.observed_at_ms
        || e.expires_at_ms <= e.observed_at_ms
        || e.expires_at_ms - e.observed_at_ms > MAX_LIFETIME_MS
    {
        return Err(EvidenceError::Invalid);
    }
    Ok(())
}
/// Validate a portable snapshot before admitting external observations.
///
/// # Errors
/// Rejects unknown versions, invalid normalized fields, and capacity violations.
pub fn validate_document(document: &EvidenceDocument) -> Result<(), EvidenceError> {
    if document.schema_version != EVIDENCE_SCHEMA_VERSION {
        return Err(EvidenceError::UnsupportedVersion);
    }
    if document.observations.len() > MAX_OBSERVATIONS {
        return Err(EvidenceError::Limit);
    }
    for e in &document.observations {
        validate_evidence(e)?;
    }
    Ok(())
}
