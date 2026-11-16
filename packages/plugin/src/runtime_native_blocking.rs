//! Bounded, persistent blocking workers for one selected native execution.
//!
//! Jobs own their execution handles. Awaiting a result never joins a worker or
//! polls author code on an async-runtime thread. Dropping a result observer does
//! not cancel the job, discard its captures, or authorize unloading native code.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
};
use tokio::sync::{Semaphore, oneshot, watch};

type Job = Box<dyn FnOnce() + Send + 'static>;
type ShutdownOutcome = Option<Result<(), String>>;

struct State {
    sender: Option<mpsc::SyncSender<Job>>,
    workers: Vec<JoinHandle<()>>,
    shutdown: Option<watch::Receiver<ShutdownOutcome>>,
    reaper: Option<JoinHandle<()>>,
}

pub(super) struct NativeWorkers {
    slots: Arc<Semaphore>,
    capacity: usize,
    receiver: Arc<Mutex<mpsc::Receiver<Job>>>,
    state: Mutex<State>,
}

impl NativeWorkers {
    /// Capacity is selected by the owning execution, not a process-global pool.
    pub fn new(capacity: std::num::NonZeroUsize) -> Self {
        let (sender, receiver) = mpsc::sync_channel(capacity.get());
        Self {
            slots: Arc::new(Semaphore::new(capacity.get())),
            capacity: capacity.get(),
            receiver: Arc::new(Mutex::new(receiver)),
            state: Mutex::new(State {
                sender: Some(sender),
                workers: Vec::new(),
                shutdown: None,
                reaper: None,
            }),
        }
    }

    /// At most `capacity` jobs, queued plus executing, retain captures at once.
    /// Workers grow with actual overlapping demand and are reused between polls.
    pub async fn submit<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, String> {
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|failure| match failure {
                tokio::sync::TryAcquireError::Closed => "native workers closed".to_string(),
                tokio::sync::TryAcquireError::NoPermits => {
                    "native worker capacity saturated before job admission".to_string()
                }
            })?;
        let (result, observation) = oneshot::channel();
        let job: Job = Box::new(move || {
            let outcome = catch_unwind(AssertUnwindSafe(work)).map_err(|_| {
                "native blocking job panicked; execution retained for cleanup".to_string()
            });
            // Publication is nonblocking even if the observer has left. The
            // execution itself retains canonical operation outcomes for ack.
            let _ = result.send(outcome);
            drop(permit);
        });
        self.enqueue(job)?;
        observation.await.map_err(|_| {
            "native worker result unavailable; execution outcome unknown".to_string()
        })?
    }

    fn enqueue(&self, job: Job) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "native worker state poisoned".to_string())?;
        if state.sender.is_none() {
            return Err("native workers closed".to_string());
        }
        let outstanding = self.capacity.saturating_sub(self.slots.available_permits());
        if state.workers.len() < outstanding {
            let receiver = self.receiver.clone();
            let worker = std::thread::Builder::new()
                .name("bcode-native-poll".to_string())
                .spawn(move || worker_loop(&receiver))
                .map_err(|_| "native worker creation failed".to_string())?;
            state.workers.push(worker);
        }
        let sender = state
            .sender
            .as_ref()
            .ok_or_else(|| "native workers closed".to_string())?;
        let admission = sender.try_send(job);
        drop(state);
        admission.map_err(|failure| match failure {
            mpsc::TrySendError::Full(_) => {
                "native worker queue saturated before job execution".to_string()
            }
            mpsc::TrySendError::Disconnected(_) => "native worker queue disconnected".to_string(),
        })
    }

    /// Close admission, then await worker joins on a separate retained reaper.
    /// A cancelled observer can call this again; no job or cleanup is replayed.
    pub async fn shutdown(&self) -> Result<(), String> {
        self.slots.close();
        let mut observed = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "native worker state poisoned".to_string())?;
            if state.shutdown.is_none() {
                let sender = state.sender.take();
                drop(sender);
                let (finished, observation) = watch::channel(None);
                let workers = std::mem::take(&mut state.workers);
                // The reaper owns every worker handle until all joins settle.
                // It never runs plugin work and never occupies the serving runtime.
                state.reaper = Some(std::thread::spawn(move || {
                    let mut outcome = Ok(());
                    for worker in workers {
                        if worker.join().is_err() {
                            outcome =
                                Err("native worker terminated unexpectedly; cleanup required"
                                    .to_string());
                        }
                    }
                    finished.send_replace(Some(outcome));
                }));
                state.shutdown = Some(observation);
            }
            state
                .shutdown
                .as_ref()
                .expect("shutdown observer installed")
                .clone()
        };
        loop {
            let outcome = observed.borrow_and_update().clone();
            if let Some(outcome) = outcome {
                return outcome;
            }
            observed
                .changed()
                .await
                .map_err(|_| "native worker shutdown outcome unavailable".to_string())?;
        }
    }
}

