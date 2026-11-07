//! Finding and stopping the user's daemons, of every build.

use std::io::BufReader;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::protocol::{self, ClientMessage, PROTOCOL, ServerMessage};

/// Longest wait for each step of stopping a daemon.
const GRACE: Duration = Duration::from_secs(3);

/// A daemon socket of the user's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Its socket.
    pub socket: PathBuf,
    /// The process serving it; `None` when nothing answers there (a stale socket).
    pub pid: Option<u32>,
}

/// How stopping a daemon went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// It exited when asked.
    Stopped,
    /// It did not exit when asked (it predates being asked, or is stuck) and was terminated
    /// by signal.
    Terminated,
    /// Nothing answered; the leftover socket was removed.
    Stale,
    /// It could not be stopped.
    Failed(String),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => formatter.write_str("stopped"),
            Self::Terminated => formatter.write_str("terminated (it did not stop when asked)"),
            Self::Stale => formatter.write_str("removed a stale socket"),
            Self::Failed(reason) => write!(formatter, "could not stop it: {reason}"),
        }
    }
}

/// Every daemon socket of `state_dir`, of all builds: those inside it, and its own in the
/// short-path directory (only trusted when that belongs to the user).
#[must_use]
pub fn find(state_dir: &Path) -> Vec<Found> {
    let sockets_in = |directory: &Path, prefix: &str| -> Vec<PathBuf> {
        std::fs::read_dir(directory)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "sock")
            })
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(prefix))
            })
            .collect()
    };
    let mut sockets = sockets_in(&state_dir.join("daemon"), "");
    let short = crate::paths::short_socket_dir();
    if std::fs::metadata(&short).is_ok_and(|metadata| metadata.uid() == crate::paths::owner()) {
        sockets.extend(sockets_in(
            &short,
            &crate::paths::short_socket_prefix(state_dir),
        ));
    }
    sockets.sort();
    sockets
        .into_iter()
        .map(|socket| Found {
            pid: greet(&socket).ok().map(|(pid, _)| pid),
            socket,
        })
        .collect()
}

/// Stop every daemon [`find`] finds.
#[must_use]
pub fn stop_all(state_dir: &Path) -> Vec<(Found, Outcome)> {
    find(state_dir)
        .into_iter()
        .map(|found| {
            let outcome = stop(&found);
            (found, outcome)
        })
        .collect()
}

/// Stop one daemon: ask it, then (when it does not exit) terminate it by signal, and remove
/// a socket it leaves behind. Its clients carry on serving themselves.
#[must_use]
pub fn stop(found: &Found) -> Outcome {
    let Some(pid) = found.pid else {
        return match std::fs::remove_file(&found.socket) {
            Ok(()) => Outcome::Stale,
            Err(error) => Outcome::Failed(error.to_string()),
        };
    };
    if pid == std::process::id() {
        // Served from this very process (embedded): ask, and never signal ourselves.
        return if ask_to_stop(&found.socket).unwrap_or(false) {
            let deadline = Instant::now() + GRACE;
            while UnixStream::connect(&found.socket).is_ok() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            Outcome::Stopped
        } else {
            Outcome::Failed("it is served by this process and did not stop".into())
        };
    }
    let Some(process) = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap_or(0)) else {
        return Outcome::Failed(format!("invalid process id {pid}"));
    };
    let alive = || rustix::process::test_kill_process(process).is_ok();
    let exited = |within: Duration| {
        let deadline = Instant::now() + within;
        while alive() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    };
    let asked = ask_to_stop(&found.socket).unwrap_or(false);
    let outcome = if asked && exited(GRACE) {
        Outcome::Stopped
    } else {
        let _ = rustix::process::kill_process(process, rustix::process::Signal::TERM);
        if !exited(GRACE) {
            let _ = rustix::process::kill_process(process, rustix::process::Signal::KILL);
        }
        if exited(GRACE) {
            Outcome::Terminated
        } else {
            return Outcome::Failed(format!("process {pid} is still running"));
        }
    };
    // A daemon stopped by signal leaves its socket behind; nobody answers there any more.
    if UnixStream::connect(&found.socket).is_err() {
        let _ = std::fs::remove_file(&found.socket);
    }
    outcome
}

/// Connect and say hello: the daemon's process id, and the open conversation.
fn greet(socket: &Path) -> std::io::Result<(u32, UnixStream)> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(GRACE))?;
    let mut writer = stream.try_clone()?;
    protocol::write(&mut writer, &ClientMessage::Hello { protocol: PROTOCOL })?;
    let mut reader = BufReader::new(stream.try_clone()?);
    match protocol::read::<ServerMessage>(&mut reader)? {
        Some(ServerMessage::Welcome { pid, .. }) => Ok((pid, stream)),
        other => Err(std::io::Error::other(format!(
            "unexpected answer {other:?}"
        ))),
    }
}

/// Ask the daemon at `socket` to exit; whether it agreed.
fn ask_to_stop(socket: &Path) -> std::io::Result<bool> {
    let (_, stream) = greet(socket)?;
    let mut writer = stream.try_clone()?;
    protocol::write(&mut writer, &ClientMessage::Stop)?;
    let mut reader = BufReader::new(stream);
    Ok(matches!(
        protocol::read::<ServerMessage>(&mut reader),
        Ok(Some(ServerMessage::Stopping))
    ))
}
