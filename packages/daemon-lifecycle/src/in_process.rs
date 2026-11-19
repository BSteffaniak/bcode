//! Daemons hosted inside an application's own process.
//!
//! An application that embeds Bcode can serve its daemon from its own process instead of
//! launching `<executable> server run`. It registers one in-process host before any client
//! connects. From then on every daemon start in that process, including the automatic start
//! a client performs when no matching daemon answers, calls the host instead of spawning a
//! process. Startup coordination is unchanged: the per-namespace startup lock, stale endpoint
//! cleanup and verified readiness still apply, so one daemon serves each artifact identity and
//! state location whichever process hosts it.
//!
//! A host must serve the endpoint it is given with the current artifact's server (for
//! example `bcode_server::run_in_process`) and return without waiting for it: readiness is
//! observed by the caller.

use std::sync::{Arc, OnceLock};

use bcode_ipc::IpcEndpoint;

use crate::DaemonStartError;

/// Starts the daemon for an endpoint inside the current process, without waiting for it to
/// become ready. It runs while the startup lock is held and must not block.
pub type InProcessDaemonStart =
    Arc<dyn Fn(&IpcEndpoint) -> Result<(), DaemonStartError> + Send + Sync>;

static HOST: OnceLock<InProcessDaemonStart> = OnceLock::new();

/// Serve this process's daemon starts with `start` instead of launching a daemon process.
///
/// Only the first registration takes effect; later ones return `false`. Register before
/// creating clients, so no client starts a daemon process first.
pub fn register_in_process_daemon_host(start: InProcessDaemonStart) -> bool {
    HOST.set(start).is_ok()
}

/// Whether this process hosts its daemon in-process.
#[must_use]
pub fn hosts_daemon_in_process() -> bool {
    HOST.get().is_some()
}

/// The registered in-process host, if any.
pub fn registered_host() -> Option<InProcessDaemonStart> {
    HOST.get().cloned()
}
