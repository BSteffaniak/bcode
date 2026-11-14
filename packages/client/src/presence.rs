//! Whether this client's daemon is up, as the client last saw it (see
//! `docs/daemon-dormancy.md`).
//!
//! Every clone of a [`super::BcodeClient`] shares one presence. A connection that receives the
//! daemon's shutdown announcement makes it dormant; any verified connection makes it live again.
//! Long-lived streams wait on it instead of reconnecting: while dormant they send nothing and
//! start nothing, and they resume once real work (a request) has started the daemon again, here
//! or in another process of the same build.

use bcode_ipc::DaemonShutdownReason;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::watch;

/// The daemon as this client last saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonPresence {
    /// Connected, or not known to have stopped.
    Live,
    /// It said it stopped. Nothing is sent until real work starts it again.
    Dormant {
        /// Why it stopped.
        reason: DaemonShutdownReason,
    },
}

/// Shared presence of one client's daemon.
#[derive(Debug, Clone)]
pub struct Presence {
    state: Arc<watch::Sender<DaemonPresence>>,
}

impl Default for Presence {
    fn default() -> Self {
        Self {
            state: Arc::new(watch::Sender::new(DaemonPresence::Live)),
        }
    }
}

impl Presence {
    /// The daemon as last seen.
    #[must_use]
    pub fn current(&self) -> DaemonPresence {
        *self.state.borrow()
    }

    /// A verified connection was made.
    pub fn connected(&self) {
        self.state.send_if_modified(|state| {
            let changed = *state != DaemonPresence::Live;
            *state = DaemonPresence::Live;
            changed
        });
    }

    /// The daemon announced it is stopping.
    pub fn stopped(&self, reason: DaemonShutdownReason) {
        self.state.send_replace(DaemonPresence::Dormant { reason });
    }

    /// Wait until the daemon is live again. While dormant, `endpoint` (a Unix socket path, when
    /// there is one) is watched, so a daemon started by another process of this build wakes
    /// the wait as well; `probe` then confirms it answers and reports the connection.
    pub async fn live<F, Fut>(&self, endpoint: Option<&Path>, probe: F)
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let mut updates = self.state.subscribe();
        if *updates.borrow_and_update() == DaemonPresence::Live {
            return;
        }
        let mut appeared = endpoint.and_then(|path| EndpointWatch::new(path.to_path_buf()));
        loop {
            tokio::select! {
                changed = updates.changed() => {
                    if changed.is_err() || *updates.borrow_and_update() == DaemonPresence::Live {
                        return;
                    }
                }
                () = async {
                    match appeared.as_mut() {
                        Some(watch) => watch.next().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if probe().await {
                        self.connected();
                        return;
                    }
                }
            }
        }
    }
}

/// Notices a daemon endpoint (a socket file) being created, without polling.
struct EndpointWatch {
    // Kept for its lifetime: dropping it stops the notifications.
    _watcher: notify::RecommendedWatcher,
    created: tokio::sync::mpsc::UnboundedReceiver<()>,
}

impl EndpointWatch {
    fn new(endpoint: PathBuf) -> Option<Self> {
        use notify::Watcher as _;
        let parent = endpoint.parent()?.to_path_buf();
        // Notifications name canonical paths (on macOS, `/private/var/...` for `/var/...`).
        let canonical = std::fs::canonicalize(&parent)
            .ok()
            .and_then(|parent| Some(parent.join(endpoint.file_name()?)));
        let (sender, created) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else { return };
                let ours = |path: &PathBuf| {
                    path == &endpoint
                        || canonical
                            .as_ref()
                            .is_some_and(|canonical| path == canonical)
                };
                if event.kind.is_create() && event.paths.iter().any(ours) {
                    let _ = sender.send(());
                }
            })
            .ok()?;
        watcher
            .watch(&parent, notify::RecursiveMode::NonRecursive)
            .ok()?;
        Some(Self {
            _watcher: watcher,
            created,
        })
    }

    async fn next(&mut self) {
        if self.created.recv().await.is_none() {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn waits_while_dormant_and_wakes_on_a_connection() {
        let presence = Presence::default();
        // Live: no wait.
        presence.live(None, || async { false }).await;
        presence.stopped(DaemonShutdownReason::Idle);
        assert_eq!(
            presence.current(),
            DaemonPresence::Dormant {
                reason: DaemonShutdownReason::Idle
            }
        );
        let waiting = {
            let presence = presence.clone();
            tokio::spawn(async move { presence.live(None, || async { false }).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished());
        presence.connected();
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("woken")
            .expect("joined");
    }

    #[tokio::test]
    async fn a_daemon_started_elsewhere_wakes_a_dormant_wait() {
        let directory = tempfile::tempdir().expect("dir");
        let endpoint = directory.path().join("daemon.sock");
        let presence = Presence::default();
        presence.stopped(DaemonShutdownReason::Idle);
        let waiting = {
            let presence = presence.clone();
            let endpoint = endpoint.clone();
            tokio::spawn(async move { presence.live(Some(&endpoint), || async { true }).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiting.is_finished());
        std::fs::write(&endpoint, b"").expect("endpoint appears");
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("woken by the endpoint")
            .expect("joined");
        assert_eq!(presence.current(), DaemonPresence::Live);
    }
}
