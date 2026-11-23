//! The daemon-side triage dispatcher thread.

use std::path::Path;

use bcode_client::BcodeClient;
use bcode_feedback_client::worker::{WorkerOptions, run};

/// A running dispatcher; stops when told to or when its plugin is dropped.
pub struct Dispatcher {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Dispatcher {
    /// Start delivering the queued triage requests of the store in `state_dir`, through the
    /// daemon serving that state location.
    ///
    /// The client never starts a daemon: it reaches the daemon this plugin runs in, at that
    /// state location's endpoint, and the handshake rejects any daemon serving another location.
    pub fn start(state_dir: &Path) -> Result<Self, String> {
        let location = bcode_config::resolve_state_location_set(
            &bcode_config::StateConfig::default(),
            &bcode_config::StateLocationSelection {
                root: Some(state_dir.to_owned()),
                profile: None,
            },
        )
        .map_err(|error| error.to_string())?
        .primary()
        .clone();
        let client = BcodeClient::for_state_location(bcode_ipc::default_endpoint(), &location);
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let options = WorkerOptions::new(bcode_feedback_client::holder("daemon"));
        std::thread::Builder::new()
            .name("bcode-feedback-dispatch".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::warn!(%error, "feedback: triage dispatcher has no runtime");
                        return;
                    }
                };
                runtime.block_on(run(client, options, async move {
                    let _ = stopped.await;
                }));
            })
            .map_err(|error| error.to_string())?;
        Ok(Self { stop: Some(stop) })
    }

    /// Stop after the current pass.
    pub fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
