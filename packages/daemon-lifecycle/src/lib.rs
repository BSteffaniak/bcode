#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Daemon lifecycle registry and cleanup models.

use bcode_ipc::{BUILD_FINGERPRINT, CURRENT_PROTOCOL_VERSION, IpcEndpoint, daemon_namespace};
use bcode_plugin_sdk::path::display_from_current_dir;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::fs;
use std::io::{Read, Seek as _, Write as _};
#[cfg(unix)]
use std::os::fd::AsRawFd as _;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;
#[cfg(all(unix, test))]
use std::os::unix::fs::MetadataExt as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::{Pid, ProcessesToUpdate, System};
use thiserror::Error;

/// Environment variable carrying parent-verified executable digest evidence to a spawned daemon.
pub const BCODE_EXECUTABLE_DIGEST_ENV: &str = "BCODE_EXECUTABLE_DIGEST";

/// Current daemon registry record schema version.
pub const DAEMON_RECORD_SCHEMA_VERSION: u32 = 3;

/// Errors returned by daemon lifecycle registry operations.
#[derive(Debug, Error)]
pub enum DaemonLifecycleError {
    /// Registry I/O failed.
    #[error("daemon registry I/O error at {}: {source}", path.display())]
    Io {
        /// Path associated with the failed operation.
        path: PathBuf,
        /// Original I/O error.
        source: std::io::Error,
    },
    /// Registry serialization failed.
    #[error("daemon registry serialization error: {0}")]
    Serialize(#[from] serde_json::Error),
    /// System time was before the Unix epoch.
    #[error("system clock is before Unix epoch")]
    Clock,
}

/// Serializable local IPC endpoint metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonEndpointRecord {
    /// Unix domain socket endpoint.
    UnixSocket {
        /// Socket path.
        path: PathBuf,
    },
    /// Windows named pipe endpoint.
    WindowsNamedPipe {
        /// Named pipe path.
        name: String,
    },
    /// Endpoint shape not known by this build.
    Unknown {
        /// Debug representation captured for diagnostics.
        debug: String,
    },
}

impl DaemonEndpointRecord {
    /// Convert this record into an IPC endpoint when supported by the current platform.
    #[must_use]
    pub fn to_ipc_endpoint(&self) -> Option<IpcEndpoint> {
        match self {
            Self::UnixSocket { path } => {
                #[cfg(unix)]
                {
                    Some(IpcEndpoint::unix_socket(path.clone()))
                }
                #[cfg(not(unix))]
                {
                    let _ = path;
                    None
                }
            }
            Self::WindowsNamedPipe { name } => {
                #[cfg(windows)]
                {
                    Some(IpcEndpoint::windows_named_pipe(name.clone()))
                }
                #[cfg(not(windows))]
                {
                    let _ = name;
                    None
                }
            }
            Self::Unknown { .. } => None,
        }
    }
}

/// Persistent metadata for one daemon instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRecord {
    /// Record schema version.
    pub schema_version: u32,
    /// Daemon namespace.
    pub namespace: String,
    /// IPC protocol version.
    pub protocol_version: u32,
    /// Exact produced-artifact identity, when advertised by this daemon generation.
    #[serde(default)]
    pub artifact_id: Option<bcode_ipc::ArtifactId>,
    /// Build fingerprint retained as source/build diagnostic evidence.
    pub build_fingerprint: String,
    /// Durable session-storage writer epoch supported by this daemon, when known.
    #[serde(default)]
    pub storage_writer_epoch: Option<u32>,
    /// Process identifier, when available.
    pub pid: Option<u32>,
    /// IPC endpoint for this daemon.
    pub endpoint: DaemonEndpointRecord,
    /// Daemon log path.
    pub log_path: PathBuf,
    /// Executable path used to start the daemon.
    pub executable_path: Option<PathBuf>,
    /// SHA-256 digest of the executable bytes running this daemon.
    #[serde(default)]
    pub executable_digest: Option<String>,
    /// Daemon start time in Unix milliseconds.
    pub started_at_unix_ms: u64,
    /// Last registry write/update time in Unix milliseconds.
    pub last_seen_unix_ms: u64,
    /// Random per-process identity token.
    pub instance_id: String,
}

impl DaemonRecord {
    /// Build a daemon record for the current process and build.
    ///
    /// # Errors
    ///
    /// Returns an error when the system clock is before the Unix epoch.
    pub fn current(
        endpoint: &IpcEndpoint,
        log_path: PathBuf,
        executable_path: Option<PathBuf>,
        instance_id: String,
    ) -> Result<Self, DaemonLifecycleError> {
        let executable_digest = executable_path
            .as_deref()
            .map(executable_sha256)
            .transpose()?;
        Self::current_with_digest(
            endpoint,
            log_path,
            executable_path,
            executable_digest,
            instance_id,
        )
    }

    /// Build a daemon record from already-verified executable digest evidence.
    ///
    /// # Errors
    ///
    /// Returns an error when the system clock is before the Unix epoch.
    pub fn current_with_digest(
        endpoint: &IpcEndpoint,
        log_path: PathBuf,
        executable_path: Option<PathBuf>,
        executable_digest: Option<String>,
        instance_id: String,
    ) -> Result<Self, DaemonLifecycleError> {
        let now = unix_time_millis()?;
        Ok(Self {
            schema_version: DAEMON_RECORD_SCHEMA_VERSION,
            namespace: daemon_namespace(),
            protocol_version: u32::from(CURRENT_PROTOCOL_VERSION),
            artifact_id: Some(bcode_ipc::ArtifactId::current()),
            build_fingerprint: BUILD_FINGERPRINT.to_string(),
            storage_writer_epoch: None,
            pid: Some(std::process::id()),
            endpoint: endpoint_record(endpoint),
            log_path,
            executable_path,
            executable_digest,
            started_at_unix_ms: now,
            last_seen_unix_ms: now,
            instance_id,
        })
    }

    /// Return true when this record describes the current daemon namespace.
    #[must_use]
    pub fn is_current_namespace(&self) -> bool {
        self.namespace == daemon_namespace()
    }
}

/// Return the daemon registry directory under a Bcode state directory.
#[must_use]
pub fn registry_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("daemons")
}

/// Return the registry path for a daemon namespace.
#[must_use]
pub fn record_path(state_dir: &Path, namespace: &str) -> PathBuf {
    registry_dir(state_dir).join(format!("{namespace}.json"))
}

/// Write a daemon registry record atomically.
///
/// # Errors
///
/// Returns an error when creating directories, serializing, writing, or renaming fails.
pub fn write_record(
    state_dir: &Path,
    record: &DaemonRecord,
) -> Result<PathBuf, DaemonLifecycleError> {
    let dir = registry_dir(state_dir);
    fs::create_dir_all(&dir).map_err(|source| DaemonLifecycleError::Io {
        path: dir.clone(),
        source,
    })?;
    let path = record_path(state_dir, &record.namespace);
    let temp_path = path.with_extension(format!("json.tmp-{}", record.instance_id));
    let contents = serde_json::to_vec_pretty(record)?;
    fs::write(&temp_path, contents).map_err(|source| DaemonLifecycleError::Io {
        path: temp_path.clone(),
        source,
    })?;
    fs::rename(&temp_path, &path).map_err(|source| DaemonLifecycleError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// Read daemon registry records from a state directory.
///
/// Invalid records are ignored so one bad file does not block cleanup.
#[must_use]
pub fn read_records(state_dir: &Path) -> Vec<(PathBuf, DaemonRecord)> {
    let dir = registry_dir(state_dir);
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                return None;
            }
            let contents = fs::read(&path).ok()?;
            let record = serde_json::from_slice(&contents).ok()?;
            Some((path, record))
        })
        .collect()
}

/// Remove a daemon registry record.
///
/// # Errors
///
/// Returns an error when removing the registry file fails for reasons other than not found.
pub fn remove_record_path(path: &Path) -> Result<(), DaemonLifecycleError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DaemonLifecycleError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Remove a daemon registry record when it still belongs to the provided instance.
///
/// # Errors
///
/// Returns an error when reading or removing the registry file fails for reasons
/// other than not found.
pub fn remove_record_if_instance(
    path: &Path,
    instance_id: &str,
) -> Result<(), DaemonLifecycleError> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(DaemonLifecycleError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let record = serde_json::from_slice::<DaemonRecord>(&contents)?;
    if record.instance_id == instance_id {
        remove_record_path(path)?;
    }
    Ok(())
}

async fn probe_daemon_status(endpoint: &IpcEndpoint) -> Option<bcode_ipc::DaemonStatus> {
    let mut stream = bcode_ipc::LocalIpcStream::connect(endpoint).await.ok()?;
    let envelope = bcode_ipc::request_envelope(
        1,
        &bcode_ipc::Request::Hello {
            client_name: "bcode-daemon-readiness".to_owned(),
            runtime_context: None,
            daemon_namespace: daemon_namespace(),
            artifact_id: Some(bcode_ipc::ArtifactId::current()),
            build_fingerprint: BUILD_FINGERPRINT.to_owned(),
        },
    )
    .ok()?;
    bcode_ipc::send_envelope(&mut stream, &envelope)
        .await
        .ok()?;
    loop {
        let envelope = bcode_ipc::recv_envelope(&mut stream).await.ok()?;
        if envelope.kind != bcode_ipc::EnvelopeKind::Response || envelope.request_id != 1 {
            continue;
        }
        return match bcode_ipc::decode_response(&envelope.payload).ok()? {
            bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello { daemon, .. }) => {
                Some(daemon)
            }
            _ => None,
        };
    }
}

/// Conservative classification of one daemon registry record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonRecordClassification {
    /// Current-build daemon responded with the exact persisted identity.
    CurrentHealthy,
    /// Historical daemon responded with the exact persisted identity.
    HistoricalExactResponsive,
    /// Endpoint could not be decoded with the current protocol, but independent process evidence
    /// exactly identifies the historical daemon.
    HistoricalProcessVerifiedProtocolUnsupported,
    /// An endpoint responded, but its identity does not match this record.
    ResponsiveIdentityMismatch,
    /// Endpoint is unreachable and positive process evidence proves the recorded process is gone
    /// or replaced.
    UnreachableStale,
    /// Evidence is incomplete or ambiguous; callers must preserve the record.
    Unverifiable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessIdentityEvidence {
    Exact,
    MissingOrReused,
    Unverifiable,
}

