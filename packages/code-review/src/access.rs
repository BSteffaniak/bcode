//! Turns at a database file shared by several processes.
//!
//! Review apps (any number, in any repositories) and the daemon's code review plugin open the
//! same state and cache databases. The storage engine locks a database file exclusively for as
//! long as a connection to it is open, and an open that finds it locked fails at once instead of
//! waiting. So every open first takes an [`AccessTurn`]: an exclusive advisory lock on a file
//! beside the database. Waiting for a turn is a blocking lock, woken as soon as the holder
//! releases it, rather than a guessed retry schedule that a busy neighbour can outlast.
//!
//! A turn excludes every other holder, in another process or another thread of this one, because
//! each caller opens its own handle to the lock file.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

/// An exclusive turn at a shared database, released when dropped.
#[derive(Debug)]
pub struct AccessTurn {
    _file: File,
}

impl AccessTurn {
    /// Take the turn at the lock file `path` (created when missing), waiting at most `wait` for
    /// its holder to release it. `None` when the wait ran out.
    ///
    /// A wait that runs out leaves its waiter to take the turn later and release it at once, so
    /// it never holds a turn nobody uses.
    ///
    /// # Errors
    ///
    /// * The lock file cannot be opened or created.
    /// * Locking it fails for a reason other than another holder.
    pub fn take(path: &Path, wait: Duration) -> std::io::Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => return Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(error),
        }
        if wait.is_zero() {
            return Ok(None);
        }
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("access-turn".to_owned())
            .spawn(move || {
                // When the receiver is gone (the wait ran out), the file comes back here, or
                // stays in the dropped channel, and closing it releases the turn.
                let _ = sender.send(file.lock().map(|()| file));
            })?;
        match receiver.recv_timeout(wait) {
            Ok(Ok(file)) => Ok(Some(Self { _file: file })),
            Ok(Err(error)) => Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(std::io::Error::other("the access turn waiter stopped"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AccessTurn;
    use std::time::{Duration, Instant};

    #[test]
    fn a_turn_waits_for_the_holder_and_starts_when_it_releases() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("turn.lock");
        let held = AccessTurn::take(&path, Duration::ZERO)
            .expect("lock file")
            .expect("free turn");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        let started = Instant::now();
        let turn = AccessTurn::take(&path, Duration::from_secs(30)).expect("lock file");
        let waited = started.elapsed();
        releaser.join().expect("releaser");
        assert!(
            turn.is_some(),
            "the turn starts once the holder releases it"
        );
        assert!(waited >= Duration::from_millis(250), "waited {waited:?}");
        assert!(waited < Duration::from_secs(5), "waited {waited:?}");
    }

    #[test]
    fn a_wait_that_runs_out_takes_no_turn_and_leaves_none_behind() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("turn.lock");
        let held = AccessTurn::take(&path, Duration::ZERO)
            .expect("lock file")
            .expect("free turn");
        let timed_out = AccessTurn::take(&path, Duration::from_millis(50)).expect("lock file");
        assert!(timed_out.is_none());
        drop(held);
        // The abandoned waiter takes the turn when it is released and gives it back at once.
        let next = AccessTurn::take(&path, Duration::from_secs(5)).expect("lock file");
        assert!(next.is_some());
    }
}
