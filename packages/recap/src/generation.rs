//! Durable, private generation checkpoints. Completed model calls are never silently repeated.
use crate::{
    engine::GenerateRequest,
    model::{
        AnalysisFuture, AnalysisRequest, AnalysisResponse, ModelError, ModelExecutor, ModelFuture,
        ModelRequest, ModelResponse,
    },
    process::Cancellation,
};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Errors preserve existing checkpoints and never imply provider work had no effects.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Another verified process still holds the operation lock.
    #[error("generation is already running")]
    Busy,
    /// Unknown, damaged, conflicting or unsafe durable state.
    #[error("generation checkpoint requires attention: {0}")]
    Invalid(&'static str),
    /// Storage could not be read or committed.
    #[error("generation checkpoint I/O failed")]
    Io(#[from] std::io::Error),
    /// A typed checkpoint could not be decoded.
    #[error("generation checkpoint decoding failed")]
    Json(#[from] serde_json::Error),
}
type Result<T> = std::result::Result<T, Error>;
const MAX_RECORD: u64 = 64 * 1024 * 1024;

/// Independently validated operation kind in the generation compatibility envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum Kind {
    /// New document generation.
    Create,
    /// Next revision of an existing document.
    Refine,
}

/// Caller-visible recovery facts, without prompts, credentials, or raw errors.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    /// Typed operation identity; not inferred from source titles or paths.
    pub kind: Kind,
    /// Compatibility boundary; future versions are rejected without modifying them.
    pub version: u32,
    /// Retained creation request; explicit generation persistence owns its source content.
    pub request: GenerateRequest,
    /// Explicit replay namespace owned by the model adapter.
    pub executor: Option<String>,
    /// Immutable source/media policy fingerprint.
    pub preparation: String,
    /// Exact published document revision, once committed.
    pub published: Option<u64>,
}

/// A process-exclusive generation lease. OS locks release on process death.
pub struct Journal {
    root: PathBuf,
    _lease: File,
    /// Immutable operation header.
    pub status: Status,
    retry_uncertain: bool,
    writes: Mutex<()>,
}

