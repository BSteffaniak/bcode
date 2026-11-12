//! Connecting an app built separately from Bcode to the user's installed Bcode.
//!
//! [`BcodeClient::default_endpoint`] addresses the daemon of the running executable's own
//! artifact, which is right for the Bcode executable and wrong for any other app: its artifact
//! names no daemon. [`BcodeClient::for_installed_bcode`] locates the installed executable
//! ([`bcode_daemon_lifecycle::installed_bcode_executable`]), verifies its running daemon with
//! [`bcode_daemon_lifecycle::DaemonTarget::verify`] in this process's state and configuration
//! scope, starts that daemon with the executable's lifecycle command when allowed, and returns
//! a client bound to it ([`BcodeClient::for_daemon`]).

use std::path::{Path, PathBuf};
use std::time::Duration;

use bcode_daemon_lifecycle::{DaemonTarget, DaemonTargetError, InstalledDaemonStartError};

use crate::{BcodeClient, ClientError};

/// Longest wait for the installed executable's start command.
const START_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait for a started daemon to become verifiable.
const START_VERIFY_TIMEOUT: Duration = Duration::from_secs(15);
/// Pause between verification attempts after a start.
const START_VERIFY_INTERVAL: Duration = Duration::from_millis(100);

/// Whether [`BcodeClient::for_installed_bcode`] may start the installed executable's daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstalledDaemonStart {
    /// Start the daemon when none is running, the way Bcode itself does.
    IfNeeded,
    /// Only attach to a running daemon.
    Never,
}

