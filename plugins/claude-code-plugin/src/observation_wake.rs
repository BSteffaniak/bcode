//! Wake signal between the native stream reader and a bounded waiting `observe`.
//! A sequence change means the next page may differ; it is never delivery evidence.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
pub struct ObservationWake {
    sequence: Mutex<u64>,
    changed: Condvar,
}

impl ObservationWake {
    pub fn notify(&self) {
        if let Ok(mut sequence) = self.sequence.lock() {
            *sequence = sequence.wrapping_add(1);
            self.changed.notify_all();
        }
    }

    pub fn sequence(&self) -> u64 {
        self.sequence.lock().map_or(0, |sequence| *sequence)
    }

    /// Wait at most `timeout` for the sequence to move past `seen`. Returns `true` when
    /// it moved or the signal is unusable, so callers never wait on a poisoned lock.
    pub fn wait_changed(&self, seen: u64, timeout: Duration) -> bool {
        let Ok(sequence) = self.sequence.lock() else {
            return true;
        };
        self.changed
            .wait_timeout_while(sequence, timeout, |sequence| *sequence == seen)
            .map_or(true, |(sequence, _)| *sequence != seen)
    }
}

#[cfg(test)]
mod tests {
    use super::ObservationWake;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn waits_until_notified_or_timeout() {
        let wake = Arc::new(ObservationWake::default());
        let seen = wake.sequence();
        let started = Instant::now();
        assert!(!wake.wait_changed(seen, Duration::from_millis(20)));
        assert!(started.elapsed() >= Duration::from_millis(20));

        let notifier = Arc::clone(&wake);
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            notifier.notify();
        });
        assert!(wake.wait_changed(seen, Duration::from_secs(10)));
        thread.join().unwrap();
        // A notification before the wait begins is not lost.
        let seen = wake.sequence();
        wake.notify();
        assert!(wake.wait_changed(seen, Duration::from_secs(10)));
    }
}