fn classify_daemon_record_evidence(
    record: &DaemonRecord,
    status: Option<&bcode_ipc::DaemonStatus>,
    endpoint_reachable: bool,
    process: ProcessIdentityEvidence,
) -> DaemonRecordClassification {
    status.map_or_else(
        || {
            if endpoint_reachable && process == ProcessIdentityEvidence::Exact {
                DaemonRecordClassification::HistoricalProcessVerifiedProtocolUnsupported
            } else if !endpoint_reachable && process == ProcessIdentityEvidence::MissingOrReused {
                DaemonRecordClassification::UnreachableStale
            } else {
                DaemonRecordClassification::Unverifiable
            }
        },
        |status| {
            if daemon_status_matches_record(record, status) {
                if record.is_current_namespace() {
                    DaemonRecordClassification::CurrentHealthy
                } else {
                    DaemonRecordClassification::HistoricalExactResponsive
                }
            } else {
                DaemonRecordClassification::ResponsiveIdentityMismatch
            }
        },
    )
}

fn daemon_status_matches_record(record: &DaemonRecord, status: &bcode_ipc::DaemonStatus) -> bool {
    status.instance_id == record.instance_id
        && status.namespace == record.namespace
        && status.protocol_version == record.protocol_version
        && status.artifact_id == record.artifact_id
        && status.build_fingerprint == record.build_fingerprint
        && status.executable_digest == record.executable_digest
        && status.storage_writer_epoch == record.storage_writer_epoch
        && status.pid == record.pid
        && status.started_at_unix_ms == record.started_at_unix_ms
}

fn process_identity_evidence(record: &DaemonRecord) -> ProcessIdentityEvidence {
    let (Some(pid), Some(expected_digest)) = (record.pid, record.executable_digest.as_deref())
    else {
        return ProcessIdentityEvidence::Unverifiable;
    };
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    let Some(process) = system.process(pid) else {
        return ProcessIdentityEvidence::MissingOrReused;
    };
    let process_started_at_seconds = process.start_time();
    if process_started_at_seconds != record.started_at_unix_ms / 1_000 {
        return ProcessIdentityEvidence::MissingOrReused;
    }
    let Some(executable) = process.exe() else {
        return ProcessIdentityEvidence::Unverifiable;
    };
    // A digest mismatch only proves PID reuse when the pathname is content-addressed, because an
    // immutable image path can never be rewritten in place. A daemon spawned from a mutable
    // pathname (such as a `cargo build` output) keeps running its original bytes even after that
    // path is rebuilt, so a mismatch there is ambiguous rather than positive reuse evidence.
    let path_is_immutable = executable_path_matches_digest(executable, expected_digest);
    match executable_sha256(executable) {
        Ok(actual_digest) if actual_digest == expected_digest => ProcessIdentityEvidence::Exact,
        Ok(_) if path_is_immutable => ProcessIdentityEvidence::MissingOrReused,
        Ok(_) | Err(_) => ProcessIdentityEvidence::Unverifiable,
    }
}

