//! Execution regressions: overlap is measured while workers are actually suspended, not
//! inferred from a graph's width or the number of admitted futures.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct ActiveWorker(Arc<AtomicUsize>);

impl Drop for ActiveWorker {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct OverlappingWorkers {
    workflow: Workflow<Vec<usize>, Vec<usize>>,
    started: Arc<Semaphore>,
    release: Arc<Semaphore>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

fn overlapping_workers(count: usize) -> OverlappingWorkers {
    let started = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let worker = {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        let active = Arc::clone(&active);
        let peak = Arc::clone(&peak);
        Step::task("worker", move |input: usize, _| {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            async move {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                let _guard = ActiveWorker(active);
                peak.fetch_max(current, Ordering::SeqCst);
                started.add_permits(1);
                release
                    .acquire()
                    .await
                    .expect("release remains open")
                    .forget();
                Ok(input)
            }
        })
        .resources([ResourceClaim::read("shared-repository")])
    };
    let workflow = WorkflowBuilder::new("scheduler-overlap", fan_out("workers", worker, count))
        .build()
        .expect("valid workflow");
    OverlappingWorkers {
        workflow,
        started,
        release,
        active,
        peak,
    }
}

#[tokio::test]
async fn uncapped_execution_overlaps_32_100_and_257_workers() {
    for count in [32, 100, 257] {
        let OverlappingWorkers {
            workflow,
            started,
            release,
            active,
            peak,
        } = overlapping_workers(count);
        let input: Vec<_> = (0..count).collect();
        let control = async {
            started
                .acquire_many(u32::try_from(count).expect("test count"))
                .await
                .expect("workers start")
                .forget();
            assert_eq!(active.load(Ordering::SeqCst), count);
            release.add_permits(count);
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(workflow.run(input.clone()), control)
        })
        .await
        .expect("all workers must overlap without a hidden cap");
        assert_eq!(result.expect("completed"), input);
        assert_eq!(peak.load(Ordering::SeqCst), count);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn cancellation_releases_32_100_and_257_workers_and_allows_fresh_execution() {
    for count in [32, 100, 257] {
        let OverlappingWorkers {
            workflow,
            started,
            release,
            active,
            peak,
        } = overlapping_workers(count);
        let cancellation = WorkflowCancellation::new();
        let input: Vec<_> = (0..count).collect();
        let control = async {
            started
                .acquire_many(u32::try_from(count).expect("test count"))
                .await
                .expect("workers start")
                .forget();
            assert_eq!(active.load(Ordering::SeqCst), count);
            cancellation.cancel();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                workflow.run_with_cancellation(input.clone(), cancellation.clone()),
                control
            )
        })
        .await
        .expect("cancellation settles all workers");
        assert!(matches!(result, Err(WorkflowError::Cancelled { .. })));
        assert_eq!(peak.load(Ordering::SeqCst), count);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        // This is a fresh execution, not a replay of ambiguous durable effects. A cancelled
        // run must release its execution/resource permits rather than poison subsequent runs.
        release.add_permits(count);
        let output = tokio::time::timeout(Duration::from_secs(10), workflow.run(input.clone()))
            .await
            .expect("fresh execution is not blocked by cancelled workers")
            .expect("fresh execution succeeds");
        assert_eq!(output, input);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn immediately_ready_fan_out_yields_to_cancellation_between_work_quanta() {
    let completed = Arc::new(AtomicUsize::new(0));
    let worker_completed = Arc::clone(&completed);
    let first = Arc::new(Notify::new());
    let worker_first = Arc::clone(&first);
    let worker = Step::task("ready", move |input: usize, _| {
        let completed = Arc::clone(&worker_completed);
        let first = Arc::clone(&worker_first);
        async move {
            completed.fetch_add(1, Ordering::SeqCst);
            first.notify_one();
            Ok(input)
        }
    });
    let workflow = WorkflowBuilder::new("fair-ready-workers", fan_out("workers", worker, 1))
        .build()
        .expect("valid workflow");
    let cancellation = WorkflowCancellation::new();
    let control = async {
        first.notified().await;
        cancellation.cancel();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            workflow.run_with_cancellation((0..10_000).collect(), cancellation.clone()),
            control
        )
    })
    .await
    .expect("ready work cannot starve cancellation");
    assert!(matches!(result, Err(WorkflowError::Cancelled { .. })));
    assert!(completed.load(Ordering::SeqCst) < 10_000);
}

#[tokio::test]
async fn explicit_cap_bounds_actual_execution_without_limiting_total_work() {
    let OverlappingWorkers {
        workflow,
        started,
        release,
        active,
        peak,
    } = overlapping_workers(100);
    let control = async {
        started.acquire_many(7).await.expect("first wave").forget();
        assert_eq!(active.load(Ordering::SeqCst), 7);
        release.add_permits(100);
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            workflow.run_with_concurrency_limit((0..100).collect(), WorkflowCancellation::new(), 7),
            control
        )
    })
    .await
    .expect("bounded waves complete");
    assert_eq!(result.expect("completed"), (0..100).collect::<Vec<_>>());
    assert_eq!(peak.load(Ordering::SeqCst), 7);
    assert_eq!(active.load(Ordering::SeqCst), 0);
}