/// Failure to reach the installed Bcode.
#[derive(Debug, thiserror::Error)]
pub enum InstalledBcodeError {
    /// No Bcode executable was found.
    #[error("Bcode is not installed: no `bcode` executable on PATH (set BCODE_BIN to choose one)")]
    NotInstalled,
    /// `BCODE_BIN` is set but does not name an executable file.
    #[error("BCODE_BIN does not name an executable file: {0}")]
    InvalidOverride(PathBuf),
    /// The installed Bcode has no running daemon, and starting one was not allowed.
    #[error("Bcode is not running; start bcode and try again")]
    NotRunning,
    /// The installed Bcode's daemon could not be started.
    #[error("Bcode could not be started: {0}")]
    StartFailed(#[from] InstalledDaemonStartError),
    /// The installed executable or its daemon could not be verified.
    #[error("Bcode at {executable} could not be verified: {error}")]
    Unverified {
        /// The installed executable.
        executable: PathBuf,
        /// Why verification failed.
        error: DaemonTargetError,
    },
    /// The client could not be configured for the verified daemon.
    #[error(transparent)]
    Client(#[from] ClientError),
}

/// A client bound to the installed Bcode's daemon, with the executable it was verified for
/// (to run Bcode commands such as `bcode tui <session>` against the same daemon).
#[derive(Debug, Clone)]
pub struct InstalledBcode {
    client: BcodeClient,
    executable: PathBuf,
}

impl InstalledBcode {
    /// Client of the installed Bcode's daemon.
    #[must_use]
    pub const fn client(&self) -> &BcodeClient {
        &self.client
    }

    /// The verified installed executable.
    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// The client, consuming this value.
    #[must_use]
    pub fn into_client(self) -> BcodeClient {
        self.client
    }
}

impl BcodeClient {
    /// Client for the daemon of the user's installed Bcode, for apps that are not the Bcode
    /// executable.
    ///
    /// The executable is `BCODE_BIN`, else the first `bcode` on `PATH`. Its daemon is the one
    /// serving this process's state and configuration scope (`BCODE_STATE_DIR`,
    /// `XDG_CONFIG_HOME`); with [`InstalledDaemonStart::IfNeeded`] a missing daemon is started
    /// with `bcode server start` (inheriting this process's environment) and verified once it
    /// answers. Configuration and authentication are resolved in this process, as for
    /// [`BcodeClient::default_endpoint`].
    ///
    /// # Errors
    ///
    /// * [`InstalledBcodeError::InvalidOverride`] when `BCODE_BIN` is set to something that is
    ///   not an executable file.
    /// * [`InstalledBcodeError::NotInstalled`] when no executable is found.
    /// * [`InstalledBcodeError::NotRunning`] when no daemon is running and `start` is
    ///   [`InstalledDaemonStart::Never`].
    /// * [`InstalledBcodeError::StartFailed`] when the start command fails.
    /// * [`InstalledBcodeError::Unverified`] when the executable or its daemon cannot be
    ///   verified, including a started daemon that does not answer in time.
    /// * [`InstalledBcodeError::Client`] when configuration or authentication cannot be
    ///   resolved for the client.
    pub async fn for_installed_bcode(
        start: InstalledDaemonStart,
    ) -> Result<InstalledBcode, InstalledBcodeError> {
        let executable = bcode_daemon_lifecycle::installed_bcode_executable().ok_or_else(|| {
            not_found(std::env::var_os(bcode_daemon_lifecycle::BCODE_BIN_ENV).as_deref())
        })?;
        let target = verified_target(&executable, start).await?;
        let (config, launch) = crate::load_current_client_config()?;
        let mut client = Self::for_daemon(target, &config, crate::current_working_directory())?;
        // This process loaded the configuration from its own layers, so it can name the
        // sources the daemon re-verifies before admitting session configuration.
        if let Some(context) = &mut client.runtime_context {
            context.config_launch = Some(Box::new(launch));
        }
        Ok(InstalledBcode { client, executable })
    }
}

/// Why no executable was found: the `BCODE_BIN` override (when set) names none, else there is
/// no `bcode` on `PATH`.
fn not_found(bcode_bin: Option<&std::ffi::OsStr>) -> InstalledBcodeError {
    bcode_bin
        .filter(|value| !value.is_empty())
        .map_or(InstalledBcodeError::NotInstalled, |value| {
            InstalledBcodeError::InvalidOverride(PathBuf::from(value))
        })
}

async fn verified_target(
    executable: &Path,
    start: InstalledDaemonStart,
) -> Result<DaemonTarget, InstalledBcodeError> {
    let unverified = |error| InstalledBcodeError::Unverified {
        executable: executable.to_path_buf(),
        error,
    };
    let digest = bcode_daemon_lifecycle::executable_sha256(executable)
        .map_err(|_| unverified(DaemonTargetError::InvalidSelection))?;
    let location = bcode_config::resolve_state_location_set(
        &bcode_config::StateConfig::default(),
        &bcode_config::StateLocationSelection {
            root: Some(bcode_config::default_state_dir()),
            profile: None,
        },
    )
    .map_err(|_| unverified(DaemonTargetError::InvalidSelection))?
    .primary()
    .clone();
    let config_directory = bcode_config::default_config_dir();
    // The scope names the configuration directory; Bcode runs without one, so a fresh setup
    // gets an empty directory rather than an unverifiable scope.
    std::fs::create_dir_all(&config_directory)
        .map_err(|_| unverified(DaemonTargetError::InvalidSelection))?;
    let verify = || {
        DaemonTarget::verify(
            executable.to_path_buf(),
            digest.clone(),
            location.clone(),
            config_directory.clone(),
        )
    };
    match verify().await {
        Ok(target) => return Ok(target),
        Err(DaemonTargetError::Unavailable) if start == InstalledDaemonStart::IfNeeded => {}
        Err(DaemonTargetError::Unavailable) => return Err(InstalledBcodeError::NotRunning),
        Err(error) => return Err(unverified(error)),
    }
    bcode_daemon_lifecycle::start_installed_daemon(executable, START_COMMAND_TIMEOUT).await?;
    let deadline = tokio::time::Instant::now() + START_VERIFY_TIMEOUT;
    loop {
        match verify().await {
            Ok(target) => return Ok(target),
            Err(DaemonTargetError::Unavailable) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(START_VERIFY_INTERVAL).await;
            }
            Err(error) => return Err(unverified(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_broken_override_is_reported_as_such() {
        assert!(matches!(not_found(None), InstalledBcodeError::NotInstalled));
        assert!(matches!(
            not_found(Some(std::ffi::OsStr::new(""))),
            InstalledBcodeError::NotInstalled
        ));
        let error = not_found(Some(std::ffi::OsStr::new("/opt/bcode-typo")));
        assert_eq!(
            error.to_string(),
            "BCODE_BIN does not name an executable file: /opt/bcode-typo"
        );
    }
}
