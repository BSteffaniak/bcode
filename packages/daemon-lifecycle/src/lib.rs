#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Daemon lifecycle registry and cleanup models.

use bcode_ipc::{BUILD_FINGERPRINT, CURRENT_PROTOCOL_VERSION, IpcEndpoint, daemon_namespace};
use bcode_plugin_sdk::path::display_from_current_dir;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::fs;
use std::io::{Read as _, Write as _};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::{Pid, ProcessesToUpdate, System};
use thiserror::Error;

/// Current daemon registry record schema version.
pub const DAEMON_RECORD_SCHEMA_VERSION: u32 = 2;

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
    /// Build fingerprint included in the namespace.
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
        let now = unix_time_millis()?;
        let executable_digest = executable_path
            .as_deref()
            .map(executable_sha256)
            .transpose()?;
        Ok(Self {
            schema_version: DAEMON_RECORD_SCHEMA_VERSION,
            namespace: daemon_namespace(),
            protocol_version: u32::from(CURRENT_PROTOCOL_VERSION),
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
        2,
        &bcode_ipc::Request::ServerStatus {
            working_directory: None,
        },
    )
    .ok()?;
    bcode_ipc::send_envelope(&mut stream, &envelope)
        .await
        .ok()?;
    loop {
        let envelope = bcode_ipc::recv_envelope(&mut stream).await.ok()?;
        if envelope.kind != bcode_ipc::EnvelopeKind::Response || envelope.request_id != 2 {
            continue;
        }
        return match bcode_ipc::decode_response(&envelope.payload).ok()? {
            bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::ServerStatus { status }) => {
                Some(status.daemon)
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
    match executable_sha256(executable) {
        Ok(actual_digest) if actual_digest == expected_digest => ProcessIdentityEvidence::Exact,
        Ok(_) => ProcessIdentityEvidence::MissingOrReused,
        Err(_) => ProcessIdentityEvidence::Unverifiable,
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

/// Return the directory that stores cached daemon executables for this namespace.
#[must_use]
pub fn daemon_image_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("daemon-images").join(daemon_namespace())
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
    let source = std::env::current_exe().map_err(|source| DaemonLifecycleError::Io {
        path: PathBuf::from("<current_exe>"),
        source,
    })?;
    let digest = executable_sha256(&source)?;
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
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file
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

/// Return the current process executable and its byte digest.
///
/// # Errors
///
/// Returns an error when the executable cannot be located or read.
pub fn current_executable_identity() -> Result<(PathBuf, String), DaemonLifecycleError> {
    static IDENTITY: OnceLock<(PathBuf, String)> = OnceLock::new();
    if let Some(identity) = IDENTITY.get() {
        return Ok(identity.clone());
    }
    let path = std::env::current_exe().map_err(|source| DaemonLifecycleError::Io {
        path: PathBuf::from("<current_exe>"),
        source,
    })?;
    let digest = executable_sha256(&path)?;
    let identity = (path, digest);
    let _ = IDENTITY.set(identity.clone());
    Ok(identity)
}

/// Verify that an executable digest agrees with its content-addressed cache path.
#[must_use]
pub fn executable_path_matches_digest(path: &Path, executable_digest: &str) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .and_then(|component| component.to_str())
        == Some(executable_digest)
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

fn ensure_current_executable_cached_in_state(
    state_dir: &Path,
) -> Result<PathBuf, DaemonLifecycleError> {
    let (source, digest) = current_executable_identity()?;
    let target = cached_daemon_executable_path_for_digest(state_dir, &digest);
    if target == source {
        return Ok(target);
    }
    if target.exists() {
        let cached_digest = executable_sha256(&target)?;
        if cached_digest == digest {
            return Ok(target);
        }
        return Err(DaemonLifecycleError::Io {
            path: target,
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "content-addressed daemon image digest mismatch",
            ),
        });
    }
    let parent = target
        .parent()
        .expect("content-addressed daemon image has a parent");
    fs::create_dir_all(parent).map_err(|source| DaemonLifecycleError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let temp = parent.join(format!(
        ".bcode.tmp-{}-{}",
        std::process::id(),
        next_daemon_image_temp_id()
    ));
    fs::copy(&source, &temp).map_err(|source_error| DaemonLifecycleError::Io {
        path: temp.clone(),
        source: source_error,
    })?;
    preserve_executable_permissions(&source, &temp)?;
    let copied_digest = executable_sha256(&temp)?;
    if copied_digest != digest {
        let _ = fs::remove_file(&temp);
        return Err(DaemonLifecycleError::Io {
            path: temp,
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "copied daemon image failed digest verification",
            ),
        });
    }
    match fs::rename(&temp, &target) {
        Ok(()) => Ok(target),
        Err(_source_error) if target.exists() && executable_sha256(&target)? == digest => {
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

fn next_daemon_image_temp_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
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

/// Remove cached daemon images that are not referenced by daemon records or the current build.
///
/// # Errors
///
/// Returns an error when reading or removing image directories fails.
pub fn cleanup_stale_daemon_images(state_dir: &Path) -> Result<usize, DaemonLifecycleError> {
    let root = state_dir.join("daemon-images");
    let Ok(namespace_entries) = fs::read_dir(&root) else {
        return Ok(0);
    };
    let mut retained = read_records(state_dir)
        .into_iter()
        .filter_map(|(_path, record)| record.executable_path)
        .collect::<std::collections::BTreeSet<_>>();
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
        assert_eq!(
            process_identity_evidence(&DaemonRecord {
                executable_digest: Some("reused-image".to_owned()),
                ..record
            }),
            ProcessIdentityEvidence::MissingOrReused
        );
    }

    #[test]
    fn daemon_record_classification_preserves_historical_and_ambiguous_evidence() {
        let current = record_with_writer_epoch(Some(2));
        let exact = bcode_ipc::DaemonStatus {
            namespace: current.namespace.clone(),
            protocol_version: current.protocol_version,
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
                .join(daemon_namespace())
                .join("abc123")
                .join(if cfg!(windows) { "bcode.exe" } else { "bcode" })
        );
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
    fn daemon_image_temporary_names_are_unique_within_a_process() {
        let first = next_daemon_image_temp_id();
        let second = next_daemon_image_temp_id();
        assert_ne!(first, second);
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

            let exe = ensure_current_executable_cached()?;
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
                .env("BCODE_DAEMON_LOG", &log_path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::from(log_file))
                .stderr(std::process::Stdio::from(stderr_log))
                .spawn()?;

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

    let mut startup_attempts = 0;
    loop {
        startup_attempts += 1;
        let lock = StartupLock::acquire().await?;
        cleanup_stale_endpoint(&options.endpoint)?;
        if ping_ready(&options.endpoint).await {
            drop(lock);
            print_daemon_status(options, "server already running");
            return Ok(());
        }

        match start(options).await {
            Ok(()) => {
                let _cleanup_task = tokio::spawn(async {
                    let _ = cleanup_stale_daemon_records().await;
                    let _ = cleanup_stale_daemon_images(&bcode_config::default_state_dir());
                });
                drop(lock);
                print_daemon_status(options, "server started");
                return Ok(());
            }
            Err(error) if error.is_existing_daemon_race() && startup_attempts < 3 => {
                drop(lock);
                if wait_for_existing_daemon(&options.endpoint).await {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(error) if error.is_existing_daemon_race() => {
                if wait_for_existing_daemon(&options.endpoint).await {
                    drop(lock);
                    return Ok(());
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
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

    async fn acquire() -> Result<Self, DaemonStartError> {
        Self::acquire_at(
            bcode_config::default_state_dir()
                .join("daemons")
                .join(format!("{}.lock", daemon_namespace())),
            Self::ACQUIRE_TIMEOUT,
        )
        .await
    }

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

async fn wait_for_server_ready(
    endpoint: &IpcEndpoint,
    child: &mut tokio::process::Child,
    log_path: &Path,
) -> Result<(), DaemonStartError> {
    for _ in 0..200 {
        if ping_ready(endpoint).await {
            if let Some(status) = child.try_wait()? {
                return Err(DaemonStartError::Exited {
                    status: status.to_string(),
                    log_path: display_from_current_dir(log_path).to_string(),
                    recent_log: recent_log_excerpt(log_path),
                });
            }
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
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
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Err(DaemonStartError::StartTimeout {
        log_path: display_from_current_dir(log_path).to_string(),
        recent_log: recent_log_excerpt(log_path),
    })
}

async fn wait_for_daemon_ready(
    endpoint: &IpcEndpoint,
    log_path: &Path,
) -> Result<(), DaemonStartError> {
    for _ in 0..200 {
        if ping_ready(endpoint).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Err(DaemonStartError::StartTimeout {
        log_path: display_from_current_dir(log_path).to_string(),
        recent_log: recent_log_excerpt(log_path),
    })
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
    if status.namespace != daemon_namespace()
        || status.protocol_version != u32::from(CURRENT_PROTOCOL_VERSION)
        || status.build_fingerprint != BUILD_FINGERPRINT
    {
        return false;
    }
    current_executable_identity()
        .is_ok_and(|(_path, digest)| status.executable_digest.as_deref() == Some(digest.as_str()))
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
    if !is_bcode_socket_path(path) || unix_socket_has_listener(path) {
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
