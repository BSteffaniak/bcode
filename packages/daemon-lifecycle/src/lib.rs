#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Daemon lifecycle registry and cleanup models.

use bcode_ipc::{BUILD_FINGERPRINT, CURRENT_PROTOCOL_VERSION, IpcEndpoint, daemon_namespace};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
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
    let debug = format!("{endpoint:?}");
    #[cfg(unix)]
    if let Some(path) = parse_unix_socket_debug(&debug) {
        return DaemonEndpointRecord::UnixSocket { path };
    }
    #[cfg(windows)]
    if let Some(name) = parse_windows_pipe_debug(&debug) {
        return DaemonEndpointRecord::WindowsNamedPipe { name };
    }
    DaemonEndpointRecord::Unknown { debug }
}

#[cfg(unix)]
fn parse_unix_socket_debug(debug: &str) -> Option<PathBuf> {
    let marker = "UnixSocket(";
    let start = debug.find(marker)? + marker.len();
    let rest = &debug[start..];
    let end = rest.rfind(')')?;
    let path = rest[..end].trim().trim_matches('"');
    (!path.is_empty()).then(|| PathBuf::from(path))
}

#[cfg(windows)]
fn parse_windows_pipe_debug(debug: &str) -> Option<String> {
    let marker = "WindowsNamedPipe(";
    let start = debug.find(marker)? + marker.len();
    let rest = &debug[start..];
    let end = rest.rfind(')')?;
    let name = rest[..end].trim().trim_matches('"');
    (!name.is_empty()).then(|| name.to_string())
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
