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
fn cancellation_wait_observes_prior_and_concurrent_requests() {
    let runtime = ProviderRuntime::new().unwrap();
    runtime
        .block_on(async {
            let turn = bcode_model_provider_runtime::TurnState::default();
            turn.cancel();
            tokio::time::timeout(Duration::from_secs(5), turn.cancelled())
                .await
                .unwrap();
            let turn = bcode_model_provider_runtime::TurnState::default();
            let wait = turn.cancelled();
            tokio::pin!(wait);
            assert!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(
                    wait.as_mut().poll(cx).is_pending()
                ))
                .await
            );
            turn.cancel();
            tokio::time::timeout(Duration::from_secs(5), wait)
                .await
                .unwrap();
        })
        .unwrap();
}

#[test]
fn abandoning_turn_store_cancels_external_handles() {
    let mut store = bcode_model_provider_runtime::TurnStore::default();
    let (_, first) = store.insert_started("provider");
    let (_, second) = store.insert_started("provider");
    drop(store);
    assert!(first.is_cancelled());
    assert!(second.is_cancelled());
}

#[test]
fn unwinding_turn_store_cancels_external_handles() {
    let mut store = bcode_model_provider_runtime::TurnStore::default();
    let (_, turn) = store.insert_started("provider");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _store = store;
        panic!("abandoned provider owner");
    }));
    assert!(outcome.is_err());
    assert!(turn.is_cancelled());
}

#[test]
fn finishing_all_turns_cancels_handles_without_reusing_identity() {
    let mut store = bcode_model_provider_runtime::TurnStore::default();
    let (first_id, first) = store.insert_started("provider");
    let (second_id, second) = store.insert_started("provider");
    store.cancel_all();
    assert!(first.is_cancelled());
    assert!(!store.drain(&first_id).is_empty());
    store.finish_all();
    assert!(first.is_cancelled());
    assert!(second.is_cancelled());
    assert!(store.drain(&first_id).is_empty());
    assert!(store.drain(&second_id).is_empty());
    store.finish_all();
    let (next_id, next) = store.insert_started("provider");
    assert_ne!(first_id, next_id);
    assert_ne!(second_id, next_id);
    store.cancel(&first_id);
    assert!(!next.is_cancelled());
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
fn worker_can_request_shutdown_without_waiting_on_itself() {
    let runtime = Arc::new(ProviderRuntime::new().unwrap());
    let worker = Arc::clone(&runtime);
    let (sent, received) = mpsc::channel();
    drop(runtime.spawn(async move {
        worker.request_shutdown();
        assert!(matches!(
            worker.try_spawn(async {}),
            Err(ProviderRuntimeError::ShuttingDown)
        ));
        sent.send(()).unwrap();
    }));
    received.recv_timeout(Duration::from_secs(5)).unwrap();
    runtime.shutdown(Duration::from_secs(5)).unwrap();
    runtime.request_shutdown();
}

#[test]
fn runtime_worker_rejects_self_wait_without_starting_shutdown() {
    let runtime = Arc::new(ProviderRuntime::new().unwrap());
    let worker_runtime = Arc::clone(&runtime);
    runtime
        .block_on(async move {
            assert!(matches!(
                worker_runtime.block_on(async {}),
                Err(ProviderRuntimeError::RuntimeThreadWait)
            ));
            assert!(matches!(
                worker_runtime.shutdown(Duration::from_secs(5)),
                Err(ProviderRuntimeError::RuntimeThreadWait)
            ));
        })
        .unwrap();
    assert_eq!(runtime.block_on(async { 42 }).unwrap(), 42);
    runtime.shutdown(Duration::from_secs(5)).unwrap();
}

#[test]
fn blocking_worker_rejects_self_wait_without_closing_admission() {
    let runtime = Arc::new(ProviderRuntime::new().unwrap());
    let worker = Arc::clone(&runtime);
    runtime
        .block_on(async move {
            tokio::task::spawn_blocking(move || {
                assert!(matches!(
                    worker.shutdown(Duration::ZERO),
                    Err(ProviderRuntimeError::RuntimeThreadWait)
                ));
                assert!(matches!(
                    worker.block_on(async {}),
                    Err(ProviderRuntimeError::RuntimeThreadWait)
                ));
            })
            .await
            .unwrap();
        })
        .unwrap();
    assert_eq!(runtime.block_on(async { 42 }).unwrap(), 42);
    runtime.shutdown(Duration::from_secs(5)).unwrap();
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
