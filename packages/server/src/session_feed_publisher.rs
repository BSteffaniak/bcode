//! Publishes each owned session's feed (see `docs/daemon-dormancy.md`): its committed events
//! and an activity marker, so a client of any build can follow a session this daemon runs
//! without reading its database. The feed is a view; failing to write it never affects the
//! session.

use crate::ServerState;
use bcode_session_feed::{FeedOwner, FeedWriter};
use bcode_session_models::SessionId;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

/// Sessions published at once; the least recently published is let go beyond this (its feed
/// stays, and the next event reopens it).
const MAX_OPEN_FEEDS: usize = 64;

pub fn start(state: &Arc<ServerState>, sessions_root: PathBuf) {
    let mut mutations = state.sessions.subscribe_mutations();
    let mut shutdown = state.subscribe_shutdown();
    let status = &state.daemon_status;
    let owner = FeedOwner {
        build_fingerprint: status.build_fingerprint.clone(),
        artifact_id: status.artifact_id.as_ref().map(ToString::to_string),
        protocol_version: status.protocol_version,
        storage_writer_epoch: status.storage_writer_epoch,
        daemon_instance_id: Some(status.instance_id.clone()),
        pid: status.pid.unwrap_or_else(std::process::id),
    };
    tokio::spawn(async move {
        let mut feeds = Feeds::new(sessions_root, owner);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.recv() => break,
                received = mutations.recv() => match received {
                    Ok(committed) => feeds.publish(&committed.event),
                    // Events were committed but not seen here: every open feed is incomplete.
                    Err(RecvError::Lagged(_)) => feeds.gap(),
                    Err(RecvError::Closed) => break,
                },
            }
        }
        feeds.release();
    });
}

/// The feeds this daemon publishes, most recently used last.
struct Feeds {
    sessions_root: PathBuf,
    owner: FeedOwner,
    open: BTreeMap<SessionId, (u64, FeedWriter)>,
    clock: u64,
}

impl Feeds {
    const fn new(sessions_root: PathBuf, owner: FeedOwner) -> Self {
        Self {
            sessions_root,
            owner,
            open: BTreeMap::new(),
            clock: 0,
        }
    }

    fn publish(&mut self, event: &bcode_session_models::SessionEvent) {
        self.clock += 1;
        let clock = self.clock;
        let session_id = event.session_id;
        if !self.open.contains_key(&session_id) {
            if self.open.len() >= MAX_OPEN_FEEDS
                && let Some(oldest) = self
                    .open
                    .iter()
                    .min_by_key(|(_, (used, _))| *used)
                    .map(|(id, _)| *id)
            {
                // Dropped, not released: the session may still be running here.
                self.open.remove(&oldest);
            }
            match FeedWriter::open(&self.sessions_root, session_id, self.owner.clone()) {
                Ok(writer) => {
                    self.open.insert(session_id, (clock, writer));
                }
                Err(error) => {
                    tracing::debug!(target: "bcode_server::session_feed", %session_id, %error, "session feed unavailable");
                    return;
                }
            }
        }
        if let Some((used, writer)) = self.open.get_mut(&session_id) {
            *used = clock;
            if let Err(error) = writer.publish(event) {
                tracing::debug!(target: "bcode_server::session_feed", %session_id, %error, "session feed write failed");
                self.open.remove(&session_id);
            }
        }
    }

    fn gap(&mut self) {
        self.open.retain(|_, (_, writer)| writer.gap().is_ok());
    }

    /// The daemon is stopping: every feed it published says it no longer owns the session.
    fn release(self) {
        for (_, writer) in self.open.into_values() {
            let _ = writer.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_feed::{FeedFollower, FeedUpdate};
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn committed_events_reach_the_session_feed_with_its_owner() {
        let root = tempfile::tempdir().expect("session root");
        let sessions = bcode_session::SessionManager::persistent(root.path()).expect("sessions");
        let state = Arc::new(crate::tests::test_server_state(sessions));
        let sessions_root = state.sessions.store_root().expect("persistent root");
        start(&state, sessions_root.clone());
        let session = state
            .sessions
            .create_session(Some("published".to_owned()), root.path().to_path_buf())
            .await
            .expect("session");
        let mut follower = FeedFollower::open(&sessions_root, session.id, None).expect("follow");
        let mut seen_marker = false;
        let mut seen_event = false;
        while !(seen_marker && seen_event) {
            match tokio::time::timeout(Duration::from_secs(5), follower.next())
                .await
                .expect("published")
                .expect("watched")
            {
                FeedUpdate::Marker(marker) => {
                    assert_eq!(
                        marker.owner.build_fingerprint,
                        state.daemon_status.build_fingerprint
                    );
                    assert!(marker.owning);
                    seen_marker = true;
                }
                FeedUpdate::Event(event) => {
                    assert_eq!(event.session_id, session.id);
                    seen_event = true;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        state.request_shutdown();
        drop(state);
    }
}