/// Classify one daemon record without mutating registry or endpoint state.
pub async fn classify_daemon_record(record: &DaemonRecord) -> DaemonRecordClassification {
    let endpoint = record.endpoint.to_ipc_endpoint();
    let status = if let Some(endpoint) = endpoint.as_ref() {
        tokio::time::timeout(Duration::from_millis(500), probe_daemon_status(endpoint))
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    let endpoint_reachable = if status.is_some() {
        true
    } else if let Some(endpoint) = endpoint.as_ref() {
        tokio::time::timeout(
            Duration::from_millis(250),
            bcode_ipc::LocalIpcStream::connect(endpoint),
        )
        .await
        .is_ok_and(|result| result.is_ok())
    } else {
        false
    };
    let classification = classify_daemon_record_evidence(
        record,
        status.as_ref(),
        endpoint_reachable,
        process_identity_evidence(record),
    );
    tracing::debug!(
        namespace = record.namespace,
        instance_id = record.instance_id,
        pid = record.pid,
        classification = ?classification,
        "classified daemon registry record"
    );
    classification
}

async fn live_record_matches_instance(record: &DaemonRecord) -> bool {
    matches!(
        classify_daemon_record(record).await,
        DaemonRecordClassification::CurrentHealthy
            | DaemonRecordClassification::HistoricalExactResponsive
    )
}

/// Classify every decodable daemon record without mutating registry state.
pub async fn classified_records(
    state_dir: &Path,
) -> Vec<(PathBuf, DaemonRecord, DaemonRecordClassification)> {
    let mut classified = Vec::new();
    for (path, record) in read_records(state_dir) {
        let classification = classify_daemon_record(&record).await;
        classified.push((path, record, classification));
    }
    classified
}

/// Return whether any live or ambiguous daemon evidence exists for an artifact identity.
///
/// `identity` may be a daemon namespace, build fingerprint, or exact artifact ID. Cleanup
/// callers must refuse mutation when this returns true. Malformed registry records are treated
/// as ambiguous evidence and therefore fail closed.
///
/// # Errors
///
/// Returns an error if the registry directory cannot be read.
pub async fn namespace_has_live_or_ambiguous_evidence(
    state_dir: &Path,
    identity: &str,
) -> Result<bool, DaemonLifecycleError> {
    let dir = registry_dir(state_dir);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(DaemonLifecycleError::Io { path: dir, source }),
    };
    for entry in entries {
        let entry = entry.map_err(|source| DaemonLifecycleError::Io {
            path: dir.clone(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let contents = fs::read(&path).map_err(|source| DaemonLifecycleError::Io {
            path: path.clone(),
            source,
        })?;
        let Ok(record) = serde_json::from_slice::<DaemonRecord>(&contents) else {
            return Ok(true);
        };
        let identity_matches = record.namespace == identity
            || record.build_fingerprint == identity
            || record
                .artifact_id
                .as_ref()
                .is_some_and(|artifact_id| artifact_id.as_str() == identity);
        if !identity_matches {
            continue;
        }
        if !matches!(
            classify_daemon_record(&record).await,
            DaemonRecordClassification::UnreachableStale
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Return daemon registry records whose IPC endpoints currently respond and whose server identity
/// matches the persisted instance record.
///
/// Stale, malformed, or endpoint-reused records are excluded. This function does not mutate
/// registry files.
pub async fn live_records(state_dir: &Path) -> Vec<(PathBuf, DaemonRecord)> {
    let mut live = Vec::new();
    for (path, record) in read_records(state_dir) {
        if live_record_matches_instance(&record).await {
            live.push((path, record));
        }
    }
    live
}

fn storage_writer_is_incompatible(record: &DaemonRecord, storage_writer_epoch: u32) -> bool {
    record.storage_writer_epoch != Some(storage_writer_epoch)
}

/// Return live daemons that do not advertise `storage_writer_epoch`.
///
/// Legacy records with no writer epoch are incompatible by default.
pub async fn incompatible_storage_writer_records(
    state_dir: &Path,
    storage_writer_epoch: u32,
) -> Vec<(PathBuf, DaemonRecord)> {
    classified_records(state_dir)
        .await
        .into_iter()
        .filter(|(_, record, classification)| {
            matches!(
                classification,
                DaemonRecordClassification::CurrentHealthy
                    | DaemonRecordClassification::HistoricalExactResponsive
            ) && storage_writer_is_incompatible(record, storage_writer_epoch)
        })
        .map(|(path, record, _)| (path, record))
        .collect()
}

/// Metadata published beside one immutable daemon image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonImageMetadata {
    /// Metadata schema version.
    pub schema_version: u32,
    /// Exact produced-artifact identity embedded in the image.
    pub artifact_id: bcode_ipc::ArtifactId,
    /// SHA-256 digest of the image bytes.
    pub executable_digest: String,
}

const DAEMON_IMAGE_METADATA_SCHEMA_VERSION: u32 = 1;
const DAEMON_IMAGE_METADATA_FILE: &str = "image.json";
const DAEMON_IMAGE_CLEANUP_LOCK_FILE: &str = "daemon-images.lock";

/// Return the artifact-scoped directory that stores cached daemon executables.
#[must_use]
pub fn daemon_image_dir(state_dir: &Path) -> PathBuf {
    state_dir
        .join("daemon-images")
        .join(bcode_ipc::ArtifactId::current().as_str())
}

/// Bootstrap snapshot of the exact running artifact retained independently of its pathname.
#[derive(Debug)]
pub struct ArtifactBootstrap {
    artifact_id: bcode_ipc::ArtifactId,
    source_path: PathBuf,
    source: Mutex<fs::File>,
    executable_digest: OnceLock<String>,
}

static ARTIFACT_BOOTSTRAP: OnceLock<ArtifactBootstrap> = OnceLock::new();

/// Capture and retain the running artifact before its original pathname can be replaced.
///
/// Repeated calls return the same process bootstrap. The retained file handle remains readable
/// after Unix pathname replacement and prevents later lifecycle work from reopening a different
/// file at the original path.
///
/// # Panics
///
/// Panics only if the process-global bootstrap cannot be observed immediately after this function
/// successfully initializes it.
///
/// # Errors
///
/// Returns an error when the current executable cannot be located or opened.
pub fn initialize_artifact_bootstrap() -> Result<&'static ArtifactBootstrap, DaemonLifecycleError> {
    if let Some(bootstrap) = ARTIFACT_BOOTSTRAP.get() {
        return Ok(bootstrap);
    }
    let source_path = std::env::current_exe().map_err(|source| DaemonLifecycleError::Io {
        path: PathBuf::from("<current_exe>"),
        source,
    })?;
    let bootstrap = ArtifactBootstrap::open(source_path, bcode_ipc::ArtifactId::current())?;
    let _ = ARTIFACT_BOOTSTRAP.set(bootstrap);
    Ok(ARTIFACT_BOOTSTRAP
        .get()
        .expect("artifact bootstrap initialized"))
}

impl ArtifactBootstrap {
    fn open(
        source_path: PathBuf,
        artifact_id: bcode_ipc::ArtifactId,
    ) -> Result<Self, DaemonLifecycleError> {
        let source = fs::File::open(&source_path).map_err(|source| DaemonLifecycleError::Io {
            path: source_path.clone(),
            source,
        })?;
        Ok(Self {
            artifact_id,
            source_path,
            source: Mutex::new(source),
            executable_digest: OnceLock::new(),
        })
    }

    /// Return the exact embedded artifact identity.
    #[must_use]
    pub const fn artifact_id(&self) -> &bcode_ipc::ArtifactId {
        &self.artifact_id
    }

    /// Return the pathname observed at bootstrap for diagnostics only.
    #[must_use]
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    /// Report whether the bootstrap source path still resolves to the retained artifact bytes.
    ///
    /// Daemons are never spawned from this mutable pathname, so this is diagnostic evidence only.
    #[cfg(all(unix, test))]
    fn source_path_still_current(&self) -> bool {
        let Ok(path_metadata) = fs::metadata(&self.source_path) else {
            return false;
        };
        let Ok(source) = self.source.lock() else {
            return false;
        };
        let Ok(source_metadata) = source.metadata() else {
            return false;
        };
        drop(source);
        path_metadata.dev() == source_metadata.dev() && path_metadata.ino() == source_metadata.ino()
    }

    fn digest(&self) -> Result<String, DaemonLifecycleError> {
        if let Some(digest) = self.executable_digest.get() {
            return Ok(digest.clone());
        }
        let mut source = self.source.lock().map_err(|_| DaemonLifecycleError::Io {
            path: self.source_path.clone(),
            source: std::io::Error::other("artifact bootstrap lock poisoned"),
        })?;
        source.rewind().map_err(|error| DaemonLifecycleError::Io {
            path: self.source_path.clone(),
            source: error,
        })?;
        let digest = sha256_reader(&mut *source, &self.source_path)?;
        drop(source);
        let _ = self.executable_digest.set(digest.clone());
        Ok(digest)
    }

    fn clone_to(&self, target: &Path) -> Result<bool, DaemonLifecycleError> {
        let source = self.source.lock().map_err(|_| DaemonLifecycleError::Io {
            path: self.source_path.clone(),
            source: std::io::Error::other("artifact bootstrap lock poisoned"),
        })?;
        try_clone_file_from_handle(&source, target).map_err(|source| DaemonLifecycleError::Io {
            path: target.to_path_buf(),
            source,
        })
    }

    fn copy_to(&self, target: &Path) -> Result<String, DaemonLifecycleError> {
        let mut source = self.source.lock().map_err(|_| DaemonLifecycleError::Io {
            path: self.source_path.clone(),
            source: std::io::Error::other("artifact bootstrap lock poisoned"),
        })?;
        source.rewind().map_err(|error| DaemonLifecycleError::Io {
            path: self.source_path.clone(),
            source: error,
        })?;
        let mut output = fs::File::create(target).map_err(|source| DaemonLifecycleError::Io {
            path: target.to_path_buf(),
            source,
        })?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 1024 * 1024].into_boxed_slice();
        loop {
            let read = source
                .read(&mut buffer)
                .map_err(|source| DaemonLifecycleError::Io {
                    path: self.source_path.clone(),
                    source,
                })?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|source| DaemonLifecycleError::Io {
                    path: target.to_path_buf(),
                    source,
                })?;
            hasher.update(&buffer[..read]);
        }
        drop(source);
        output
            .sync_all()
            .map_err(|source| DaemonLifecycleError::Io {
                path: target.to_path_buf(),
                source,
            })?;
        let digest = format!("{:x}", hasher.finalize());
        let _ = self.executable_digest.set(digest.clone());
        Ok(digest)
    }
}

#[cfg(target_os = "macos")]
fn try_clone_file_from_handle(source: &fs::File, target: &Path) -> std::io::Result<bool> {
    let target = std::ffi::CString::new(target.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in image path"))?;
    // SAFETY: `source` remains open for the call and `target` is a valid NUL-terminated path.
    let result =
        unsafe { libc::fclonefileat(source.as_raw_fd(), libc::AT_FDCWD, target.as_ptr(), 0) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error
        .raw_os_error()
        .is_some_and(|code| [libc::ENOTSUP, libc::EXDEV, libc::EINVAL].contains(&code))
    {
        return Ok(false);
    }
    Err(error)
}

#[cfg(target_os = "linux")]
fn try_clone_file_from_handle(source: &fs::File, target: &Path) -> std::io::Result<bool> {
    let output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    // SAFETY: both file descriptors remain valid for the duration of the ioctl call.
    let result = unsafe { libc::ioctl(output.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    drop(output);
    let _ = fs::remove_file(target);
    if error.raw_os_error().is_some_and(|code| {
        [libc::ENOTTY, libc::EOPNOTSUPP, libc::EXDEV, libc::EINVAL].contains(&code)
    }) {
        return Ok(false);
    }
    Err(error)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn try_clone_file_from_handle(_source: &fs::File, _target: &Path) -> std::io::Result<bool> {
    Ok(false)
}

fn sha256_reader(mut reader: impl Read, path: &Path) -> Result<String, DaemonLifecycleError> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024].into_boxed_slice();
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|source| DaemonLifecycleError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Return the immutable cached executable path for one binary digest.
#[must_use]
pub fn cached_daemon_executable_path_for_digest(
    state_dir: &Path,
    executable_digest: &str,
) -> PathBuf {
    daemon_image_dir(state_dir)
        .join(executable_digest)
        .join(if cfg!(windows) { "bcode.exe" } else { "bcode" })
}

/// Return the cached executable path for the current process image.
///
/// # Errors
///
/// Returns an error when the current executable cannot be located or read.
pub fn current_cached_daemon_executable_path(
    state_dir: &Path,
) -> Result<PathBuf, DaemonLifecycleError> {
    let bootstrap = initialize_artifact_bootstrap()?;
    let digest = bootstrap.digest()?;
    Ok(cached_daemon_executable_path_for_digest(state_dir, &digest))
}

/// Return the lowercase SHA-256 digest of a file.
///
/// # Errors
///
/// Returns an error when the file cannot be opened or read.
pub fn executable_sha256(path: &Path) -> Result<String, DaemonLifecycleError> {
    let mut file = fs::File::open(path).map_err(|source| DaemonLifecycleError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    sha256_reader(&mut file, path)
}

/// Return the current process executable and its byte digest.
///
/// # Errors
///
/// Returns an error when the executable cannot be located or read.
pub fn current_executable_identity() -> Result<(PathBuf, String), DaemonLifecycleError> {
    let bootstrap = initialize_artifact_bootstrap()?;
    Ok((bootstrap.source_path().to_path_buf(), bootstrap.digest()?))
}

fn cached_executable_digest(path: &Path) -> Option<&str> {
    path.parent()?.file_name()?.to_str()
}

/// Verify that an executable digest agrees with its content-addressed cache path.
#[must_use]
pub fn executable_path_matches_digest(path: &Path, executable_digest: &str) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .and_then(|component| component.to_str())
        == Some(executable_digest)
}

fn daemon_image_metadata_path(executable: &Path) -> PathBuf {
    executable
        .parent()
        .expect("content-addressed daemon image has a parent")
        .join(DAEMON_IMAGE_METADATA_FILE)
}

fn read_daemon_image_metadata(
    executable: &Path,
) -> Result<Option<DaemonImageMetadata>, DaemonLifecycleError> {
    let metadata_path = daemon_image_metadata_path(executable);
    let contents = match fs::read(&metadata_path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DaemonLifecycleError::Io {
                path: metadata_path,
                source,
            });
        }
    };
    Ok(serde_json::from_slice(&contents).ok())
}

fn daemon_image_is_valid(
    executable: &Path,
    expected_digest: &str,
) -> Result<bool, DaemonLifecycleError> {
    if !executable.is_file() || executable_sha256(executable)? != expected_digest {
        return Ok(false);
    }
    let Some(metadata) = read_daemon_image_metadata(executable)? else {
        return Ok(false);
    };
    Ok(
        metadata.schema_version == DAEMON_IMAGE_METADATA_SCHEMA_VERSION
            && metadata.artifact_id == bcode_ipc::ArtifactId::current()
            && metadata.executable_digest == expected_digest,
    )
}

fn write_daemon_image_metadata(
    executable: &Path,
    digest: &str,
) -> Result<(), DaemonLifecycleError> {
    let metadata_path = daemon_image_metadata_path(executable);
    let metadata = DaemonImageMetadata {
        schema_version: DAEMON_IMAGE_METADATA_SCHEMA_VERSION,
        artifact_id: bcode_ipc::ArtifactId::current(),
        executable_digest: digest.to_owned(),
    };
    let contents = serde_json::to_vec_pretty(&metadata)?;
    fs::write(&metadata_path, contents).map_err(|source| DaemonLifecycleError::Io {
        path: metadata_path,
        source,
    })
}

fn remove_interrupted_image_publications(state_dir: &Path) -> Result<(), DaemonLifecycleError> {
    let artifact_dir = daemon_image_dir(state_dir);
    let entries = match fs::read_dir(&artifact_dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(DaemonLifecycleError::Io {
                path: artifact_dir,
                source,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| DaemonLifecycleError::Io {
            path: artifact_dir.clone(),
            source,
        })?;
        let path = entry.path();
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".bcode.tmp-")
        {
            fs::remove_file(&path).map_err(|source| DaemonLifecycleError::Io { path, source })?;
            continue;
        }
        if !entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
            continue;
        }
        let child_entries = fs::read_dir(&path).map_err(|source| DaemonLifecycleError::Io {
            path: path.clone(),
            source,
        })?;
        for child in child_entries {
            let child = child.map_err(|source| DaemonLifecycleError::Io {
                path: path.clone(),
                source,
            })?;
            if child
                .file_name()
                .to_string_lossy()
                .starts_with(".bcode.tmp-")
            {
                let child_path = child.path();
                fs::remove_file(&child_path).map_err(|source| DaemonLifecycleError::Io {
                    path: child_path,
                    source,
                })?;
            }
        }
    }
    Ok(())
}

fn current_cached_daemon_image(
    state_dir: &Path,
) -> Result<Option<(PathBuf, String)>, DaemonLifecycleError> {
    let artifact_dir = daemon_image_dir(state_dir);
    let entries = match fs::read_dir(&artifact_dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DaemonLifecycleError::Io {
                path: artifact_dir,
                source,
            });
        }
    };
    let mut candidate = None;
    for entry in entries {
        let entry = entry.map_err(|source| DaemonLifecycleError::Io {
            path: artifact_dir.clone(),
            source,
        })?;
        if !entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
            continue;
        }
        let executable = entry
            .path()
            .join(if cfg!(windows) { "bcode.exe" } else { "bcode" });
        let Some(metadata) = read_daemon_image_metadata(&executable)? else {
            continue;
        };
        if metadata.schema_version != DAEMON_IMAGE_METADATA_SCHEMA_VERSION
            || metadata.artifact_id != bcode_ipc::ArtifactId::current()
            || !executable_path_matches_digest(&executable, &metadata.executable_digest)
            || !daemon_image_is_valid(&executable, &metadata.executable_digest)?
        {
            continue;
        }
        if candidate.is_some() {
            return Ok(None);
        }
        candidate = Some((executable, metadata.executable_digest));
    }
    Ok(candidate)
}

