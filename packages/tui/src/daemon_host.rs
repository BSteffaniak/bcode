//! In-process daemon hosting for the TUI.

use std::sync::{Arc, Mutex};

use bcode_daemon_lifecycle::{DaemonStartError, EnsureDaemonOptions};
use tokio::task::JoinHandle;

/// Hosts the matching build's daemon inside the running TUI process.
#[derive(Debug, Clone)]
pub struct TuiDaemonHost {
    static_plugins: Arc<Vec<bcode_plugin::StaticBundledPlugin>>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl TuiDaemonHost {
    /// Create a TUI daemon host for caller-provided static bundled plugins.
    #[must_use]
    pub fn new(static_plugins: &[bcode_plugin::StaticBundledPlugin]) -> Self {
        Self {
            static_plugins: Arc::new(static_plugins.to_vec()),
            task: Arc::new(Mutex::new(None)),
        }
    }

    /// Ensure the current exact-build daemon namespace is available.
    ///
    /// # Errors
    ///
    /// Returns an error when lifecycle coordination fails or the daemon does not become ready.
    pub async fn ensure_available(&self) -> Result<(), DaemonStartError> {
        bcode_daemon_lifecycle::ensure_daemon_running_in_process(
            &EnsureDaemonOptions::default_for_current_namespace(),
            || self.start_if_needed(),
        )
        .await
    }

    fn start_if_needed(&self) -> Result<(), DaemonStartError> {
        let mut task = self
            .task
            .lock()
            .map_err(|_| std::io::Error::other("TUI daemon host lock poisoned"))?;
        if task.as_ref().is_some_and(|task| !task.is_finished()) {
            return Ok(());
        }

        let endpoint = bcode_ipc::default_endpoint();
        let static_plugins = Arc::clone(&self.static_plugins);
        *task = Some(tokio::spawn(async move {
            if let Err(error) =
                bcode_server::run_with_static_bundled(endpoint, &static_plugins).await
            {
                eprintln!("in-process daemon exited: {error}");
            }
        }));
        drop(task);
        Ok(())
    }
}
