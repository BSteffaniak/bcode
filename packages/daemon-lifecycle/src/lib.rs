#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Daemon lifecycle registry and cleanup models.

use bcode_ipc::{BUILD_FINGERPRINT, CURRENT_PROTOCOL_VERSION, IpcEndpoint, daemon_namespace};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Current daemon registry record schema version.
pub const DAEMON_RECORD_SCHEMA_VERSION: u32 = 1;

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
    /// Process identifier, when available.
    pub pid: Option<u32>,
    /// IPC endpoint for this daemon.
    pub endpoint: DaemonEndpointRecord,
    /// Daemon log path.
    pub log_path: PathBuf,
    /// Executable path used to start the daemon.
    pub executable_path: Option<PathBuf>,
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
        Ok(Self {
            schema_version: DAEMON_RECORD_SCHEMA_VERSION,
            namespace: daemon_namespace(),
            protocol_version: u32::from(CURRENT_PROTOCOL_VERSION),
            build_fingerprint: BUILD_FINGERPRINT.to_string(),
            pid: Some(std::process::id()),
            endpoint: endpoint_record(endpoint),
            log_path,
            executable_path,
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

    #[test]
    fn registry_path_uses_namespace_json() {
        assert_eq!(
            record_path(Path::new("/state"), "ipc-v1-test"),
            PathBuf::from("/state/daemons/ipc-v1-test.json")
        );
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
            Self::Lifecycle(_) => false,
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
    if ping_ready(&options.endpoint).await {
        if !options.quiet {
            println!("server already running");
            println!("namespace: {}", daemon_namespace());
            println!("log: {}", options.log_path.display());
        }
        return Ok(());
    }

    let mut startup_attempts = 0;
    loop {
        startup_attempts += 1;
        let lock = StartupLock::acquire()?;
        cleanup_stale_endpoint(&options.endpoint)?;
        if ping_ready(&options.endpoint).await {
            drop(lock);
            if !options.quiet {
                println!("server already running");
                println!("namespace: {}", daemon_namespace());
                println!("log: {}", options.log_path.display());
            }
            return Ok(());
        }

        if let Some(parent) = options.log_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut log_file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&options.log_path)?;
        writeln!(log_file, "--- bcode daemon start ---")?;
        let stderr_log = log_file.try_clone()?;

        let exe = std::env::current_exe()?;
        let (endpoint_env_name, endpoint_env_value) =
            bcode_ipc::endpoint_env_pair(&options.endpoint)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
        let mut child = tokio::process::Command::new(exe)
            .args(["server", "run"])
            .env(endpoint_env_name, endpoint_env_value)
            .env("BCODE_DAEMON_LOG", &options.log_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(stderr_log))
            .spawn()?;

        match wait_for_server_ready(&options.endpoint, &mut child, &options.log_path).await {
            Ok(()) => {
                let _cleanup_task = tokio::spawn(async {
                    let _ = cleanup_stale_daemon_records().await;
                });
                drop(lock);
                if !options.quiet {
                    println!("server started");
                    println!("namespace: {}", daemon_namespace());
                    println!("log: {}", options.log_path.display());
                }
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
    path: PathBuf,
}

impl StartupLock {
    fn acquire() -> Result<Self, DaemonStartError> {
        let path = bcode_config::default_state_dir()
            .join("daemons")
            .join(format!("{}.lock", daemon_namespace()));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        for _ in 0..20 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    writeln!(file, "pid={}", std::process::id())?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
        let _ = fs::remove_file(&path);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                writeln!(file, "pid={}", std::process::id())?;
                Ok(Self { path })
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for StartupLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

async fn wait_for_server_ready(
    endpoint: &IpcEndpoint,
    child: &mut tokio::process::Child,
    log_path: &Path,
) -> Result<(), DaemonStartError> {
    for _ in 0..50 {
        if ping_ready(endpoint).await {
            if let Some(status) = child.try_wait()? {
                return Err(DaemonStartError::Exited {
                    status: status.to_string(),
                    log_path: log_path.display().to_string(),
                    recent_log: recent_log_excerpt(log_path),
                });
            }
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            let error = DaemonStartError::Exited {
                status: status.to_string(),
                log_path: log_path.display().to_string(),
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
        log_path: log_path.display().to_string(),
        recent_log: recent_log_excerpt(log_path),
    })
}

async fn ping_ready(endpoint: &IpcEndpoint) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_millis(500), ping_once(endpoint)).await,
        Ok(Ok(()))
    )
}

async fn probe_daemon_ready(endpoint: &IpcEndpoint) -> bool {
    for delay in [25, 50, 100, 200, 400] {
        if ping_ready(endpoint).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    false
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

async fn ping_once(endpoint: &IpcEndpoint) -> Result<(), bcode_ipc::CodecError> {
    let mut stream =
        bcode_ipc::LocalIpcStream::connect(endpoint)
            .await
            .map_err(|error| match error {
                bcode_ipc::IpcTransportError::Io(error) => bcode_ipc::CodecError::Io(error),
                other => bcode_ipc::CodecError::Io(std::io::Error::other(other.to_string())),
            })?;
    let envelope = bcode_ipc::request_envelope(1, &bcode_ipc::Request::Ping)?;
    bcode_ipc::send_envelope(&mut stream, &envelope).await?;
    loop {
        let envelope = bcode_ipc::recv_envelope(&mut stream).await?;
        if envelope.kind != bcode_ipc::EnvelopeKind::Response || envelope.request_id != 1 {
            continue;
        }
        let response = bcode_ipc::decode_response(&envelope.payload)?;
        return match response {
            bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Pong) => Ok(()),
            _ => Err(bcode_ipc::CodecError::Io(std::io::Error::other(
                "unexpected ping response",
            ))),
        };
    }
}

async fn cleanup_stale_daemon_records() -> Result<(), DaemonLifecycleError> {
    let state_dir = bcode_config::default_state_dir();
    for (path, record) in read_records(&state_dir) {
        if record.is_current_namespace() {
            continue;
        }
        let Some(endpoint) = record.endpoint.to_ipc_endpoint() else {
            continue;
        };
        if probe_daemon_ready(&endpoint).await {
            continue;
        }
        remove_record_path(&path)?;
        remove_stale_socket(&record);
    }
    Ok(())
}

fn remove_stale_socket(record: &DaemonRecord) {
    #[cfg(unix)]
    if let DaemonEndpointRecord::UnixSocket { path } = &record.endpoint {
        let _ = remove_stale_unix_socket_path(path);
    }
}

fn cleanup_stale_endpoint(endpoint: &IpcEndpoint) -> Result<(), DaemonLifecycleError> {
    #[cfg(unix)]
    if let Some(path) = endpoint.as_unix_socket() {
        remove_stale_unix_socket_path(path)?;
    }
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