fn worker_loop(receiver: &Mutex<mpsc::Receiver<Job>>) {
    loop {
        // Only queue receipt is serialized. Arbitrary native work runs unlocked.
        let job_result = receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv();
        match job_result {
            Ok(job) => job(),
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::{
        num::NonZeroUsize,
        sync::atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test(flavor = "current_thread")]
    async fn pending_native_callback_does_not_block_single_thread_serving_runtime() {
        let workers = NativeWorkers::new(NonZeroUsize::new(2).expect("capacity"));
        let handle = tokio::runtime::Handle::current();
        let (release, released) = oneshot::channel();
        let job = workers.submit(move || {
            assert!(tokio::runtime::Handle::try_current().is_err());
            handle.block_on(async {
                released.await.expect("async serving task");
                42_u64
            })
        });
        let serving = async {
            tokio::task::yield_now().await;
            release.send(()).expect("release");
        };
        let (result, ()) = tokio::join!(job, serving);
        assert_eq!(result.expect("job"), 42);
        workers.shutdown().await.expect("all workers joined");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn worker_is_reused_and_admission_is_bounded_without_blocking_future_poll() {
        let workers = NativeWorkers::new(NonZeroUsize::new(1).expect("capacity"));
        let first = workers
            .submit(|| std::thread::current().id())
            .await
            .expect("first");
        let second = workers
            .submit(|| std::thread::current().id())
            .await
            .expect("second");
        assert_eq!(first, second);
        let (release, released) = mpsc::channel();
        let (entered, entry) = oneshot::channel();
        let first = workers.submit(move || {
            let _ = entered.send(());
            released.recv().expect("release");
        });
        tokio::pin!(first);
        assert!(
            std::future::poll_fn(|context| std::task::Poll::Ready(first.as_mut().poll(context)))
                .await
                .is_pending()
        );
        entry.await.expect("worker running");
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let second = workers.submit(move || {
            observed.fetch_add(1, Ordering::SeqCst);
        });
        assert!(
            second
                .await
                .expect_err("explicit saturation")
                .contains("saturated")
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "rejected job performed no effects"
        );
        release.send(()).expect("release");
        first.await.expect("first settled");
        let observed = calls.clone();
        workers
            .submit(move || {
                observed.fetch_add(1, Ordering::SeqCst);
            })
            .await
            .expect("explicit new admission after capacity returns");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        workers.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn observer_drop_keeps_job_owned_until_explicit_shutdown_settles() {
        let workers = NativeWorkers::new(NonZeroUsize::new(1).expect("capacity"));
        let (release, released) = mpsc::channel();
        let (entered, entry) = oneshot::channel();
        let settled = Arc::new(AtomicUsize::new(0));
        let observed = settled.clone();
        let mut job = Box::pin(workers.submit(move || {
            let _ = entered.send(());
            released.recv().expect("release");
            observed.store(1, Ordering::SeqCst);
        }));
        assert!(
            std::future::poll_fn(|context| std::task::Poll::Ready(job.as_mut().poll(context)))
                .await
                .is_pending()
        );
        entry.await.expect("worker running");
        drop(job);
        assert_eq!(settled.load(Ordering::SeqCst), 0);
        let shutdown = workers.shutdown();
        tokio::pin!(shutdown);
        assert!(
            std::future::poll_fn(|context| std::task::Poll::Ready(shutdown.as_mut().poll(context)))
                .await
                .is_pending()
        );
        release.send(()).expect("release");
        shutdown.await.expect("joined");
        assert_eq!(settled.load(Ordering::SeqCst), 1);
    }
}