/// Ensure the currently running executable is cached for detached daemon starts.
///
/// The returned path is content-addressed and never replaced after publication.
///
/// # Errors
///
/// Returns an error when the current executable cannot be located, copied, verified, or made
/// executable.
pub fn ensure_current_executable_cached() -> Result<PathBuf, DaemonLifecycleError> {
    ensure_current_executable_cached_in_state(&bcode_config::default_state_dir())
}

fn materialize_verified_daemon_image(
    bootstrap: &ArtifactBootstrap,
    source: &Path,
    temp: &Path,
) -> Result<(String, bool), DaemonLifecycleError> {
    let cloned = bootstrap.clone_to(temp)?;
    tracing::debug!(
        target: "bcode_daemon_lifecycle::startup",
        cloned,
        "daemon image clone attempted"
    );
    let verify_started_at = std::time::Instant::now();
    let digest = if cloned {
        let digest = executable_sha256(temp)?;
        fs::OpenOptions::new()
            .read(true)
            .open(temp)
            .and_then(|file| file.sync_all())
            .map_err(|source| DaemonLifecycleError::Io {
                path: temp.to_path_buf(),
                source,
            })?;
        let _ = bootstrap.executable_digest.set(digest.clone());
        digest
    } else {
        bootstrap.copy_to(temp)?
    };
    tracing::debug!(
        target: "bcode_daemon_lifecycle::startup",
        elapsed_ms = verify_started_at.elapsed().as_millis(),
        cloned,
        "daemon image bytes verified and synchronized"
    );
    preserve_executable_permissions(source, temp)?;
    Ok((digest, cloned))
}

fn ensure_current_executable_cached_in_state(
    state_dir: &Path,
) -> Result<PathBuf, DaemonLifecycleError> {
    let bootstrap = initialize_artifact_bootstrap()?;
    let source = bootstrap.source_path().to_path_buf();
    remove_interrupted_image_publications(state_dir)?;
    if let Some((target, _digest)) = current_cached_daemon_image(state_dir)?
        && target != source
    {
        return Ok(target);
    }
    let temp_parent = daemon_image_dir(state_dir);
    fs::create_dir_all(&temp_parent).map_err(|source| DaemonLifecycleError::Io {
        path: temp_parent.clone(),
        source,
    })?;
    let temp = temp_parent.join(format!(".bcode.tmp-{}", std::process::id()));
    let materialize_started_at = std::time::Instant::now();
    let (digest, cloned) = materialize_verified_daemon_image(bootstrap, &source, &temp)?;
    let target = cached_daemon_executable_path_for_digest(state_dir, &digest);
    if target == source {
        let _ = fs::remove_file(&temp);
        return Ok(target);
    }
    let parent = target
        .parent()
        .expect("content-addressed daemon image has a parent");
    fs::create_dir_all(parent).map_err(|source| DaemonLifecycleError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    if target.exists() {
        if daemon_image_is_valid(&target, &digest)? {
            let _ = fs::remove_file(&temp);
            return Ok(target);
        }
        fs::remove_file(&target).map_err(|source| DaemonLifecycleError::Io {
            path: target.clone(),
            source,
        })?;
        let metadata = daemon_image_metadata_path(&target);
        if let Err(source) = fs::remove_file(&metadata)
            && source.kind() != std::io::ErrorKind::NotFound
        {
            return Err(DaemonLifecycleError::Io {
                path: metadata,
                source,
            });
        }
    }
    match fs::rename(&temp, &target) {
        Ok(()) => {
            write_daemon_image_metadata(&target, &digest)?;
            tracing::debug!(
                target: "bcode_daemon_lifecycle::startup",
                elapsed_ms = materialize_started_at.elapsed().as_millis(),
                cloned,
                "daemon image published"
            );
            Ok(target)
        }
        Err(_source_error) if daemon_image_is_valid(&target, &digest)? => {
            let _ = fs::remove_file(&temp);
            Ok(target)
        }
        Err(source_error) => {
            let _ = fs::remove_file(&temp);
            Err(DaemonLifecycleError::Io {
                path: target,
                source: source_error,
            })
        }
    }
}

#[cfg(unix)]
fn preserve_executable_permissions(
    source: &Path,
    target: &Path,
) -> Result<(), DaemonLifecycleError> {
    let mode = fs::metadata(source)
        .map_err(|source_error| DaemonLifecycleError::Io {
            path: source.to_path_buf(),
            source: source_error,
        })?
        .permissions()
        .mode();
    let mut permissions = fs::metadata(target)
        .map_err(|source_error| DaemonLifecycleError::Io {
            path: target.to_path_buf(),
            source: source_error,
        })?
        .permissions();
    permissions.set_mode(mode | 0o700);
    fs::set_permissions(target, permissions).map_err(|source_error| DaemonLifecycleError::Io {
        path: target.to_path_buf(),
        source: source_error,
    })
}

#[cfg(not(unix))]
fn preserve_executable_permissions(
    _source: &Path,
    _target: &Path,
) -> Result<(), DaemonLifecycleError> {
    Ok(())
}

fn daemon_image_cleanup_lock_path(state_dir: &Path) -> PathBuf {
    state_dir
        .join("daemons")
        .join(DAEMON_IMAGE_CLEANUP_LOCK_FILE)
}

fn open_daemon_image_cleanup_lock(state_dir: &Path) -> Result<fs::File, DaemonLifecycleError> {
    let path = daemon_image_cleanup_lock_path(state_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| DaemonLifecycleError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| DaemonLifecycleError::Io { path, source })
}

struct DaemonImageUseGuard {
    file: fs::File,
}

impl DaemonImageUseGuard {
    fn acquire(state_dir: &Path) -> Result<Self, DaemonLifecycleError> {
        let file = open_daemon_image_cleanup_lock(state_dir)?;
        file.lock_shared()
            .map_err(|source| DaemonLifecycleError::Io {
                path: daemon_image_cleanup_lock_path(state_dir),
                source,
            })?;
        Ok(Self { file })
    }
}

impl Drop for DaemonImageUseGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn try_acquire_daemon_image_cleanup_lock(
    state_dir: &Path,
) -> Result<Option<fs::File>, DaemonLifecycleError> {
    let file = open_daemon_image_cleanup_lock(state_dir)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(source)) => Err(DaemonLifecycleError::Io {
            path: daemon_image_cleanup_lock_path(state_dir),
            source,
        }),
    }
}

fn retained_daemon_image_paths(
    state_dir: &Path,
) -> Result<(std::collections::BTreeSet<PathBuf>, bool), DaemonLifecycleError> {
    let registry = registry_dir(state_dir);
    let entries = match fs::read_dir(&registry) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok((std::collections::BTreeSet::new(), false));
        }
        Err(source) => {
            return Err(DaemonLifecycleError::Io {
                path: registry,
                source,
            });
        }
    };
    let mut retained = std::collections::BTreeSet::new();
    let mut ambiguous_record_evidence = false;
    for entry in entries {
        let entry = entry.map_err(|source| DaemonLifecycleError::Io {
            path: registry.clone(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let contents = fs::read(&path).map_err(|source| DaemonLifecycleError::Io {
            path: path.clone(),
            source,
        })?;
        let Ok(record) = serde_json::from_slice::<DaemonRecord>(&contents) else {
            ambiguous_record_evidence = true;
            continue;
        };
        if !(1..=DAEMON_RECORD_SCHEMA_VERSION).contains(&record.schema_version) {
            ambiguous_record_evidence = true;
            continue;
        }
        if let Some(executable_path) = record.executable_path {
            retained.insert(executable_path);
        }
    }
    Ok((retained, ambiguous_record_evidence))
}

/// Remove cached daemon images that are not referenced by daemon records or the current build.
///
/// Cleanup fails closed when any registry record is malformed or has an unknown schema because
/// that record may contain image-retention evidence this build cannot safely interpret.
///
/// # Errors
///
/// Returns an error when reading registry evidence or removing image directories fails.
pub fn cleanup_stale_daemon_images(state_dir: &Path) -> Result<usize, DaemonLifecycleError> {
    let Some(_cleanup_lock) = try_acquire_daemon_image_cleanup_lock(state_dir)? else {
        return Ok(0);
    };
    let root = state_dir.join("daemon-images");
    let namespace_entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(DaemonLifecycleError::Io { path: root, source }),
    };
    let (mut retained, ambiguous_record_evidence) = retained_daemon_image_paths(state_dir)?;
    if ambiguous_record_evidence {
        return Ok(0);
    }
    retained.insert(current_cached_daemon_executable_path(state_dir)?);
    let mut removed = 0;
    for namespace_entry in namespace_entries.flatten() {
        if !namespace_entry
            .file_type()
            .is_ok_and(|file_type| file_type.is_dir())
        {
            continue;
        }
        let Ok(image_entries) = fs::read_dir(namespace_entry.path()) else {
            continue;
        };
        for image_entry in image_entries.flatten() {
            let path = image_entry.path();
            let candidate = if image_entry
                .file_type()
                .is_ok_and(|file_type| file_type.is_dir())
            {
                path.join(if cfg!(windows) { "bcode.exe" } else { "bcode" })
            } else {
                path.clone()
            };
            if retained.contains(&candidate) {
                continue;
            }
            if image_entry
                .file_type()
                .is_ok_and(|file_type| file_type.is_dir())
            {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            }
            .map_err(|source| DaemonLifecycleError::Io {
                path: path.clone(),
                source,
            })?;
            removed += 1;
        }
        if fs::read_dir(namespace_entry.path()).is_ok_and(|mut entries| entries.next().is_none()) {
            let _ = fs::remove_dir(namespace_entry.path());
        }
    }
    Ok(removed)
}

/// Return Unix time in milliseconds.
///
/// # Errors
///
/// Returns an error when the system clock is before the Unix epoch.
pub fn unix_time_millis() -> Result<u64, DaemonLifecycleError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| DaemonLifecycleError::Clock)?;
    Ok(duration.as_millis().try_into().unwrap_or(u64::MAX))
}

