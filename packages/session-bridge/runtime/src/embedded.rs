//! A Bcode embedded in the application: Bcode's daemon hosted in the application's process.
//!
//! The first attach registers the application as its process's in-process daemon host
//! (`bcode_daemon_lifecycle::register_in_process_daemon_host`). From then on, whenever a Bcode
//! client in this build needs its daemon and none answers, the daemon starts here
//! (`bcode_server::run_in_process`) instead of as a process. It runs on its own runtime, whose
//! threads are named [`DAEMON_THREAD_PREFIX`], with the stacks Bcode needs, and keeps sessions
//! in the user's Bcode store like an installed Bcode.
//!
//! The application talks to it exactly as to an installed Bcode: through the session bridge,
//! served in-process over an in-memory pipe.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use bcode_agent_runtime::CancellationToken;
use bcode_session_bridge::models::VersionRange;

use crate::{AttachError, Attached, RuntimeOptions};

/// Name prefix of the embedded daemon's threads. An application's panic hook can leave the
/// terminal alone for panics on them: Bcode recovers from those.
pub const DAEMON_THREAD_PREFIX: &str = "bcode-daemon";

/// Thread stack size of the embedded daemon's runtime: Bcode nests plugin, managed-session and
/// harness calls on one thread, which overflows the 2 MiB default in unoptimized builds.
const DAEMON_THREAD_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Bytes buffered each way between the application and the in-process bridge.
const BRIDGE_PIPE_BYTES: usize = 1 << 20;

/// The process's in-process daemon host.
struct Host {
    runtime: tokio::runtime::Runtime,
    shutdown: CancellationToken,
    /// Daemons started here, to wait for when the application exits.
    served: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

static HOST: OnceLock<Result<Host, String>> = OnceLock::new();

/// The host, registered on first use with the first caller's name and private environment.
fn host(options: &RuntimeOptions) -> Result<&'static Host, AttachError> {
    HOST.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .thread_name(DAEMON_THREAD_PREFIX)
            .thread_stack_size(DAEMON_THREAD_STACK_BYTES)
            .enable_all()
            .build()
            .map_err(|error| format!("could not start the embedded Bcode's runtime: {error}"))?;
        let host = Host {
            runtime,
            shutdown: CancellationToken::new(),
            served: Mutex::new(Vec::new()),
        };
        let server_options = bcode_server::InProcessServerOptions {
            static_plugins: bcode_bundled_plugins::static_bundled_plugins(),
            host_name: options.app.clone(),
            bcode_cli: bcode_daemon_lifecycle::installed_bcode_executable(),
            idle_shutdown: false,
            private_env: options.private_env.iter().cloned().collect(),
        };
        let handle = host.runtime.handle().clone();
        let shutdown = host.shutdown.clone();
        let registered = bcode_daemon_lifecycle::register_in_process_daemon_host(
            std::sync::Arc::new(move |endpoint| {
                let serving = bcode_server::run_in_process(
                    endpoint.clone(),
                    server_options.clone(),
                    shutdown.clone(),
                );
                let task = handle.spawn(async move {
                    if let Err(error) = serving.await {
                        tracing::warn!("embedded Bcode stopped: {error}");
                    }
                });
                if let Some(host) = HOST.get().and_then(|host| host.as_ref().ok()) {
                    lock(&host.served).push(task);
                }
                Ok(())
            }),
        );
        if !registered {
            return Err("another in-process Bcode host is already registered".to_owned());
        }
        Ok(host)
    })
    .as_ref()
    .map_err(|error| AttachError::Failed(error.clone()))
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
    host.runtime.spawn(async move {
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
    HOST.get().is_some_and(Result::is_ok)
}

/// Stop the embedded Bcode, waiting up to `wait` for its cleanup to finish.
pub async fn shutdown(wait: Duration) {
    let Some(Ok(host)) = HOST.get() else {
        return;
    };
    host.shutdown.cancel();
    let served: Vec<_> = lock(&host.served).drain(..).collect();
    let all = async {
        for task in served {
            let _ = task.await;
        }
    };
    if tokio::time::timeout(wait, all).await.is_err() {
        tracing::warn!("the embedded Bcode did not finish stopping in time");
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
