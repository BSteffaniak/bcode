//! Durable tracking-health fence for automatic storage maintenance.
//!
//! The fence starts dirty before reads are admitted and is marked clean only after all readers
//! and maintenance work have drained successfully. Crashes and failed access updates leave it
//! dirty. A dirty fence is never automatically reset: maintenance must explicitly establish fresh
//! access evidence first. This primitive does not itself coordinate multiple daemon participants.

use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};

const CLEAN: &[u8; 17] = b"BCSTHEALTH1:CLEAN";
const DIRTY: &[u8; 17] = b"BCSTHEALTH1:DIRTY";

/// Exclusive durable tracking-health ownership for one coordinated reader population.
///
/// Dropping without explicit clean completion preserves dirty state. The caller must hold this
/// fence before serving reads, stop maintenance immediately on failure, and not mark it clean until
/// every task that can read or update access state has completed (cancellation is not completion).
pub struct StorageTrackingFence {
    file: File,
    failed: AtomicBool,
}

impl StorageTrackingFence {
    /// Start a tracking epoch on a confined, read/write file.
    ///
    /// The caller is responsible for durable file creation and for ensuring that every participant
    /// accessing this state location is registered. A separate process cannot silently join this
    /// exclusive epoch. Empty files may initialize a new population, not repair missing evidence.
    ///
    /// # Errors
    /// Returns an error for contention, dirty/unknown/malformed state or I/O. Existing dirty state
    /// is preserved byte-for-byte and never treated as clean because no owner is alive.
    pub fn begin(mut file: File) -> io::Result<Self> {
        file.try_lock().map_err(io::Error::from)?;
        let length = file.metadata()?.len();
        if length != 0 {
            if length != CLEAN.len() as u64 {
                return Err(unavailable());
            }
            file.seek(SeekFrom::Start(0))?;
            let mut bytes = [0; CLEAN.len()];
            file.read_exact(&mut bytes)?;
            if &bytes != CLEAN {
                return Err(unavailable());
            }
        }
        file.seek(SeekFrom::Start(0))?;
        file.write_all(DIRTY)?;
        file.sync_all()?;
        Ok(Self {
            file,
            failed: AtomicBool::new(false),
        })
    }

    /// Permanently disable scheduling for this epoch. Already-durable dirty state needs no I/O.
    pub fn fail(&self) {
        self.failed.store(true, Ordering::SeqCst);
    }

    /// Whether this coordinated epoch has observed no tracking failures.
    #[must_use]
    pub fn healthy(&self) -> bool {
        !self.failed.load(Ordering::SeqCst)
    }

    /// Mark a fully drained healthy epoch clean, then release its file lock.
    ///
    /// # Errors
    /// Refuses failed epochs; I/O failure leaves unknown/dirty state which cannot be reopened as
    /// healthy without explicit maintenance. Caller must prove that no participant is still active.
    pub fn finish(mut self) -> io::Result<()> {
        if !self.healthy() {
            return Err(unavailable());
        }
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(CLEAN)?;
        self.file.sync_all()?;
        self.file.unlock()
    }
}

fn unavailable() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "storage access tracking requires explicit maintenance",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_child() {
        let Ok(path) = std::env::var("BCODE_TRACKING_FENCE_CRASH_FILE") else {
            return;
        };
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("file");
        let _fence = StorageTrackingFence::begin(file).expect("begin");
        std::process::exit(93);
    }

    #[test]
    fn process_death_releases_lock_but_retains_dirty_authority() {
        let file = tempfile::NamedTempFile::new().expect("file");
        let status = std::process::Command::new(std::env::current_exe().expect("executable"))
            .args(["--exact", "storage_tracking_fence::tests::crash_child"])
            .env("BCODE_TRACKING_FENCE_CRASH_FILE", file.path())
            .status()
            .expect("child");
        assert_eq!(status.code(), Some(93));
        let reopened = file.reopen().expect("open");
        reopened.try_lock().expect("dead owner released OS lock");
        reopened.unlock().expect("unlock");
        assert!(StorageTrackingFence::begin(reopened).is_err());
        assert_eq!(std::fs::read(file.path()).expect("durable dirty"), DIRTY);
    }

    #[test]
    fn clean_shutdown_can_restart_but_failed_epoch_cannot() {
        let file = tempfile::NamedTempFile::new().expect("file");
        let fence = StorageTrackingFence::begin(file.reopen().expect("open")).expect("begin");
        assert!(fence.healthy());
        assert!(StorageTrackingFence::begin(file.reopen().expect("contender")).is_err());
        fence.finish().expect("clean");
        let fence = StorageTrackingFence::begin(file.reopen().expect("restart")).expect("restart");
        fence.fail();
        assert!(!fence.healthy());
        assert!(fence.finish().is_err());
        assert!(StorageTrackingFence::begin(file.reopen().expect("later restart")).is_err());
        assert_eq!(std::fs::read(file.path()).expect("dirty"), DIRTY);
    }

    #[test]
    fn abandoned_epoch_and_unknown_versions_are_not_repaired() {
        let file = tempfile::NamedTempFile::new().expect("file");
        drop(StorageTrackingFence::begin(file.reopen().expect("open")).expect("begin"));
        assert!(StorageTrackingFence::begin(file.reopen().expect("reopen")).is_err());
        for bytes in [b"future format".as_slice(), b"", b"BCSTHEALTH2:CLEAN"] {
            if bytes.is_empty() {
                continue;
            }
            std::fs::write(file.path(), bytes).expect("fixture");
            assert!(StorageTrackingFence::begin(file.reopen().expect("open")).is_err());
            assert_eq!(std::fs::read(file.path()).expect("unchanged"), bytes);
        }
    }

    #[test]
    fn failure_requires_no_further_durable_write() {
        let file = tempfile::NamedTempFile::new().expect("file");
        let fence = StorageTrackingFence::begin(file.reopen().expect("open")).expect("begin");
        assert_eq!(std::fs::read(file.path()).expect("pre-failure"), DIRTY);
        fence.fail();
        assert_eq!(std::fs::read(file.path()).expect("post-failure"), DIRTY);
        drop(fence);
        assert!(StorageTrackingFence::begin(file.reopen().expect("restart")).is_err());
    }
}
