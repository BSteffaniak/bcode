//! Single flight: requests for the same result that overlap share one computation.
//!
//! Listings and merge bases are cached once computed, but two requests that arrive together (a
//! refresh racing an open, two screens of one repository) would both miss the cache and both
//! run git. With a flight, the first computes and the others wait for its answer. A failure is
//! not shared: each waiter then computes for itself, so it gets its own error.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

/// Computations in flight, by key.
#[derive(Debug)]
pub struct SingleFlight<K, V> {
    flights: Mutex<BTreeMap<K, Arc<Flight<V>>>>,
}

impl<K, V> Default for SingleFlight<K, V> {
    fn default() -> Self {
        Self {
            flights: Mutex::new(BTreeMap::new()),
        }
    }
}

/// One computation: how it went, and who waits for it.
#[derive(Debug)]
struct Flight<V> {
    answer: Mutex<Answer<V>>,
    done: Condvar,
    waiting: AtomicUsize,
}

/// How a computation went.
#[derive(Debug, Clone)]
enum Answer<V> {
    /// Still running.
    Running,
    /// Done, with this value.
    Value(V),
    /// Failed (or panicked): each waiter computes for itself.
    Failed,
}

impl<K: Ord + Clone, V: Clone> SingleFlight<K, V> {
    /// `compute`'s answer for `key`, or the answer of the computation for `key` already in
    /// flight. When that one fails (or panics), `compute` runs here instead.
    ///
    /// # Errors
    ///
    /// `compute`'s error.
    pub fn run<E>(&self, key: &K, compute: impl FnOnce() -> Result<V, E>) -> Result<V, E> {
        let (flight, leading) = self.join_or_lead(key);
        if !leading {
            return match flight.wait() {
                Answer::Value(value) => Ok(value),
                Answer::Running | Answer::Failed => compute(),
            };
        }
        // Landing (even by a panic) answers the waiters and ends the flight.
        let mut landing = Landing {
            flights: &self.flights,
            key,
            flight: &flight,
            answer: None,
        };
        let outcome = compute();
        landing.answer = outcome.as_ref().ok().cloned();
        outcome
    }

    /// The flight for `key`, and whether this request leads it (starts it) or waits for it.
    fn join_or_lead(&self, key: &K) -> (Arc<Flight<V>>, bool) {
        let mut flights = lock(&self.flights);
        let joined = flights.get(key).map(|flight| {
            flight.waiting.fetch_add(1, Ordering::SeqCst);
            Arc::clone(flight)
        });
        let led = joined.is_none().then(|| {
            let flight = Arc::new(Flight {
                answer: Mutex::new(Answer::Running),
                done: Condvar::new(),
                waiting: AtomicUsize::new(0),
            });
            flights.insert(key.clone(), Arc::clone(&flight));
            flight
        });
        drop(flights);
        match (joined, led) {
            (Some(flight), _) => (flight, false),
            (None, Some(flight)) => (flight, true),
            (None, None) => unreachable!("a flight is joined or led"),
        }
    }

    /// How many requests wait for the computation of `key` (tests).
    #[cfg(test)]
    fn waiting(&self, key: &K) -> usize {
        lock(&self.flights)
            .get(key)
            .map_or(0, |flight| flight.waiting.load(Ordering::SeqCst))
    }
}

impl<V: Clone> Flight<V> {
    /// Wait until the computation is done, and say how it went.
    fn wait(&self) -> Answer<V> {
        let answer = self
            .done
            .wait_while(lock(&self.answer), |answer| {
                matches!(answer, Answer::Running)
            })
            .unwrap_or_else(PoisonError::into_inner);
        answer.clone()
    }
}

/// Ends a flight when dropped: stores its answer, wakes the waiters, and forgets the flight.
struct Landing<'a, K: Ord, V> {
    flights: &'a Mutex<BTreeMap<K, Arc<Flight<V>>>>,
    key: &'a K,
    flight: &'a Arc<Flight<V>>,
    answer: Option<V>,
}

impl<K: Ord, V> Drop for Landing<'_, K, V> {
    fn drop(&mut self) {
        lock(self.flights).remove(self.key);
        *lock(&self.flight.answer) = self.answer.take().map_or(Answer::Failed, Answer::Value);
        self.flight.done.notify_all();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn overlapping_requests_share_one_computation_and_a_failure_is_not_shared() {
        let flights: Arc<SingleFlight<String, u32>> = Arc::default();
        let computed = Arc::new(AtomicUsize::new(0));
        let (started, leader_started) = mpsc::channel();
        let (go, leader_go) = mpsc::channel::<()>();
        let leader = {
            let (flights, computed) = (Arc::clone(&flights), Arc::clone(&computed));
            std::thread::spawn(move || {
                flights.run(&"key".to_owned(), || {
                    computed.fetch_add(1, Ordering::SeqCst);
                    started.send(()).expect("started");
                    leader_go.recv().expect("go");
                    Ok::<_, ()>(7)
                })
            })
        };
        leader_started.recv().expect("leader started");
        let follower = {
            let (flights, computed) = (Arc::clone(&flights), Arc::clone(&computed));
            std::thread::spawn(move || {
                flights.run(&"key".to_owned(), || {
                    computed.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, ()>(8)
                })
            })
        };
        while flights.waiting(&"key".to_owned()) == 0 {
            std::thread::yield_now();
        }
        go.send(()).expect("go");
        assert_eq!(leader.join().expect("leader"), Ok(7));
        assert_eq!(follower.join().expect("follower"), Ok(7));
        assert_eq!(computed.load(Ordering::SeqCst), 1);

        // Once landed, the next request computes again (the caller's cache answers repeats).
        assert_eq!(flights.run(&"key".to_owned(), || Ok::<_, ()>(9)), Ok(9));
        // A failure is the caller's own.
        assert_eq!(
            flights.run(&"other".to_owned(), || Err::<u32, _>("no")),
            Err("no")
        );
        // A panic still ends the flight.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            flights.run(&"boom".to_owned(), || -> Result<u32, ()> { panic!("boom") })
        }));
        assert!(panicked.is_err());
        assert_eq!(flights.run(&"boom".to_owned(), || Ok::<_, ()>(1)), Ok(1));
    }
}
