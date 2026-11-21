//! A Bcode embedded in the application: Bcode's daemon hosted in the application's process.
//!
//! The first attach registers the application as its process's embedded daemon host
//! ([`EmbeddedDaemonHost`]). From then on, whenever a Bcode client in this build needs its
//! daemon and none answers, the daemon starts here instead of as a process, on the host's
//! runtime, whose threads are named [`DAEMON_THREAD_PREFIX`].
//!
//! The application talks to it exactly as to an installed Bcode: through the session bridge,
//! served in-process over an in-memory pipe.

use std::path::PathBuf;
use std::time::Duration;

use bcode_server::embedded_host::{EmbeddedDaemonHost, EmbeddedDaemonHostOptions};
use bcode_session_bridge::models::VersionRange;

use crate::{AttachError, Attached, RuntimeOptions};

/// Name prefix of the embedded daemon's threads. An application's panic hook can leave the
/// terminal alone for panics on them: Bcode recovers from those.
pub const DAEMON_THREAD_PREFIX: &str = "bcode-daemon";

/// Bytes buffered each way between the application and the in-process bridge.
const BRIDGE_PIPE_BYTES: usize = 1 << 20;

/// The process's embedded daemon host, registered on first use with the first caller's name
/// and private environment.
fn host(options: &RuntimeOptions) -> Result<&'static EmbeddedDaemonHost, AttachError> {
    EmbeddedDaemonHost::register(EmbeddedDaemonHostOptions {
        server: bcode_server::InProcessServerOptions {
            static_plugins: bcode_bundled_plugins::static_bundled_plugins(),
            host_name: options.app.clone(),
            bcode_cli: bcode_daemon_lifecycle::installed_bcode_executable(),
            idle_shutdown: false,
            private_env: options.private_env.iter().cloned().collect(),
        },
        thread_name: DAEMON_THREAD_PREFIX.to_owned(),
    })
    .map_err(|error| AttachError::Failed(error.to_string()))
}

/// Start (or find) the embedded daemon and serve the session bridge to it in-process.
pub async fn attach(options: &RuntimeOptions) -> Result<Attached, AttachError> {
    let host = host(options)?;
    let (app_side, bridge_side) = tokio::io::duplex(BRIDGE_PIPE_BYTES);
    let (bridge_reader, bridge_writer) = tokio::io::split(bridge_side);
    let bridge = bcode_session_bridge_server::BridgeOptions {
        versions: VersionRange::supported(),
        bcode_version: format!("embedded in {}", options.app),
        // "Open in Bcode" hands sessions to the installed Bcode's TUI.
        executable: bcode_daemon_lifecycle::installed_bcode_executable()
            .unwrap_or_else(|| PathBuf::from("bcode")),
    };
    host.handle().spawn(async move {
        // The client starts the daemon through this host when none answers.
        let client = bcode_client::BcodeClient::default_endpoint();
        if let Err(error) =
            bcode_session_bridge_server::serve(bridge_reader, bridge_writer, client, bridge).await
        {
            tracing::warn!("embedded Bcode's session bridge stopped: {error}");
        }
    });
    let (reader, writer) = tokio::io::split(app_side);
    Attached::connect(reader, writer, &options.app, options.versions).await
}

/// Whether this process hosts the embedded Bcode.
pub fn running() -> bool {
    EmbeddedDaemonHost::get().is_some()
}

/// Stop the embedded Bcode, waiting up to `wait` for its cleanup to finish.
pub async fn shutdown(wait: Duration) {
    if let Some(host) = EmbeddedDaemonHost::get()
        && !host.shutdown(wait).await
    {
        tracing::warn!("the embedded Bcode did not finish stopping in time");
    }
}
