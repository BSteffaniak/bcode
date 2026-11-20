//! A ready-made in-process daemon host for applications that embed Bcode.
//!
//! [`crate::run_in_process`] serves one daemon and
//! `bcode_daemon_lifecycle::register_in_process_daemon_host` routes this process's daemon
//! starts to a host. [`EmbeddedDaemonHost::register`] does both for the common case: it builds
//! the runtime the daemon runs on (its own threads, named for the application, with the stacks
//! Bcode needs), registers the process's in-process host, starts a daemon on that runtime
//! whenever a client in the process needs one and none answers, and keeps the started daemons
//! so the application can stop them gracefully on exit with [`EmbeddedDaemonHost::shutdown`].
//!
//! Clients in the process then reach the daemon like any other: through
//! `bcode_client::BcodeClient::default_endpoint` with automatic start.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bcode_agent_runtime::CancellationToken;

use crate::InProcessServerOptions;

/// Thread stack size of an embedded daemon's runtime (Bcode's own executable uses the same).
///
/// Bcode nests plugin, managed-session and harness calls on one thread, which overflows the
/// 2 MiB default in unoptimized builds.
pub const EMBEDDED_DAEMON_THREAD_STACK_BYTES: usize = 8 * 1024 * 1024;

/// How an embedded daemon host runs.
#[derive(Clone)]
pub struct EmbeddedDaemonHostOptions {
    /// The daemon's options.
    pub server: InProcessServerOptions,
    /// Name of the daemon runtime's threads, so the application can tell them apart (for
    /// example to leave the terminal alone for a panic Bcode recovers from).
    pub thread_name: String,
}

/// Failure to register an embedded daemon host.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EmbeddedDaemonHostError {
    /// The daemon's runtime could not be built.
    #[error("could not start the embedded Bcode's runtime: {0}")]
    Runtime(String),
    /// Another in-process host was registered first.
    #[error("another in-process Bcode host is already registered")]
    AlreadyRegistered,
}

/// This process's embedded daemon host (see the [module docs](self)).
pub struct EmbeddedDaemonHost {
    runtime: tokio::runtime::Runtime,
    shutdown: CancellationToken,
    /// Daemons started here, to wait for on shutdown.
    served: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for EmbeddedDaemonHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EmbeddedDaemonHost")
            .field("stopping", &self.shutdown.is_cancelled())
            .finish_non_exhaustive()
    }
}

static HOST: OnceLock<Result<EmbeddedDaemonHost, EmbeddedDaemonHostError>> = OnceLock::new();

impl EmbeddedDaemonHost {
    /// Register this process's embedded daemon host, or return the one already registered
    /// (whose options are kept). Register before creating clients, so no client starts a
    /// daemon process first.
    ///
    /// # Errors
    ///
    /// * [`EmbeddedDaemonHostError::Runtime`] when the daemon's runtime cannot be built.
    /// * [`EmbeddedDaemonHostError::AlreadyRegistered`] when another in-process host was
    ///   registered through `bcode_daemon_lifecycle` directly.
    ///
    /// A failed first registration is final: later calls return the same error.
    pub fn register(
        options: EmbeddedDaemonHostOptions,
    ) -> Result<&'static Self, EmbeddedDaemonHostError> {
        HOST.get_or_init(|| Self::start(options))
            .as_ref()
            .map_err(Clone::clone)
    }

    /// The registered embedded daemon host, if any.
    #[must_use]
    pub fn get() -> Option<&'static Self> {
        HOST.get().and_then(|host| host.as_ref().ok())
    }

    fn start(options: EmbeddedDaemonHostOptions) -> Result<Self, EmbeddedDaemonHostError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .thread_name(options.thread_name)
            .thread_stack_size(EMBEDDED_DAEMON_THREAD_STACK_BYTES)
            .enable_all()
            .build()
            .map_err(|error| EmbeddedDaemonHostError::Runtime(error.to_string()))?;
        let host = Self {
            runtime,
            shutdown: CancellationToken::new(),
            served: Mutex::new(Vec::new()),
        };
        let handle = host.runtime.handle().clone();
        let shutdown = host.shutdown.clone();
        let server = options.server;
        let registered =
            bcode_daemon_lifecycle::register_in_process_daemon_host(Arc::new(move |endpoint| {
                let serving =
                    crate::run_in_process(endpoint.clone(), server.clone(), shutdown.clone());
                let task = handle.spawn(async move {
                    if let Err(error) = serving.await {
                        tracing::warn!(%error, "embedded Bcode daemon stopped");
                    }
                });
                // The host is stored before any client can start a daemon through it.
                if let Some(host) = Self::get() {
                    lock(&host.served).push(task);
                }
                Ok(())
            }));
        if !registered {
            return Err(EmbeddedDaemonHostError::AlreadyRegistered);
        }
        Ok(host)
    }

    /// Handle of the daemon's runtime, to run other Bcode work (such as a session bridge) next
    /// to the daemon with the same thread stacks.
    #[must_use]
    pub fn handle(&self) -> &tokio::runtime::Handle {
        self.runtime.handle()
    }

    /// Stop the daemons started here, waiting up to `wait` for their cleanup (releasing
    /// sessions and plugins) to finish. Returns whether they finished in time.
    pub async fn shutdown(&self, wait: Duration) -> bool {
        self.shutdown.cancel();
        let served: Vec<_> = lock(&self.served).drain(..).collect();
        let all = async {
            for task in served {
                let _ = task.await;
            }
        };
        let finished = tokio::time::timeout(wait, all).await.is_ok();
        if !finished {
            tracing::warn!("the embedded Bcode daemon did not finish stopping in time");
        }
        finished
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