/// Hash a normalized serialized value without persisting request-only credentials.
pub(crate) fn digest(value: &impl Serialize) -> Result<String> {
    Ok(Sha256::digest(serde_json::to_vec(value)?)
        .iter()
        .fold(String::new(), |mut text, byte| {
            use std::fmt::Write as _;
            write!(&mut text, "{byte:02x}").expect("writing to String");
            text
        }))
}
/// Produce a non-secret replay identity from adapter-owned normalized selection facts.
#[must_use]
pub fn identity(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .fold(String::new(), |mut text, byte| {
            use std::fmt::Write as _;
            write!(&mut text, "{byte:02x}").expect("writing to String");
            text
        })
}
fn directory(path: &Path) -> Result<PathBuf> {
    let created = match fs::create_dir(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error.into()),
    };
    let metadata = path.symlink_metadata()?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::Invalid("unsafe checkpoint directory"));
    }
    #[cfg(not(unix))]
    let _ = created;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if created {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(path.canonicalize()?)
}
#[derive(Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    checksum: String,
    value: serde_json::Value,
}
fn record<T: DeserializeOwned>(root: &Path, name: &str) -> Result<Option<T>> {
    let path = root.join(name);
    let metadata = match path.symlink_metadata() {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_RECORD {
        return Err(Error::Invalid("unsafe or oversized checkpoint"));
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_RECORD + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD {
        return Err(Error::Invalid("oversized checkpoint"));
    }
    let envelope: Envelope = serde_json::from_slice(&bytes)?;
    if envelope.version != 1 || digest(&envelope.value)? != envelope.checksum {
        return Err(Error::Invalid("unsupported or damaged checkpoint record"));
    }
    Ok(Some(serde_json::from_value(envelope.value)?))
}
fn atomic(root: &Path, name: &str, value: &impl Serialize) -> Result<()> {
    let value = serde_json::to_value(value)?;
    let envelope = Envelope {
        version: 1,
        checksum: digest(&value)?,
        value,
    };
    let bytes = serde_json::to_vec(&envelope)?;
    if bytes.len() as u64 > MAX_RECORD {
        return Err(Error::Invalid("oversized checkpoint"));
    }
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(root.join(name)).map_err(|error| error.error)?;
    File::open(root)?.sync_all()?;
    Ok(())
}
impl Journal {
    /// Open an explicit generation, retaining ownership until this value is dropped.
    /// # Errors
    /// Rejects unsafe paths, concurrent ownership, changed requests/selection, and unknown versions.
    pub fn open(
        root: &Path,
        request: &GenerateRequest,
        executor: Option<String>,
        preparation: String,
        retry_uncertain: bool,
    ) -> Result<Self> {
        Self::open_kind(
            root,
            request,
            executor,
            preparation,
            retry_uncertain,
            Kind::Create,
        )
    }
    pub(crate) fn open_kind(
        root: &Path,
        request: &GenerateRequest,
        executor: Option<String>,
        preparation: String,
        retry_uncertain: bool,
        kind: Kind,
    ) -> Result<Self> {
        crate::contracts::validate_id(&request.id)
            .map_err(|_| Error::Invalid("invalid generation ID"))?;
        fs::create_dir_all(root)?;
        let store = directory(root)?;
        let generations = directory(&store.join("generations"))?;
        let root = directory(&generations.join(&request.id))?;
        let lease_path = root.join("owner");
        if lease_path
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(Error::Invalid("symlink generation lease"));
        }
        let lease = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lease_path)?;
        lease.try_lock().map_err(|_| Error::Busy)?;
        let status = if let Some(status) = record::<Status>(&root, "status.json")? {
            if status.version != 1 {
                return Err(Error::Invalid("unsupported checkpoint version"));
            }
            if status.executor.is_none() {
                return Err(Error::Invalid("adapter has no stable recovery identity"));
            }
            if status.kind != kind {
                return Err(Error::Invalid("generation operation kind changed"));
            }
            if digest(&status.request)? != digest(request)?
                || status.preparation != preparation
                || status.executor != executor
            {
                return Err(Error::Invalid(
                    "request, source policy or model selection changed; start a new generation",
                ));
            }
            status
        } else {
            let status = Status {
                kind,
                version: 1,
                request: request.clone(),
                executor,
                preparation,
                published: None,
            };
            atomic(&root, "status.json", &status)?;
            status
        };
        directory(&root.join("calls"))?;
        directory(&root.join("assets"))?;
        // Derived discovery pointer failure must not prevent explicit-ID recovery.
        let _ = atomic(&generations, "latest.json", &request.id);
        Ok(Self {
            root,
            _lease: lease,
            status,
            retry_uncertain,
            writes: Mutex::new(()),
        })
    }
    /// Read the most recently opened generation identity without executing or repairing anything.
    /// # Errors
    /// Rejects missing or invalid derived pointers; it does not substitute another generation.
    pub fn latest(store: &Path) -> Result<String> {
        let root = store.canonicalize()?.join("generations");
        if root.symlink_metadata()?.file_type().is_symlink() {
            return Err(Error::Invalid("unsafe generation root"));
        }
        let id =
            record::<String>(&root, "latest.json")?.ok_or(Error::Invalid("no saved generation"))?;
        Self::inspect(store, &id)?;
        Ok(id)
    }

    /// Restore a saved source/media policy, without acquiring execution authority.
    /// # Errors
    /// Rejects missing, unsupported or changed configuration records.
    pub fn configuration(store: &Path, id: &str) -> Result<crate::engine::EngineOptions> {
        let status = Self::inspect(store, id)?;
        let options = record::<crate::engine::EngineOptions>(
            &store.join("generations").join(id),
            "configuration.json",
        )?
        .ok_or(Error::Invalid("missing generation configuration"))?;
        if digest(&format!("recap-engine-v2:{options:?}"))? != status.preparation {
            return Err(Error::Invalid("configuration checksum mismatch"));
        }
        Ok(options)
    }

    /// Read a bounded, exact generation header without claiming execution ownership.
    /// # Errors
    /// Rejects missing, damaged and future headers or unsafe roots.
    pub fn inspect(store: &Path, id: &str) -> Result<Status> {
        crate::contracts::validate_id(id).map_err(|_| Error::Invalid("invalid generation ID"))?;
        if store.symlink_metadata()?.file_type().is_symlink() {
            return Err(Error::Invalid("unsafe store root"));
        }
        let root = store.canonicalize()?;
        let generations = root.join("generations");
        if generations.symlink_metadata()?.file_type().is_symlink() {
            return Err(Error::Invalid("unsafe generation root"));
        }
        let directory = generations.join(id);
        if directory.canonicalize()? != directory
            || directory.symlink_metadata()?.file_type().is_symlink()
        {
            return Err(Error::Invalid("unsafe generation path"));
        }
        let status = record::<Status>(&directory, "status.json")?
            .ok_or(Error::Invalid("generation not found"))?;
        if status.version != 1 || status.request.id != id {
            return Err(Error::Invalid("unsupported or conflicting checkpoint"));
        }
        Ok(status)
    }
    pub(crate) fn read<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        record(&self.root, name)
    }
    pub(crate) fn write(&self, name: &str, value: &impl Serialize) -> Result<()> {
        atomic(&self.root, name, value)
    }
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
    /// Mark publication only after the authoritative document commit.
    /// # Errors
    /// A failed marker write does not roll back a committed document.
    pub fn published(&self, revision: u64) -> Result<()> {
        let mut status = self.status.clone();
        status.published = Some(revision);
        self.write("status.json", &status)
    }
    fn begin<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let _guard = self
            .writes
            .lock()
            .map_err(|_| Error::Invalid("checkpoint lock poisoned"))?;
        let root = self.root.join("calls");
        if root.symlink_metadata()?.file_type().is_symlink() {
            return Err(Error::Invalid("unsafe calls directory"));
        }
        if let Some(result) = record(&root, &format!("{key}.done"))? {
            return Ok(Some(result));
        }
        let inflight = format!("{key}.started");
        if record::<u32>(&root, &inflight)?.is_some() && !self.retry_uncertain {
            return Err(Error::Invalid(
                "unresolved model call; explicit retry approval required",
            ));
        }
        atomic(&root, &inflight, &1_u32)?;
        Ok(None)
    }
    fn finish(&self, key: &str, value: &impl Serialize) -> Result<()> {
        let root = self.root.join("calls");
        atomic(&root, &format!("{key}.done"), value)
    }
}

