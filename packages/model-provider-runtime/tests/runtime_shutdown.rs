#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_model_provider_runtime::{ProviderRuntime, ProviderRuntimeError};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

struct Released(mpsc::Sender<()>);

impl Drop for Released {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

#[test]
fn shutdown_acknowledges_task_destruction_and_is_idempotent() {
    let runtime = ProviderRuntime::new().unwrap();
    let (released, receiver) = mpsc::channel();
    let guard = Released(released);
    let task = runtime.spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    runtime.shutdown(Duration::from_secs(5)).unwrap();
    receiver
        .try_recv()
        .expect("task resources released before acknowledgement");
    assert!(task.is_finished());
    runtime.shutdown(Duration::ZERO).unwrap();
    assert!(matches!(
        runtime.block_on(async {}),
        Err(ProviderRuntimeError::ShuttingDown)
    ));
    let polled = Arc::new(AtomicBool::new(false));
    let observed = polled.clone();
    drop(runtime.spawn(async move {
        observed.store(true, Ordering::SeqCst);
    }));
    drop(runtime);
    assert!(!polled.load(Ordering::SeqCst));
}

#[test]
fn concurrent_shutdown_callers_observe_the_same_release() {
    let runtime = Arc::new(ProviderRuntime::new().unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let runtime = Arc::clone(&runtime);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                runtime.shutdown(Duration::from_secs(5))
            })
        })
        .collect();
    barrier.wait();
    for worker in workers {
        worker.join().unwrap().unwrap();
    }
    runtime.shutdown(Duration::ZERO).unwrap();
}

#[test]
fn shutdown_timeout_retains_ownership_until_blocking_work_releases() {
    let runtime = ProviderRuntime::new().unwrap();
    let (release, receiver) = mpsc::channel();
    let (started, start_receiver) = mpsc::channel();
    runtime
        .block_on(async move {
            drop(tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                receiver.recv().unwrap();
            }));
        })
        .unwrap();
    start_receiver.recv_timeout(Duration::from_secs(5)).unwrap();
    let result = runtime.shutdown(Duration::from_millis(10));
    // Release before asserting so a regression cannot deadlock the runtime's Drop.
    release.send(()).unwrap();
    assert!(matches!(result, Err(ProviderRuntimeError::ShutdownTimeout)));
    runtime.shutdown(Duration::from_secs(5)).unwrap();
}
