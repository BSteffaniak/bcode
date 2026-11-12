//! The installed Bcode executable, for apps built separately from Bcode.
//!
//! An app that is not the Bcode executable cannot serve as a daemon, and its own artifact
//! identity names no daemon. It reaches the user's Bcode by locating the installed executable,
//! verifying it with [`crate::DaemonTarget::verify`], and starting that executable's daemon
//! with its own lifecycle command when none is running.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;

/// Environment variable naming the Bcode executable an app should use instead of the first
/// `bcode` on `PATH`.
pub const BCODE_BIN_ENV: &str = "BCODE_BIN";

/// Longest stderr excerpt kept from a failed start.
const START_ERROR_LIMIT: u64 = 4096;

/// The installed Bcode executable: [`BCODE_BIN_ENV`] when set, else the first `bcode` on
/// `PATH`. The result is canonical.
#[must_use]
pub fn installed_bcode_executable() -> Option<PathBuf> {
    installed_bcode_executable_from(
        std::env::var_os(BCODE_BIN_ENV).as_deref(),
        std::env::var_os("PATH").as_deref(),
    )
}

/// [`installed_bcode_executable`] over explicit values of [`BCODE_BIN_ENV`] and `PATH`.
///
/// A set but empty override is ignored. A set override that is not an executable file selects
/// nothing rather than falling back to `PATH`, so a mistyped override is never silently
/// replaced by another Bcode.
#[must_use]
pub fn installed_bcode_executable_from(
    bcode_bin: Option<&OsStr>,
    path: Option<&OsStr>,
) -> Option<PathBuf> {
    if let Some(selected) = bcode_bin.filter(|value| !value.is_empty()) {
        return executable_file(Path::new(selected));
    }
    let name = if cfg!(windows) { "bcode.exe" } else { "bcode" };
    std::env::split_paths(path?)
        .filter(|directory| directory.is_absolute())
        .find_map(|directory| executable_file(&directory.join(name)))
}

fn executable_file(path: &Path) -> Option<PathBuf> {
    let path = path.canonicalize().ok()?;
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    Some(path)
}

/// Failure to start the installed executable's daemon.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InstalledDaemonStartError {
    /// The executable could not be run.
    #[error("could not run {executable}: {message}")]
    Spawn {
        /// The executable.
        executable: PathBuf,
        /// Why it could not run.
        message: String,
    },
    /// The start command did not finish in time.
    #[error("starting the daemon took longer than {0:?}")]
    Timeout(Duration),
    /// The start command reported failure.
    #[error("starting the daemon failed: {0}")]
    Failed(String),
}

/// Start `executable`'s daemon with its lifecycle command, `server start`.
///
/// This is how Bcode starts its own daemon. Waits at most `timeout` for the command to finish.
/// The daemon inherits this process's environment, so it serves the same state and
/// configuration scope.
///
/// # Errors
///
/// * [`InstalledDaemonStartError::Spawn`] when the executable cannot run.
/// * [`InstalledDaemonStartError::Timeout`] when the command does not finish in `timeout`.
/// * [`InstalledDaemonStartError::Failed`] when it exits unsuccessfully; carries its error
///   output.
pub async fn start_installed_daemon(
    executable: &Path,
    timeout: Duration,
) -> Result<(), InstalledDaemonStartError> {
    let mut child = tokio::process::Command::new(executable)
        .args(["server", "start"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| InstalledDaemonStartError::Spawn {
            executable: executable.to_path_buf(),
            message: error.to_string(),
        })?;
    let stderr = child.stderr.take();
    let outcome = tokio::time::timeout(timeout, async {
        let mut output = Vec::new();
        if let Some(stderr) = stderr {
            let _ = stderr
                .take(START_ERROR_LIMIT)
                .read_to_end(&mut output)
                .await;
        }
        (child.wait().await, output)
    })
    .await
    .map_err(|_| InstalledDaemonStartError::Timeout(timeout))?;
    match outcome {
        (Ok(status), _) if status.success() => Ok(()),
        (Ok(status), output) => {
            let message = String::from_utf8_lossy(&output).trim().to_owned();
            Err(InstalledDaemonStartError::Failed(if message.is_empty() {
                status.to_string()
            } else {
                message
            }))
        }
        (Err(error), _) => Err(InstalledDaemonStartError::Failed(error.to_string())),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn script(directory: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = directory.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn override_wins_over_path_and_path_is_searched_in_order() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        let chosen = root.path().join("chosen");
        for directory in [&first, &second, &chosen] {
            std::fs::create_dir_all(directory).unwrap();
        }
        // A non-executable `bcode` earlier on the path is skipped.
        std::fs::write(first.join("bcode"), b"not executable").unwrap();
        let on_path = script(&second, "bcode", "exit 0");
        let selected = script(&chosen, "my-bcode", "exit 0");
        let path = std::env::join_paths([&first, &second]).unwrap();

        assert_eq!(
            installed_bcode_executable_from(None, Some(&path)),
            Some(on_path.canonicalize().unwrap())
        );
        assert_eq!(
            installed_bcode_executable_from(Some(selected.as_os_str()), Some(&path)),
            Some(selected.canonicalize().unwrap())
        );
        // An empty override is unset; a broken one selects nothing.
        assert_eq!(
            installed_bcode_executable_from(Some(OsStr::new("")), Some(&path)),
            Some(on_path.canonicalize().unwrap())
        );
        assert_eq!(
            installed_bcode_executable_from(
                Some(root.path().join("missing").as_os_str()),
                Some(&path)
            ),
            None
        );
        assert_eq!(installed_bcode_executable_from(None, None), None);
    }

    #[tokio::test]
    async fn start_runs_the_lifecycle_command_and_reports_its_failure() {
        let root = tempfile::tempdir().unwrap();
        let record = root.path().join("args");
        let ok = script(
            root.path(),
            "ok-bcode",
            &format!("echo \"$@\" > '{}'", record.display()),
        );
        start_installed_daemon(&ok, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&record).unwrap().trim(),
            "server start"
        );

        let failing = script(root.path(), "bad-bcode", "echo 'no state dir' >&2; exit 3");
        assert_eq!(
            start_installed_daemon(&failing, Duration::from_secs(10)).await,
            Err(InstalledDaemonStartError::Failed("no state dir".to_owned()))
        );

        let slow = script(root.path(), "slow-bcode", "sleep 5");
        assert_eq!(
            start_installed_daemon(&slow, Duration::from_millis(100)).await,
            Err(InstalledDaemonStartError::Timeout(Duration::from_millis(
                100
            )))
        );
    }
}
