//! Following a session that another daemon owns (see `docs/daemon-dormancy.md`).
//!
//! A session's owning daemon publishes its committed events and an activity marker (the
//! session feed). A client whose own daemon is dormant, or of another build than the session's
//! owner, follows that feed read-only instead of reading the session's database, which only the
//! owner may open.

use super::BcodeClient;
pub use bcode_session_feed::{ActivityMarker, FeedFollower, FeedUnreadable, FeedUpdate};
use bcode_session_models::SessionId;
use std::path::PathBuf;

/// Who runs a session now, as its published marker says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOwner {
    /// No live owner: the session is not running anywhere.
    None,
    /// A daemon of this client's build: talk to it.
    ThisBuild(Box<ActivityMarker>),
    /// A daemon of another build this client can follow, read-only.
    OtherBuild(Box<ActivityMarker>),
    /// A newer build whose feed this build cannot read.
    NewerBuild,
}

impl BcodeClient {
    /// The session store root of this client's state location.
    fn sessions_root(&self) -> PathBuf {
        self.lifecycle_state_location
            .as_ref()
            .map_or_else(bcode_config::default_session_store_dir, |location| {
                location.sessions_root().to_path_buf()
            })
    }

    /// Who runs `session_id`, from its published marker. A marker whose owner process is gone
    /// (it crashed before giving the session up) is no owner.
    #[must_use]
    pub fn session_owner(&self, session_id: SessionId) -> SessionOwner {
        match bcode_session_feed::read_marker(&self.sessions_root(), session_id) {
            Ok(Some(marker)) if marker.owning && process_alive(marker.owner.pid) => {
                let build = self.explicit_daemon_target.as_ref().map_or(
                    bcode_ipc::BUILD_FINGERPRINT,
                    bcode_daemon_lifecycle::DaemonTarget::build_fingerprint,
                );
                if marker.owner.build_fingerprint == build {
                    SessionOwner::ThisBuild(Box::new(marker))
                } else {
                    SessionOwner::OtherBuild(Box::new(marker))
                }
            }
            Ok(_) | Err(FeedUnreadable::Damaged) => SessionOwner::None,
            Err(FeedUnreadable::NewerBuild) => SessionOwner::NewerBuild,
        }
    }

    /// Follow `session_id`'s published feed from after sequence `after`, read-only.
    ///
    /// # Errors
    /// Returns an error when the feed directory cannot be prepared for watching.
    pub fn follow_session_feed(
        &self,
        session_id: SessionId,
        after: Option<u64>,
    ) -> std::io::Result<FeedFollower> {
        FeedFollower::open(&self.sessions_root(), session_id, after)
    }
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // Signal 0 checks existence; EPERM means it exists under another user.
        // SAFETY: `kill` with signal 0 sends nothing and only reads process existence.
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_feed::{FeedOwner, FeedWriter};

    fn client(root: &std::path::Path) -> (BcodeClient, PathBuf) {
        std::fs::create_dir_all(root.join("state")).expect("state root");
        let locations = bcode_config::resolve_state_location_set_with_environment(
            &bcode_config::StateConfig::default(),
            &bcode_config::StateLocationSelection {
                root: Some(root.join("state")),
                profile: None,
            },
            &bcode_config::ConfigEnvironmentSnapshot::new(
                std::collections::BTreeMap::new(),
                root.to_path_buf(),
            ),
        )
        .expect("locations");
        let sessions_root = locations.primary().sessions_root().to_path_buf();
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(root.join("unused.sock"));
        (
            BcodeClient::for_state_location(endpoint, locations.primary()),
            sessions_root,
        )
    }

    fn owner(build: &str, pid: u32) -> FeedOwner {
        FeedOwner {
            build_fingerprint: build.to_owned(),
            artifact_id: None,
            protocol_version: 1,
            storage_writer_epoch: None,
            daemon_instance_id: None,
            pid,
        }
    }

    #[test]
    fn the_owner_is_classified_by_build_and_a_dead_owner_is_none() {
        let directory = tempfile::tempdir().expect("dir");
        let (client, sessions_root) = client(directory.path());
        let session = SessionId::new();
        assert_eq!(client.session_owner(session), SessionOwner::None);
        let ours = FeedWriter::open(
            &sessions_root,
            session,
            owner(bcode_ipc::BUILD_FINGERPRINT, std::process::id()),
        )
        .expect("ours");
        assert!(matches!(
            client.session_owner(session),
            SessionOwner::ThisBuild(_)
        ));
        ours.release().expect("released");
        assert_eq!(client.session_owner(session), SessionOwner::None);
        let other = SessionId::new();
        let _theirs = FeedWriter::open(
            &sessions_root,
            other,
            owner("another-build", std::process::id()),
        )
        .expect("theirs");
        assert!(matches!(
            client.session_owner(other),
            SessionOwner::OtherBuild(_)
        ));
        // An owner that crashed without giving the session up is no owner.
        let stale = SessionId::new();
        let _stale = FeedWriter::open(&sessions_root, stale, owner("another-build", u32::MAX - 1))
            .expect("stale");
        assert_eq!(client.session_owner(stale), SessionOwner::None);
        assert!(client.follow_session_feed(other, None).is_ok());
    }
}