fn endpoint_record(endpoint: &IpcEndpoint) -> DaemonEndpointRecord {
    if let Some(path) = endpoint.as_unix_socket() {
        return DaemonEndpointRecord::UnixSocket {
            path: path.to_path_buf(),
        };
    }
    if let Some(name) = endpoint.as_windows_named_pipe() {
        return DaemonEndpointRecord::WindowsNamedPipe {
            name: name.to_owned(),
        };
    }
    DaemonEndpointRecord::Unknown {
        debug: format!("{endpoint:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_with_writer_epoch(storage_writer_epoch: Option<u32>) -> DaemonRecord {
        DaemonRecord {
            schema_version: DAEMON_RECORD_SCHEMA_VERSION,
            namespace: "test".to_string(),
            protocol_version: u32::from(CURRENT_PROTOCOL_VERSION),
            artifact_id: Some(bcode_ipc::ArtifactId::current()),
            build_fingerprint: "test-build".to_string(),
            storage_writer_epoch,
            pid: Some(std::process::id()),
            endpoint: DaemonEndpointRecord::Unknown {
                debug: "test".to_string(),
            },
            log_path: PathBuf::from("test.log"),
            executable_path: None,
            executable_digest: None,
            started_at_unix_ms: 0,
            last_seen_unix_ms: 0,
            instance_id: "test-instance".to_string(),
        }
    }

    fn stale_image_path(state_dir: &Path, namespace: &str, digest: &str) -> PathBuf {
        state_dir
            .join("daemon-images")
            .join(namespace)
            .join(digest)
            .join(if cfg!(windows) { "bcode.exe" } else { "bcode" })
    }

    fn write_test_image(path: &Path) {
        fs::create_dir_all(path.parent().expect("image parent")).expect("image directory");
        fs::write(path, b"stale daemon image").expect("image");
    }

    #[test]
    fn readiness_identity_uses_contract_epochs_not_executable_digest() {
        let matching = bcode_ipc::DaemonStatus {
            namespace: daemon_namespace(),
            protocol_version: u32::from(CURRENT_PROTOCOL_VERSION),
            artifact_id: Some(bcode_ipc::ArtifactId::current()),
            build_fingerprint: BUILD_FINGERPRINT.to_owned(),
            executable_digest: Some("diagnostic-digest-may-differ".to_owned()),
            storage_writer_epoch: Some(bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
            session_event_schema_version: Some(
                bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            ),
            ..bcode_ipc::DaemonStatus::default()
        };
        assert!(daemon_status_matches_current_executable(&matching));
        assert!(!daemon_status_matches_current_executable(
            &bcode_ipc::DaemonStatus {
                storage_writer_epoch: matching.storage_writer_epoch.map(|epoch| epoch + 1),
                ..matching.clone()
            }
        ));
        assert!(!daemon_status_matches_current_executable(
            &bcode_ipc::DaemonStatus {
                session_event_schema_version: matching
                    .session_event_schema_version
                    .map(|schema| schema + 1),
                ..matching
            }
        ));
    }

    #[test]
    fn daemon_image_cleanup_skips_while_any_artifact_start_uses_images() {
        let state_dir = std::env::temp_dir().join(format!(
            "bcode-daemon-image-use-lock-test-{}-{}",
            std::process::id(),
            unix_time_millis().expect("time")
        ));
        let stale_image = stale_image_path(&state_dir, "historical", "in-use");
        write_test_image(&stale_image);
        let use_guard = DaemonImageUseGuard::acquire(&state_dir).expect("image use guard");

        assert_eq!(cleanup_stale_daemon_images(&state_dir).expect("cleanup"), 0);
        assert!(stale_image.exists());

        drop(use_guard);
        assert_eq!(cleanup_stale_daemon_images(&state_dir).expect("cleanup"), 1);
        assert!(!stale_image.exists());
        fs::remove_dir_all(state_dir).expect("state cleanup");
    }

    #[test]
    fn daemon_image_cleanup_retains_record_references_and_removes_only_unreferenced_images() {
        let state_dir = std::env::temp_dir().join(format!(
            "bcode-daemon-image-cleanup-test-{}-{}",
            std::process::id(),
            unix_time_millis().expect("time")
        ));
        let retained_image = stale_image_path(&state_dir, "historical", "retained");
        let stale_image = stale_image_path(&state_dir, "historical", "stale");
        write_test_image(&retained_image);
        write_test_image(&stale_image);
        let record = DaemonRecord {
            namespace: "historical-cleanup-record".to_owned(),
            executable_path: Some(retained_image.clone()),
            ..record_with_writer_epoch(Some(2))
        };
        let record_path = write_record(&state_dir, &record).expect("record");

        assert_eq!(cleanup_stale_daemon_images(&state_dir).expect("cleanup"), 1);
        assert!(retained_image.exists());
        assert!(!stale_image.exists());

        remove_record_path(&record_path).expect("remove record");
        assert_eq!(cleanup_stale_daemon_images(&state_dir).expect("cleanup"), 1);
        assert!(!retained_image.exists());
        fs::remove_dir_all(state_dir).expect("state cleanup");
    }

    #[test]
    fn daemon_image_cleanup_fails_closed_for_ambiguous_registry_evidence() {
        for (case, contents) in [
            ("malformed", b"not valid json".to_vec()),
            (
                "future-schema",
                serde_json::to_vec(&DaemonRecord {
                    schema_version: DAEMON_RECORD_SCHEMA_VERSION + 1,
                    namespace: "future-record".to_owned(),
                    ..record_with_writer_epoch(Some(2))
                })
                .expect("future record"),
            ),
        ] {
            let state_dir = std::env::temp_dir().join(format!(
                "bcode-daemon-image-ambiguous-{case}-{}-{}",
                std::process::id(),
                unix_time_millis().expect("time")
            ));
            let stale_image = stale_image_path(&state_dir, "historical", "ambiguous");
            write_test_image(&stale_image);
            let registry = registry_dir(&state_dir);
            fs::create_dir_all(&registry).expect("registry");
            fs::write(registry.join("ambiguous.json"), contents).expect("ambiguous record");

            assert_eq!(cleanup_stale_daemon_images(&state_dir).expect("cleanup"), 0);
            assert!(stale_image.exists());
            fs::remove_dir_all(state_dir).expect("state cleanup");
        }
    }

    #[test]
    fn current_process_identity_evidence_rejects_pid_reuse_and_accepts_exact_record() {
        let (executable_path, executable_digest) = current_executable_identity().expect("identity");
        let mut system = System::new();
        let pid = Pid::from_u32(std::process::id());
        system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        let process = system.process(pid).expect("current process");
        let record = DaemonRecord {
            pid: Some(std::process::id()),
            executable_path: Some(executable_path),
            executable_digest: Some(executable_digest),
            started_at_unix_ms: process.start_time().saturating_mul(1_000),
            ..record_with_writer_epoch(Some(2))
        };
        assert_eq!(
            process_identity_evidence(&record),
            ProcessIdentityEvidence::Exact
        );
        assert_eq!(
            process_identity_evidence(&DaemonRecord {
                started_at_unix_ms: record.started_at_unix_ms.saturating_add(1_000),
                ..record.clone()
            }),
            ProcessIdentityEvidence::MissingOrReused
        );
        // The test binary runs from a mutable pathname, so a digest mismatch is ambiguous and
        // must not be reported as positive PID-reuse evidence.
        assert_eq!(
            process_identity_evidence(&DaemonRecord {
                executable_digest: Some("reused-image".to_owned()),
                ..record
            }),
            ProcessIdentityEvidence::Unverifiable
        );
    }

    #[test]
    fn rebuilt_mutable_executable_path_is_ambiguous_and_preserves_the_record() {
        // A rebuilt source path must never be classified as stale while the daemon still answers,
        // so a live daemon spawned from `target/` keeps serving its own artifact after a rebuild.
        let current = DaemonRecord {
            namespace: bcode_ipc::daemon_namespace(),
            ..record_with_writer_epoch(Some(2))
        };
        assert_eq!(
            classify_daemon_record_evidence(
                &current,
                None,
                true,
                ProcessIdentityEvidence::Unverifiable,
            ),
            DaemonRecordClassification::Unverifiable
        );
        assert_eq!(
            classify_daemon_record_evidence(
                &current,
                None,
                false,
                ProcessIdentityEvidence::Unverifiable,
            ),
            DaemonRecordClassification::Unverifiable
        );
    }

    #[test]
    fn stale_plugin_state_cleanup_is_namespace_confined_and_symlink_safe() {
        let state = tempfile::tempdir().expect("state");
        let plugins = state.path().join("derived/plugins");
        let stale_root = plugins.join("ipc-v1-stale");
        let retained = plugins.join("ipc-v1-retained");
        fs::create_dir_all(&stale_root).expect("stale state");
        fs::create_dir_all(&retained).expect("retained state");
        fs::write(stale_root.join("provider.json"), b"derived").expect("provider state");

        remove_stale_plugin_state(state.path(), "ipc-v1-stale").expect("cleanup stale state");
        assert!(!stale_root.exists());
        assert!(retained.exists());
        remove_stale_plugin_state(state.path(), "../ipc-v1-retained")
            .expect("reject hostile namespace");
        assert!(retained.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let external = tempfile::tempdir().expect("external");
            let link = plugins.join("ipc-v1-link");
            symlink(external.path(), &link).expect("namespace symlink");
            remove_stale_plugin_state(state.path(), "ipc-v1-link")
                .expect("symlink cleanup fails closed");
            assert!(link.exists());
            assert!(external.path().exists());
        }
    }

    #[test]
    fn daemon_record_identity_fields_are_independently_validated() {
        let record = record_with_writer_epoch(Some(2));
        let exact = bcode_ipc::DaemonStatus {
            namespace: record.namespace.clone(),
            protocol_version: record.protocol_version,
            artifact_id: record.artifact_id.clone(),
            build_fingerprint: record.build_fingerprint.clone(),
            executable_digest: record.executable_digest.clone(),
            storage_writer_epoch: record.storage_writer_epoch,
            session_event_schema_version: None,
            pid: record.pid,
            instance_id: record.instance_id.clone(),
            started_at_unix_ms: record.started_at_unix_ms,
        };
        assert!(daemon_status_matches_record(&record, &exact));

        let cases = [
            bcode_ipc::DaemonStatus {
                instance_id: "other-instance".to_owned(),
                ..exact.clone()
            },
            bcode_ipc::DaemonStatus {
                namespace: "other-namespace".to_owned(),
                ..exact.clone()
            },
            bcode_ipc::DaemonStatus {
                protocol_version: exact.protocol_version.saturating_add(1),
                ..exact.clone()
            },
            bcode_ipc::DaemonStatus {
                artifact_id: Some(
                    bcode_ipc::ArtifactId::parse("other-artifact")
                        .expect("other artifact identity"),
                ),
                ..exact.clone()
            },
            bcode_ipc::DaemonStatus {
                build_fingerprint: "other-build".to_owned(),
                ..exact.clone()
            },
            bcode_ipc::DaemonStatus {
                executable_digest: Some("other-digest".to_owned()),
                ..exact.clone()
            },
            bcode_ipc::DaemonStatus {
                storage_writer_epoch: exact.storage_writer_epoch.map(|epoch| epoch + 1),
                ..exact.clone()
            },
            bcode_ipc::DaemonStatus {
                pid: exact.pid.map(|pid| pid.saturating_add(1)),
                ..exact.clone()
            },
            bcode_ipc::DaemonStatus {
                started_at_unix_ms: exact.started_at_unix_ms.saturating_add(1),
                ..exact
            },
        ];
        for status in cases {
            assert!(!daemon_status_matches_record(&record, &status));
        }
    }

    #[test]
    fn daemon_record_classification_preserves_historical_and_ambiguous_evidence() {
        let current = record_with_writer_epoch(Some(2));
        let exact = bcode_ipc::DaemonStatus {
            namespace: current.namespace.clone(),
            protocol_version: current.protocol_version,
            artifact_id: current.artifact_id.clone(),
            build_fingerprint: current.build_fingerprint.clone(),
            executable_digest: current.executable_digest.clone(),
            storage_writer_epoch: current.storage_writer_epoch,
            session_event_schema_version: None,
            pid: current.pid,
            instance_id: current.instance_id.clone(),
            started_at_unix_ms: current.started_at_unix_ms,
        };
        assert_eq!(
            classify_daemon_record_evidence(
                &current,
                Some(&exact),
                true,
                ProcessIdentityEvidence::Exact,
            ),
            DaemonRecordClassification::HistoricalExactResponsive
        );
        assert_eq!(
            classify_daemon_record_evidence(&current, None, true, ProcessIdentityEvidence::Exact,),
            DaemonRecordClassification::HistoricalProcessVerifiedProtocolUnsupported
        );
        assert_eq!(
            classify_daemon_record_evidence(
                &current,
                Some(&bcode_ipc::DaemonStatus {
                    instance_id: "replacement".to_owned(),
                    ..exact
                }),
                true,
                ProcessIdentityEvidence::MissingOrReused,
            ),
            DaemonRecordClassification::ResponsiveIdentityMismatch
        );
        assert_eq!(
            classify_daemon_record_evidence(
                &current,
                None,
                false,
                ProcessIdentityEvidence::MissingOrReused,
            ),
            DaemonRecordClassification::UnreachableStale
        );
        assert_eq!(
            classify_daemon_record_evidence(
                &current,
                None,
                false,
                ProcessIdentityEvidence::Unverifiable,
            ),
            DaemonRecordClassification::Unverifiable
        );
    }

    #[test]
    fn storage_writer_compatibility_fails_closed_for_legacy_records() {
        assert!(storage_writer_is_incompatible(
            &record_with_writer_epoch(None),
            2
        ));
        assert!(storage_writer_is_incompatible(
            &record_with_writer_epoch(Some(1)),
            2
        ));
        assert!(!storage_writer_is_incompatible(
            &record_with_writer_epoch(Some(2)),
            2
        ));
    }

    #[test]
    fn daemon_record_accepts_verified_executable_outside_cache() {
        let (executable_path, digest) = current_executable_identity().unwrap();
        let record = DaemonRecord::current(
            &bcode_ipc::default_endpoint(),
            PathBuf::from("test.log"),
            Some(executable_path.clone()),
            "foreground-test".to_string(),
        )
        .unwrap();

        assert_eq!(
            record.executable_path.as_deref(),
            Some(executable_path.as_path())
        );
        assert_eq!(record.executable_digest.as_deref(), Some(digest.as_str()));
    }

    #[test]
    fn cached_daemon_executable_path_is_content_addressed() {
        assert_eq!(
            cached_daemon_executable_path_for_digest(Path::new("/state"), "abc123"),
            Path::new("/state")
                .join("daemon-images")
                .join(bcode_ipc::ArtifactId::current().as_str())
                .join("abc123")
                .join(if cfg!(windows) { "bcode.exe" } else { "bcode" })
        );
    }

    #[cfg(unix)]
    #[test]
    fn retained_artifact_handle_survives_source_path_replacement() {
        let root = std::env::temp_dir().join(format!(
            "bcode-retained-artifact-test-{}-{}",
            std::process::id(),
            unix_time_millis().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("bcode");
        fs::write(&source, b"original artifact bytes").unwrap();
        let bootstrap = ArtifactBootstrap::open(
            source.clone(),
            bcode_ipc::ArtifactId::parse("retained-test").unwrap(),
        )
        .unwrap();
        let original_digest = bootstrap.digest().unwrap();
        assert!(bootstrap.source_path_still_current());
        let replacement = root.join("replacement");
        fs::write(&replacement, b"replacement bytes").unwrap();
        fs::rename(&replacement, &source).unwrap();
        assert!(!bootstrap.source_path_still_current());
        let copied = root.join("copied");
        let copied_digest = bootstrap.copy_to(&copied).unwrap();

        assert_eq!(copied_digest, original_digest);
        assert_eq!(executable_sha256(&copied).unwrap(), original_digest);
        assert_ne!(executable_sha256(&source).unwrap(), original_digest);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_cached_image_is_repaired_with_exact_metadata() {
        let state_dir = std::env::temp_dir().join(format!(
            "bcode-daemon-image-repair-test-{}-{}",
            std::process::id(),
            unix_time_millis().unwrap()
        ));
        let cached = ensure_current_executable_cached_in_state(&state_dir).unwrap();
        fs::write(&cached, b"corrupt").unwrap();

        let repaired = ensure_current_executable_cached_in_state(&state_dir).unwrap();
        let (_source, digest) = current_executable_identity().unwrap();
        assert_eq!(repaired, cached);
        assert!(daemon_image_is_valid(&repaired, &digest).unwrap());
        assert_eq!(
            ensure_current_executable_cached_in_state(&state_dir).unwrap(),
            repaired
        );
        assert!(
            fs::read_dir(repaired.parent().expect("cached image parent"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".bcode.tmp-"))
        );
        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn interrupted_cache_copy_is_removed_before_verified_publication() {
        let state_dir = std::env::temp_dir().join(format!(
            "bcode-daemon-image-interrupted-test-{}-{}",
            std::process::id(),
            unix_time_millis().unwrap()
        ));
        let (_source, digest) = current_executable_identity().unwrap();
        let target = cached_daemon_executable_path_for_digest(&state_dir, &digest);
        let parent = target.parent().expect("cached image parent");
        fs::create_dir_all(parent).unwrap();
        let interrupted = parent.join(format!(".bcode.tmp-{}", std::process::id()));
        fs::write(&interrupted, b"partial executable bytes").unwrap();

        let cached = ensure_current_executable_cached_in_state(&state_dir).unwrap();

        assert_eq!(cached, target);
        assert!(!interrupted.exists());
        assert!(daemon_image_is_valid(&cached, &digest).unwrap());
        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn content_addressed_paths_are_verified() {
        let path = cached_daemon_executable_path_for_digest(Path::new("/state"), "abc123");
        assert!(executable_path_matches_digest(&path, "abc123"));
        assert!(!executable_path_matches_digest(&path, "other"));
        assert!(!executable_path_matches_digest(
            Path::new("/state/daemon-images/legacy/bcode"),
            "abc123"
        ));
    }

    #[test]
    fn caches_current_executable_into_immutable_digest_directory() {
        let state_dir = std::env::temp_dir().join(format!(
            "bcode-daemon-image-test-{}-{}",
            std::process::id(),
            unix_time_millis().unwrap()
        ));

        let cached = ensure_current_executable_cached_in_state(&state_dir).unwrap();
        let (_source, digest) = current_executable_identity().unwrap();

        assert!(cached.exists());
        assert_eq!(
            cached,
            cached_daemon_executable_path_for_digest(&state_dir, &digest)
        );
        assert_eq!(executable_sha256(&cached).unwrap(), digest);
        assert!(daemon_image_is_valid(&cached, &digest).unwrap());
        assert_eq!(
            serde_json::from_slice::<DaemonImageMetadata>(
                &fs::read(daemon_image_metadata_path(&cached)).unwrap()
            )
            .unwrap()
            .artifact_id,
            bcode_ipc::ArtifactId::current()
        );
        assert_eq!(
            ensure_current_executable_cached_in_state(&state_dir).unwrap(),
            cached
        );
        let _ = fs::remove_dir_all(state_dir);
    }
}

/// Options controlling daemon startup orchestration.
#[derive(Debug, Clone)]
pub struct EnsureDaemonOptions {
    /// Endpoint the daemon should serve.
    pub endpoint: IpcEndpoint,
    /// Suppress user-facing status output.
    pub quiet: bool,
    /// Path used for daemon stdout/stderr logs.
    pub log_path: PathBuf,
}

impl EnsureDaemonOptions {
    /// Build default daemon startup options for the current namespace.
    #[must_use]
    pub fn default_for_current_namespace() -> Self {
        Self {
            endpoint: bcode_ipc::default_endpoint(),
            quiet: true,
            log_path: default_daemon_log_path(),
        }
    }
}

/// Error returned when daemon process startup fails.
#[derive(Debug, Error)]
pub enum DaemonStartError {
    /// Daemon lifecycle registry cleanup failed.
    #[error("daemon lifecycle error: {0}")]
    Lifecycle(#[from] DaemonLifecycleError),
    /// Daemon process I/O failed.
    #[error("daemon process I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Daemon did not become ready before the startup timeout.
    #[error(
        "daemon did not become ready after auto-start; log: {log_path}\ntry `bcode server run` to see startup failures in the foreground\n\n{recent_log}"
    )]
    StartTimeout {
        /// Daemon log path.
        log_path: String,
        /// Recent daemon log excerpt.
        recent_log: String,
    },
    /// Daemon process exited before readiness.
    #[error(
        "daemon exited before becoming ready ({status}); log: {log_path}\ntry `bcode server run` to see startup failures in the foreground\n\n{recent_log}"
    )]
    Exited {
        /// Child process exit status.
        status: String,
        /// Daemon log path.
        log_path: String,
        /// Recent daemon log excerpt.
        recent_log: String,
    },
    /// Daemon readiness was transient and failed a follow-up health check.
    #[error(
        "daemon became ready but failed a follow-up health check; log: {log_path}\ntry `bcode server run` to see startup failures in the foreground\n\n{recent_log}"
    )]
    HealthCheckFailed {
        /// Daemon log path.
        log_path: String,
        /// Recent daemon log excerpt.
        recent_log: String,
    },
    /// Another process held the namespace startup lock beyond the bounded startup window.
    #[error("timed out waiting for daemon startup coordination lock at {}", lock_path.display())]
    StartupCoordinationTimeout {
        /// Namespace-scoped startup lock path.
        lock_path: PathBuf,
    },
}

impl DaemonStartError {
    /// Return true when startup likely lost a race to an already-running daemon.
    #[must_use]
    pub fn is_existing_daemon_race(&self) -> bool {
        match self {
            Self::Exited { recent_log, .. }
            | Self::StartTimeout { recent_log, .. }
            | Self::HealthCheckFailed { recent_log, .. } => {
                recent_log.contains("refusing to replace live IPC socket")
                    || recent_log.contains("another bcode daemon is listening")
                    || recent_log.contains("Address already in use")
            }
            Self::Io(error) => error.kind() == std::io::ErrorKind::AddrInUse,
            Self::Lifecycle(_) | Self::StartupCoordinationTimeout { .. } => false,
        }
    }
}

/// Ensure the current namespace daemon is running, starting it when needed.
///
/// # Errors
///
/// Returns an error when stale-record cleanup fails, spawning the daemon fails,
/// or the daemon does not pass bounded readiness checks.
pub async fn ensure_daemon_running(options: &EnsureDaemonOptions) -> Result<(), DaemonStartError> {
    ensure_daemon_running_with_start(options, |options| {
        let endpoint = options.endpoint.clone();
        let log_path = options.log_path.clone();
        async move {
            if let Some(parent) = log_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut log_file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&log_path)?;
            writeln!(log_file, "--- bcode daemon start ---")?;
            let stderr_log = log_file.try_clone()?;

            let spawn_started_at = std::time::Instant::now();
            let cached_exe = ensure_current_executable_cached()?;
            let executable_digest = cached_executable_digest(&cached_exe)
                .ok_or_else(|| DaemonLifecycleError::Io {
                    path: cached_exe.clone(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "cached daemon executable path has no digest component",
                    ),
                })?
                .to_owned();
            // Always spawn from the immutable content-addressed image. Spawning from the
            // bootstrap source path would pin the daemon to a mutable pathname (for example
            // `target/<triple>/release/bcode`), so rebuilding that path would later make the
            // running daemon's own identity unverifiable even though it keeps serving its
            // original artifact correctly.
            let exe = cached_exe;
            tracing::debug!(
                target: "bcode_daemon_lifecycle::startup",
                elapsed_ms = spawn_started_at.elapsed().as_millis(),
                "daemon image ready"
            );
            let (endpoint_env_name, endpoint_env_value) =
                bcode_ipc::endpoint_env_pair(&endpoint)
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
            let mut child = tokio::process::Command::new(exe)
                .args(["server", "run"])
                .env(endpoint_env_name, endpoint_env_value)
                .env(
                    bcode_ipc::BCODE_IPC_ENDPOINT_NAMESPACE_ENV,
                    bcode_ipc::daemon_namespace(),
                )
                .env(BCODE_EXECUTABLE_DIGEST_ENV, executable_digest)
                .env("BCODE_DAEMON_LOG", &log_path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::from(log_file))
                .stderr(std::process::Stdio::from(stderr_log))
                .spawn()?;
            tracing::debug!(
                target: "bcode_daemon_lifecycle::startup",
                elapsed_ms = spawn_started_at.elapsed().as_millis(),
                "daemon child spawned"
            );

            wait_for_server_ready(&endpoint, &mut child, &log_path).await
        }
    })
    .await
}

/// Ensure the current namespace daemon is running using an in-process start callback.
///
/// # Errors
///
/// Returns an error when stale endpoint cleanup fails, the callback fails, or the daemon does not
/// pass bounded readiness checks.
pub async fn ensure_daemon_running_in_process(
    options: &EnsureDaemonOptions,
    mut start: impl FnMut() -> Result<(), DaemonStartError>,
) -> Result<(), DaemonStartError> {
    let log_path = options.log_path.clone();
    ensure_daemon_running_with_start(options, move |options| {
        let result = start();
        let endpoint = options.endpoint.clone();
        let log_path = log_path.clone();
        async move {
            result?;
            wait_for_daemon_ready(&endpoint, &log_path).await
        }
    })
    .await
}

async fn ensure_daemon_running_with_start<F, Fut>(
    options: &EnsureDaemonOptions,
    mut start: F,
) -> Result<(), DaemonStartError>
where
    F: FnMut(&EnsureDaemonOptions) -> Fut,
    Fut: std::future::Future<Output = Result<(), DaemonStartError>>,
{
    if ping_ready(&options.endpoint).await {
        print_daemon_status(options, "server already running");
        return Ok(());
    }

    let Some(lock) = StartupLock::acquire(&options.endpoint).await? else {
        print_daemon_status(options, "server already running");
        return Ok(());
    };
    let image_use_guard = DaemonImageUseGuard::acquire(&bcode_config::default_state_dir())?;
    cleanup_stale_endpoint(&options.endpoint)?;
    if ping_ready(&options.endpoint).await {
        drop(image_use_guard);
        drop(lock);
        print_daemon_status(options, "server already running");
        return Ok(());
    }

    start(options).await?;
    drop(image_use_guard);
    let _cleanup_task = tokio::spawn(async {
        let _ = cleanup_stale_daemon_records().await;
        let _ = cleanup_stale_daemon_images(&bcode_config::default_state_dir());
    });
    drop(lock);
    print_daemon_status(options, "server started");
    Ok(())
}

fn print_daemon_status(options: &EnsureDaemonOptions, status: &str) {
    if !options.quiet {
        println!("{status}");
        println!("namespace: {}", daemon_namespace());
        println!("log: {}", display_from_current_dir(&options.log_path));
    }
}

/// Return the default daemon log path for the current namespace.
#[must_use]
pub fn default_daemon_log_path() -> PathBuf {
    std::env::var_os("BCODE_DAEMON_LOG").map_or_else(
        || {
            bcode_config::default_state_dir()
                .join("logs")
                .join(format!("daemon-{}.log", daemon_namespace()))
        },
        PathBuf::from,
    )
}

#[derive(Debug)]
struct StartupLock {
    file: fs::File,
}

impl StartupLock {
    const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(25);
    const POLL_INTERVAL: Duration = Duration::from_millis(25);

    async fn acquire(endpoint: &IpcEndpoint) -> Result<Option<Self>, DaemonStartError> {
        let path = bcode_config::default_state_dir()
            .join("daemons")
            .join(format!("{}.lock", daemon_namespace()));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let started = std::time::Instant::now();
        loop {
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            match file.try_lock() {
                Ok(()) => {
                    file.set_len(0)?;
                    writeln!(&file, "pid={}", std::process::id())?;
                    file.sync_data()?;
                    return Ok(Some(Self { file }));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    if ping_ready(endpoint).await {
                        return Ok(None);
                    }
                    if started.elapsed() >= Self::ACQUIRE_TIMEOUT {
                        return Err(DaemonStartError::StartupCoordinationTimeout {
                            lock_path: path,
                        });
                    }
                    tokio::time::sleep(Self::POLL_INTERVAL).await;
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    }

    #[cfg(test)]
    async fn acquire_at(path: PathBuf, timeout: Duration) -> Result<Self, DaemonStartError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let started = std::time::Instant::now();
        loop {
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            match file.try_lock() {
                Ok(()) => {
                    file.set_len(0)?;
                    writeln!(&file, "pid={}", std::process::id())?;
                    file.sync_data()?;
                    return Ok(Self { file });
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    if started.elapsed() >= timeout {
                        return Err(DaemonStartError::StartupCoordinationTimeout {
                            lock_path: path,
                        });
                    }
                    tokio::time::sleep(Self::POLL_INTERVAL).await;
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    }
}

impl Drop for StartupLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

const READINESS_TIMEOUT: Duration = Duration::from_secs(20);
const READINESS_RETRY_DELAY: Duration = Duration::from_millis(25);

async fn wait_for_server_ready(
    endpoint: &IpcEndpoint,
    child: &mut tokio::process::Child,
    log_path: &Path,
) -> Result<(), DaemonStartError> {
    let readiness_started_at = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + READINESS_TIMEOUT;
    loop {
        let readiness = ping_ready(endpoint);
        tokio::pin!(readiness);
        tokio::select! {
            biased;
            status = child.wait() => {
                let status = status?;
                let error = DaemonStartError::Exited {
                    status: status.to_string(),
                    log_path: display_from_current_dir(log_path).to_string(),
                    recent_log: recent_log_excerpt(log_path),
                };
                if error.is_existing_daemon_race() && wait_for_existing_daemon(endpoint).await {
                    return Ok(());
                }
                return Err(error);
            }
            ready = &mut readiness => {
                if ready {
                    tracing::debug!(
                        target: "bcode_daemon_lifecycle::startup",
                        elapsed_ms = readiness_started_at.elapsed().as_millis(),
                        "daemon readiness verified"
                    );
                    return Ok(());
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(DaemonStartError::StartTimeout {
                log_path: display_from_current_dir(log_path).to_string(),
                recent_log: recent_log_excerpt(log_path),
            });
        }
        tokio::time::sleep(READINESS_RETRY_DELAY).await;
    }
}

async fn wait_for_daemon_ready(
    endpoint: &IpcEndpoint,
    log_path: &Path,
) -> Result<(), DaemonStartError> {
    let deadline = tokio::time::Instant::now() + READINESS_TIMEOUT;
    loop {
        if ping_ready(endpoint).await {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(DaemonStartError::StartTimeout {
                log_path: display_from_current_dir(log_path).to_string(),
                recent_log: recent_log_excerpt(log_path),
            });
        }
        tokio::time::sleep(READINESS_RETRY_DELAY).await;
    }
}

async fn ping_ready(endpoint: &IpcEndpoint) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_millis(500), async {
            let Some(status) = probe_daemon_status(endpoint).await else {
                return false;
            };
            daemon_status_matches_current_executable(&status)
        })
        .await,
        Ok(true)
    )
}

fn daemon_status_matches_current_executable(status: &bcode_ipc::DaemonStatus) -> bool {
    status.namespace == daemon_namespace()
        && status.protocol_version == u32::from(CURRENT_PROTOCOL_VERSION)
        && status.artifact_id.as_ref() == Some(&bcode_ipc::ArtifactId::current())
        && status.build_fingerprint == BUILD_FINGERPRINT
        && status.storage_writer_epoch == Some(bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH)
        && status.session_event_schema_version
            == Some(bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION)
}

async fn wait_for_existing_daemon(endpoint: &IpcEndpoint) -> bool {
    for delay in [50, 100, 200, 400, 800, 1_000] {
        if ping_ready(endpoint).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    false
}

fn remove_stale_plugin_state(
    state_dir: &Path,
    namespace: &str,
) -> Result<(), DaemonLifecycleError> {
    if namespace.is_empty()
        || namespace.len() > 256
        || !namespace
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Ok(());
    }
    let plugins_root = state_dir.join("derived").join("plugins");
    let namespace_root = plugins_root.join(namespace);
    let metadata = match fs::symlink_metadata(&namespace_root) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(DaemonLifecycleError::Io {
                path: namespace_root,
                source,
            });
        }
    };
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Ok(());
    }
    fs::remove_dir_all(&namespace_root).map_err(|source| DaemonLifecycleError::Io {
        path: namespace_root.clone(),
        source,
    })?;
    if fs::read_dir(&plugins_root).is_ok_and(|mut entries| entries.next().is_none()) {
        let _ = fs::remove_dir(&plugins_root);
    }
    Ok(())
}

async fn cleanup_stale_daemon_records() -> Result<(), DaemonLifecycleError> {
    let state_dir = bcode_config::default_state_dir();
    for (path, record) in read_records(&state_dir) {
        if record.is_current_namespace() {
            continue;
        }
        if !matches!(
            classify_daemon_record(&record).await,
            DaemonRecordClassification::UnreachableStale
        ) {
            continue;
        }
        remove_stale_plugin_state(&state_dir, &record.namespace)?;
        remove_record_path(&path)?;
        remove_stale_socket(&record);
    }
    Ok(())
}

#[cfg(unix)]
fn remove_stale_socket(record: &DaemonRecord) {
    if let DaemonEndpointRecord::UnixSocket { path } = &record.endpoint {
        let _ = remove_stale_unix_socket_path(path);
    }
}

#[cfg(not(unix))]
const fn remove_stale_socket(_record: &DaemonRecord) {}

#[cfg(unix)]
fn cleanup_stale_endpoint(endpoint: &IpcEndpoint) -> Result<(), DaemonLifecycleError> {
    if let Some(path) = endpoint.as_unix_socket() {
        remove_stale_unix_socket_path(path)?;
    }
    Ok(())
}

#[cfg(not(unix))]
const fn cleanup_stale_endpoint(_endpoint: &IpcEndpoint) -> Result<(), DaemonLifecycleError> {
    Ok(())
}

#[cfg(unix)]
fn remove_stale_unix_socket_path(path: &Path) -> Result<(), DaemonLifecycleError> {
    if !path.exists() || !is_bcode_socket_path(path) || unix_socket_has_listener(path) {
        return Ok(());
    }
    std::thread::sleep(Duration::from_millis(100));
    if unix_socket_has_listener(path) {
        return Ok(());
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DaemonLifecycleError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(unix)]
fn is_bcode_socket_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with("bcode-")
                && Path::new(name)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("sock"))
        })
}