/// Memoizes exact completed calls inside one generation; never retries errors itself.
pub(crate) struct Executor<'a> {
    pub inner: &'a dyn ModelExecutor,
    pub journal: &'a Journal,
}
impl ModelExecutor for Executor<'_> {
    fn recovery_identity(&self) -> Option<String> {
        self.inner.recovery_identity()
    }
    fn execute<'a>(&'a self, request: ModelRequest, cancel: &'a Cancellation) -> ModelFuture<'a> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(ModelError::Cancelled);
            }
            let key = digest(&request).map_err(|_| ModelError::Checkpoint)?;
            if let Some(response) = self
                .journal
                .begin::<ModelResponse>(&key)
                .map_err(|_| ModelError::RecoveryRequired)?
            {
                cancel.activity(crate::activity::Activity::Reused);
                return Ok(response);
            }
            let response = self.inner.execute(request, cancel).await?;
            self.journal
                .finish(&key, &response)
                .map_err(|_| ModelError::Checkpoint)?;
            Ok(response)
        })
    }
    fn execute_analysis<'a>(
        &'a self,
        request: AnalysisRequest,
        cancel: &'a Cancellation,
    ) -> AnalysisFuture<'a> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(ModelError::Cancelled);
            }
            let key = digest(&request).map_err(|_| ModelError::Checkpoint)?;
            if let Some(response) = self
                .journal
                .begin::<AnalysisResponse>(&key)
                .map_err(|_| ModelError::RecoveryRequired)?
            {
                cancel.activity(crate::activity::Activity::Reused);
                return Ok(response);
            }
            let response = self.inner.execute_analysis(request, cancel).await?;
            self.journal
                .finish(&key, &response)
                .map_err(|_| ModelError::Checkpoint)?;
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicUsize, Ordering},
    };
    fn request() -> GenerateRequest {
        GenerateRequest {
            id: "recovery-test".into(),
            source_id: "source".into(),
            source_title: "Source".into(),
            input: crate::source::SourceInput::Youtube(
                "https://www.youtube.com/watch?v=lXUZvyajciY".into(),
            ),
            transcript: None,
            intent: crate::contracts::GenerationIntent::default(),
        }
    }
    fn open(root: &Path, approve: bool) -> Journal {
        Journal::open(
            root,
            &request(),
            Some("offline-model-v1".into()),
            "policy".into(),
            approve,
        )
        .unwrap()
    }
    fn call() -> ModelRequest {
        ModelRequest {
            instructions: "trusted".into(),
            input_text: "source".into(),
            images: vec![],
            max_response_bytes: 4096,
        }
    }
    struct Model {
        calls: AtomicUsize,
        fail: bool,
    }
    impl ModelExecutor for Model {
        fn execute<'a>(&'a self, _: ModelRequest, _: &'a Cancellation) -> ModelFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if self.fail {
                    Err(ModelError::ExecutionFailed)
                } else {
                    Ok(ModelResponse {
                        title: "accepted result".into(),
                        sections: vec![],
                        image_choices: BTreeMap::new(),
                    })
                }
            })
        }
    }
    #[tokio::test]
    async fn reopened_completed_call_is_reused_but_uncertain_call_requires_approval() {
        let directory = tempfile::tempdir().unwrap();
        let model = Model {
            calls: AtomicUsize::new(0),
            fail: false,
        };
        let journal = open(directory.path(), false);
        let first = Executor {
            inner: &model,
            journal: &journal,
        }
        .execute(call(), &Cancellation::default())
        .await
        .unwrap();
        drop(journal);
        let journal = open(directory.path(), false);
        assert_eq!(
            Executor {
                inner: &model,
                journal: &journal
            }
            .execute(call(), &Cancellation::default())
            .await
            .unwrap(),
            first
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        let failing = Model {
            calls: AtomicUsize::new(0),
            fail: true,
        };
        let mut changed = call();
        changed.input_text = "new work".into();
        assert!(
            Executor {
                inner: &failing,
                journal: &journal
            }
            .execute(changed.clone(), &Cancellation::default())
            .await
            .is_err()
        );
        drop(journal);
        let journal = open(directory.path(), false);
        assert!(matches!(
            Executor {
                inner: &model,
                journal: &journal
            }
            .execute(changed.clone(), &Cancellation::default())
            .await,
            Err(ModelError::RecoveryRequired)
        ));
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        drop(journal);
        let journal = open(directory.path(), true);
        Executor {
            inner: &model,
            journal: &journal,
        }
        .execute(changed, &Cancellation::default())
        .await
        .unwrap();
        assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    }
    #[test]
    fn ownership_versions_damage_and_changed_selection_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let journal = open(directory.path(), false);
        assert!(matches!(
            Journal::open(
                directory.path(),
                &request(),
                Some("offline-model-v1".into()),
                "policy".into(),
                false
            ),
            Err(Error::Busy)
        ));
        drop(journal);
        assert!(
            Journal::open(
                directory.path(),
                &request(),
                Some("different".into()),
                "policy".into(),
                false
            )
            .is_err()
        );
        let journal = open(directory.path(), false);
        let mut future = journal.status.clone();
        future.version = 2;
        journal.write("status.json", &future).unwrap();
        drop(journal);
        assert!(
            Journal::open(
                directory.path(),
                &request(),
                Some("offline-model-v1".into()),
                "policy".into(),
                false
            )
            .is_err()
        );
        let path = directory
            .path()
            .join("generations/recovery-test/status.json");
        fs::write(&path, b"corrupt").unwrap();
        assert!(Journal::inspect(directory.path(), "recovery-test").is_err());
        assert_eq!(fs::read(path).unwrap(), b"corrupt");
    }
    #[cfg(unix)]
    #[test]
    fn symlink_generation_and_record_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("generations")).unwrap();
        std::os::unix::fs::symlink(
            outside.path(),
            directory.path().join("generations/recovery-test"),
        )
        .unwrap();
        assert!(Journal::open(directory.path(), &request(), None, "policy".into(), false).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