#[cfg(unix)]
fn unix_socket_has_listener(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

#[cfg(test)]
mod startup_lock_tests {
    use super::*;

    fn lock_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "bcode-daemon-startup-lock-{name}-{}-{}",
            std::process::id(),
            unix_time_millis().expect("time")
        ))
    }

    #[cfg(unix)]
    #[test]
    fn stale_socket_cleanup_preserves_live_listener_then_removes_stale_path() {
        let root = PathBuf::from(format!(
            "/tmp/bcode-ds-{}-{}",
            std::process::id(),
            unix_time_millis().expect("time")
        ));
        fs::create_dir_all(&root).expect("socket directory");
        let live_path = root.join("bcode-foreign-artifact.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&live_path).expect("bind live listener");

        remove_stale_unix_socket_path(&live_path).expect("preserve live endpoint");
        assert!(live_path.exists());
        let connection = std::os::unix::net::UnixStream::connect(&live_path)
            .expect("live foreign endpoint remains reachable");
        drop(connection);
        drop(listener);

        let stale_path = root.join("bcode-stale-artifact.sock");
        drop(std::os::unix::net::UnixDatagram::bind(&stale_path).expect("bind stale socket"));
        remove_stale_unix_socket_path(&stale_path).expect("remove stale endpoint");
        assert!(!stale_path.exists());
        fs::remove_file(live_path).expect("live socket cleanup");
        fs::remove_dir_all(root).expect("socket directory cleanup");
    }

    #[tokio::test]
    async fn startup_lock_waits_for_owner_instead_of_stealing_lock_file() {
        let path = lock_path("wait");
        let first = StartupLock::acquire_at(path.clone(), Duration::from_secs(1))
            .await
            .expect("first lock");
        let waiter_path = path.clone();
        let waiter = tokio::spawn(async move {
            StartupLock::acquire_at(waiter_path, Duration::from_secs(1)).await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiter.is_finished());
        drop(first);
        let second = waiter.await.expect("waiter task").expect("second lock");
        drop(second);
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn startup_lock_crash_helper() {
        let Some(path) = std::env::var_os("BCODE_STARTUP_LOCK_CRASH_PATH") else {
            return;
        };
        let marker =
            std::env::var_os("BCODE_STARTUP_LOCK_CRASH_MARKER").expect("crash helper marker path");
        let _lock = StartupLock::acquire_at(PathBuf::from(path), Duration::from_secs(2))
            .await
            .expect("crash helper lock");
        fs::write(marker, b"locked").expect("write crash helper marker");
        tokio::time::sleep(Duration::from_millis(250)).await;
        std::process::exit(70);
    }

    #[tokio::test]
    async fn startup_lock_owner_crash_permits_bounded_takeover() {
        let path = lock_path("crash");
        let marker = path.with_extension("locked");
        let mut child = tokio::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["startup_lock_crash_helper", "--nocapture"])
            .env("BCODE_STARTUP_LOCK_CRASH_PATH", &path)
            .env("BCODE_STARTUP_LOCK_CRASH_MARKER", &marker)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn crash helper");

        let marker_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !marker.exists() {
            assert!(
                tokio::time::Instant::now() < marker_deadline,
                "crash helper did not acquire startup lock"
            );
            assert!(
                child.try_wait().expect("query crash helper").is_none(),
                "crash helper exited before acquiring startup lock"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let replacement = StartupLock::acquire_at(path.clone(), Duration::from_secs(2))
            .await
            .expect("take over lock after owner crash");
        let status = child.wait().await.expect("wait for crash helper");
        assert!(!status.success());
        drop(replacement);
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(marker);
    }

    #[test]
    fn startup_errors_preserve_actionable_log_context() {
        let log_path = PathBuf::from("/tmp/bcode-daemon-test.log");
        let recent_log = "fatal startup detail\n".to_owned();
        let exited = DaemonStartError::Exited {
            status: "exit status: 70".to_owned(),
            log_path: log_path.display().to_string(),
            recent_log: recent_log.clone(),
        };
        let timeout = DaemonStartError::StartTimeout {
            log_path: log_path.display().to_string(),
            recent_log,
        };

        for error in [exited, timeout] {
            let message = error.to_string();
            assert!(message.contains("/tmp/bcode-daemon-test.log"));
            assert!(message.contains("fatal startup detail"));
            assert!(message.contains("bcode server run"));
        }
    }

    #[test]
    fn recent_log_excerpt_is_bounded_to_latest_context() {
        let path = lock_path("recent-log");
        let contents = (0..40)
            .map(|line| format!("line-{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, contents).expect("write test log");

        let excerpt = recent_log_excerpt(&path);

        assert!(!excerpt.contains("line-0\n"));
        assert!(excerpt.starts_with("line-10\n"));
        assert!(excerpt.ends_with("line-39\n"));
        fs::remove_file(path).expect("remove test log");
    }

    #[tokio::test]
    async fn startup_lock_times_out_without_removing_active_owner() {
        let path = lock_path("timeout");
        let first = StartupLock::acquire_at(path.clone(), Duration::from_secs(1))
            .await
            .expect("first lock");
        let error = StartupLock::acquire_at(path.clone(), Duration::from_millis(50))
            .await
            .expect_err("active owner must not be replaced");
        assert!(matches!(
            error,
            DaemonStartError::StartupCoordinationTimeout { lock_path } if lock_path == path
        ));
        assert!(path.exists());
        drop(first);
        let _ = fs::remove_file(path);
    }
}

fn recent_log_excerpt(log_path: &Path) -> String {
    let Ok(contents) = fs::read_to_string(log_path) else {
        return "daemon log could not be read".to_string();
    };
    let lines = contents.lines().rev().take(30).collect::<Vec<_>>();
    if lines.is_empty() {
        return "daemon log is empty".to_string();
    }
    let mut excerpt = lines.into_iter().rev().collect::<Vec<_>>().join("\n");
    if !excerpt.ends_with('\n') {
        excerpt.push('\n');
    }
    excerpt
}
