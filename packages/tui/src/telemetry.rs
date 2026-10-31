//! Bounded non-blocking client telemetry for the TUI event loop.

use bcode_client::BcodeClient;
use bcode_metrics::{
    ClientMetricBatch, ClientMetricObservation, MAX_CLIENT_METRIC_OBSERVATIONS, MetricLabels,
};
use std::collections::BTreeMap;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

const TELEMETRY_FLUSH_INTERVAL: Duration = Duration::from_secs(1);

type MetricKey = (String, MetricLabels);

#[derive(Debug)]
struct DeliveryOutcome {
    observations: usize,
    succeeded: bool,
}

type Upload = Pin<Box<dyn Future<Output = bool> + Send>>;

#[derive(Default)]
struct DeliveryState {
    enabled: bool,
    pending: Option<ClientMetricBatch>,
    upload: Option<Upload>,
    observations: usize,
}

struct Delivery {
    state: Arc<Mutex<DeliveryState>>,
    wake: Arc<Notify>,
    outcomes: mpsc::Receiver<DeliveryOutcome>,
    worker: JoinHandle<()>,
}

impl Delivery {
    fn new<F, U>(mut upload: F) -> Self
    where
        F: FnMut(ClientMetricBatch) -> U + Send + 'static,
        U: Future<Output = bool> + Send + 'static,
    {
        let state = Arc::new(Mutex::new(DeliveryState {
            enabled: true,
            ..DeliveryState::default()
        }));
        let wake = Arc::new(Notify::new());
        let (outcome_tx, outcomes) = mpsc::channel(1);
        let worker_state = Arc::clone(&state);
        let worker_wake = Arc::clone(&wake);
        let worker = tokio::spawn(async move {
            loop {
                worker_wake.notified().await;
                // Optional metrics yield to ready interactive work before dispatch.
                tokio::task::yield_now().await;
                loop {
                    let outcome = poll_fn(|cx| {
                        let mut state = worker_state.lock().expect("telemetry delivery lock");
                        if !state.enabled {
                            return Poll::Ready(None);
                        }
                        if state.upload.is_none() {
                            let Some(batch) = state.pending.take() else {
                                return Poll::Ready(None);
                            };
                            let observations = batch.observations.len();
                            let request = upload(batch);
                            state.upload = Some(Box::pin(request));
                            // The count belongs to this request, not the replaceable slot.
                            state.observations = observations;
                        }
                        // Poll and disable share a short, non-async lock. Once disable
                        // returns no old future can be polled or queued batch dispatched.
                        let succeeded = std::task::ready!(
                            state
                                .upload
                                .as_mut()
                                .expect("active upload")
                                .as_mut()
                                .poll(cx)
                        );
                        state.upload = None;
                        Poll::Ready(Some(DeliveryOutcome {
                            observations: state.observations,
                            succeeded,
                        }))
                    })
                    .await;
                    let Some(outcome) = outcome else { break };
                    // Delivery diagnostics must never stall uploads or interactive work.
                    let _ = outcome_tx.try_send(outcome);
                    tokio::task::yield_now().await;
                }
            }
        });
        Self {
            state,
            wake,
            outcomes,
            worker,
        }
    }

    fn replace(&self, batch: ClientMetricBatch) -> bool {
        let mut state = self.state.lock().expect("telemetry delivery lock");
        if !state.enabled {
            return false;
        }
        let replaced = state.pending.replace(batch).is_some();
        drop(state);
        self.wake.notify_one();
        replaced
    }
}

impl Drop for Delivery {
    fn drop(&mut self) {
        let mut state = self.state.lock().expect("telemetry delivery lock");
        state.enabled = false;
        state.pending = None;
        // Cancel only this metrics request. Transport framing/reuse is client-owned.
        state.upload = None;
        drop(state);
        self.worker.abort();
    }
}

/// TUI-local telemetry aggregation with one in-flight and one replaceable pending batch.
pub struct TuiTelemetry {
    enabled: bool,
    client: BcodeClient,
    counters: BTreeMap<MetricKey, u64>,
    gauges: BTreeMap<MetricKey, i64>,
    histograms: Vec<(MetricKey, u64)>,
    next_flush_at: Option<Instant>,
    delivery: Option<Delivery>,
}

impl TuiTelemetry {
    /// Construct dormant telemetry; only explicit enable starts a delivery worker.
    pub(crate) fn new(client: BcodeClient, enabled: bool) -> Self {
        let mut telemetry = Self {
            enabled: false,
            client,
            counters: BTreeMap::new(),
            gauges: BTreeMap::new(),
            histograms: Vec::new(),
            next_flush_at: None,
            delivery: None,
        };
        telemetry.set_enabled(enabled);
        telemetry
    }

    /// Whether callers should prepare metric-only clocks, labels, and snapshots.
    #[must_use]
    pub(crate) const fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        if enabled {
            let client = self.client.clone();
            self.delivery = Some(Delivery::new(move |batch| {
                let client = client.clone();
                async move { client.ingest_client_metrics(batch).await.is_ok() }
            }));
            self.next_flush_at = Some(Instant::now() + TELEMETRY_FLUSH_INTERVAL);
        } else {
            self.delivery = None;
            self.next_flush_at = None;
            self.counters = BTreeMap::new();
            self.gauges = BTreeMap::new();
            self.histograms = Vec::new();
        }
    }

    pub(crate) fn add_counter(&mut self, name: &str, value: u64) {
        self.add_counter_with_labels(name, value, MetricLabels::new());
    }

    pub(crate) fn add_counter_with_labels(&mut self, name: &str, value: u64, labels: MetricLabels) {
        if !self.enabled || value == 0 {
            return;
        }
        let entry = self.counters.entry((name.to_owned(), labels)).or_default();
        *entry = entry.saturating_add(value);
        self.flush_if_due(Instant::now());
    }

    pub(crate) fn record_histogram(&mut self, name: &str, value: u64) {
        self.record_histogram_with_labels(name, value, MetricLabels::new());
    }

    pub(crate) fn set_gauge(&mut self, name: &str, value: i64) {
        if !self.enabled {
            return;
        }
        self.gauges
            .insert((name.to_owned(), MetricLabels::new()), value);
        self.flush_if_due(Instant::now());
    }

    pub(crate) fn record_histogram_with_labels(
        &mut self,
        name: &str,
        value: u64,
        labels: MetricLabels,
    ) {
        if !self.enabled {
            return;
        }
        self.histograms.push(((name.to_owned(), labels), value));
        self.flush_if_due(Instant::now());
    }

    pub(crate) const fn next_flush_at(&self) -> Option<Instant> {
        self.next_flush_at
    }

    pub(crate) fn flush_if_due(&mut self, now: Instant) {
        if !self.enabled {
            return;
        }
        self.drain_outcomes();
        if self.next_flush_at.is_some_and(|deadline| now >= deadline)
            || self.observation_count() >= MAX_CLIENT_METRIC_OBSERVATIONS
        {
            self.flush(now);
        }
    }

    fn observation_count(&self) -> usize {
        self.counters
            .len()
            .saturating_add(self.gauges.len())
            .saturating_add(self.histograms.len())
    }

    fn drain_outcomes(&mut self) {
        while let Some(outcome) = self
            .delivery
            .as_mut()
            .and_then(|delivery| delivery.outcomes.try_recv().ok())
        {
            if outcome.succeeded {
                self.add_counter_without_flush(
                    "tui.telemetry.delivered_observations",
                    u64::try_from(outcome.observations).unwrap_or(u64::MAX),
                );
            } else {
                self.add_counter_without_flush("tui.telemetry.failed_total", 1);
                self.add_counter_without_flush(
                    "tui.telemetry.failed_observations",
                    u64::try_from(outcome.observations).unwrap_or(u64::MAX),
                );
            }
        }
    }

    fn add_counter_without_flush(&mut self, name: &str, value: u64) {
        if !self.enabled || value == 0 {
            return;
        }
        let entry = self
            .counters
            .entry((name.to_owned(), MetricLabels::new()))
            .or_default();
        *entry = entry.saturating_add(value);
    }

    fn flush(&mut self, now: Instant) {
        if self.observation_count() == 0 {
            self.next_flush_at = Some(now + TELEMETRY_FLUSH_INTERVAL);
            return;
        }
        let batch_observations = self.observation_count();
        let mut observations = Vec::with_capacity(batch_observations.saturating_add(2));
        observations.extend(self.counters.iter().map(|((name, labels), value)| {
            ClientMetricObservation::CounterDelta {
                name: name.clone(),
                value: *value,
                labels: labels.clone(),
            }
        }));
        observations.extend(self.gauges.iter().map(|((name, labels), value)| {
            ClientMetricObservation::Gauge {
                name: name.clone(),
                value: *value,
                labels: labels.clone(),
            }
        }));
        observations.extend(self.histograms.iter().map(|((name, labels), value)| {
            ClientMetricObservation::Histogram {
                name: name.clone(),
                value: *value,
                labels: labels.clone(),
            }
        }));
        let deferred_batch_metrics =
            observations.len() > MAX_CLIENT_METRIC_OBSERVATIONS.saturating_sub(2);
        if !deferred_batch_metrics {
            observations.push(ClientMetricObservation::CounterDelta {
                name: "tui.telemetry.batch_total".to_owned(),
                value: 1,
                labels: MetricLabels::new(),
            });
            observations.push(ClientMetricObservation::CounterDelta {
                name: "tui.telemetry.batch_observations".to_owned(),
                value: u64::try_from(batch_observations).unwrap_or(u64::MAX),
                labels: MetricLabels::new(),
            });
        }
        observations.truncate(MAX_CLIENT_METRIC_OBSERVATIONS);
        let batch = ClientMetricBatch { observations };
        self.counters.clear();
        self.gauges.clear();
        self.histograms.clear();
        if deferred_batch_metrics {
            self.add_counter_without_flush("tui.telemetry.batch_total", 1);
            self.add_counter_without_flush(
                "tui.telemetry.batch_observations",
                u64::try_from(batch_observations).unwrap_or(u64::MAX),
            );
        }

        if self
            .delivery
            .as_ref()
            .is_some_and(|delivery| delivery.replace(batch))
        {
            self.add_counter_without_flush("tui.telemetry.dropped_total", 1);
        }
        self.next_flush_at = Some(now + TELEMETRY_FLUSH_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn batch(value: u64) -> ClientMetricBatch {
        ClientMetricBatch {
            observations: vec![ClientMetricObservation::CounterDelta {
                name: "test.delivery".to_owned(),
                value,
                labels: MetricLabels::new(),
            }],
        }
    }

    #[test]
    fn disabled_startup_needs_no_runtime_or_delivery_state() {
        let mut telemetry = TuiTelemetry::new(BcodeClient::default_endpoint(), false);
        telemetry.add_counter("test.counter", 1);
        telemetry.record_histogram("test.histogram", 1);
        telemetry.set_gauge("test.gauge", 1);
        telemetry.flush_if_due(Instant::now());
        telemetry.set_enabled(false);
        assert!(!telemetry.is_enabled());
        assert!(telemetry.delivery.is_none());
        assert!(telemetry.next_flush_at().is_none());
        assert_eq!(telemetry.observation_count(), 0);
    }

    #[tokio::test]
    async fn disable_discards_pending_before_dispatch_and_reenable_starts_fresh() {
        let mut telemetry = TuiTelemetry::new(BcodeClient::default_endpoint(), true);
        let calls = Arc::new(AtomicUsize::new(0));
        let uploads = Arc::clone(&calls);
        telemetry.delivery = Some(Delivery::new(move |_| {
            uploads.fetch_add(1, Ordering::SeqCst);
            async { true }
        }));
        telemetry.add_counter_without_flush("test.old", 1);
        telemetry.flush(Instant::now());
        telemetry.set_enabled(false);
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(telemetry.delivery.is_none());
        assert!(telemetry.next_flush_at().is_none());
        assert_eq!(telemetry.observation_count(), 0);

        telemetry.set_enabled(true);
        assert!(telemetry.delivery.is_some());
        assert!(telemetry.next_flush_at().is_some());
        assert_eq!(telemetry.observation_count(), 0);
        telemetry.add_counter("test.new", 1);
        assert_eq!(telemetry.observation_count(), 1);
        telemetry.set_enabled(true);
        assert_eq!(telemetry.observation_count(), 1);
    }

    struct CancelProbe(Arc<AtomicUsize>);

    impl Drop for CancelProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn disable_cancels_inflight_and_never_dispatches_pending() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicUsize::new(0));
        let (started_tx, mut started_rx) = mpsc::channel(1);
        let uploads = Arc::clone(&calls);
        let drops = Arc::clone(&cancelled);
        let mut telemetry = TuiTelemetry::new(BcodeClient::default_endpoint(), true);
        telemetry.delivery = Some(Delivery::new(move |_| {
            uploads.fetch_add(1, Ordering::SeqCst);
            let probe = CancelProbe(Arc::clone(&drops));
            let _ = started_tx.try_send(());
            async move {
                let _probe = probe;
                std::future::pending::<bool>().await
            }
        }));
        telemetry.delivery.as_ref().unwrap().replace(batch(1));
        started_rx.recv().await.unwrap();
        telemetry.delivery.as_ref().unwrap().replace(batch(2));
        telemetry.add_counter("test.aggregated", 1);
        telemetry.set_enabled(false);
        // Cancellation drops the request synchronously, not at the next task poll.
        assert_eq!(cancelled.load(Ordering::SeqCst), 1);
        assert_eq!(telemetry.observation_count(), 0);
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn slow_delivery_keeps_only_latest_pending_and_reports_outcomes() {
        let (started_tx, mut started_rx) = mpsc::channel(4);
        let permits = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::clone(&permits);
        let mut delivery = Delivery::new(move |batch| {
            let _ = started_tx.try_send(batch);
            let permits = Arc::clone(&permits);
            async move {
                permits.acquire().await.unwrap().forget();
                true
            }
        });
        assert!(!delivery.replace(batch(1)));
        assert_eq!(started_rx.recv().await.unwrap(), batch(1));
        assert!(!delivery.replace(batch(2)));
        for value in 3..1_000 {
            assert!(delivery.replace(batch(value)));
        }
        assert!(started_rx.try_recv().is_err());
        release.add_permits(1);
        assert_eq!(started_rx.recv().await.unwrap(), batch(999));
        let outcome = delivery.outcomes.recv().await.unwrap();
        assert!(outcome.succeeded);
        assert_eq!(outcome.observations, 1);
        release.add_permits(1);
        assert!(delivery.outcomes.recv().await.unwrap().succeeded);
        assert!(started_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn disabled_runtime_snapshots_advance_baseline_without_collecting_metrics() {
        let mut telemetry = TuiTelemetry::new(BcodeClient::default_endpoint(), false);
        let mut recorder = super::super::runtime_adapter::RuntimeStatsRecorder::default();
        recorder.record(
            &mut telemetry,
            &bmux_tui_runtime::RuntimeStats {
                frames_presented: 100,
                ..bmux_tui_runtime::RuntimeStats::default()
            },
        );
        assert_eq!(telemetry.observation_count(), 0);
        assert!(telemetry.delivery.is_none());
        telemetry.set_enabled(true);
        recorder.record(
            &mut telemetry,
            &bmux_tui_runtime::RuntimeStats {
                frames_presented: 103,
                ..bmux_tui_runtime::RuntimeStats::default()
            },
        );
        assert_eq!(
            telemetry.counters.get(&(
                "tui.runtime.frames_presented_total".to_owned(),
                MetricLabels::new()
            )),
            Some(&3)
        );
    }

    #[tokio::test]
    async fn reenabled_delivery_sends_new_observations_without_old_queue() {
        let mut telemetry = TuiTelemetry::new(BcodeClient::default_endpoint(), true);
        telemetry.add_counter_without_flush("test.old", 1);
        telemetry.flush(Instant::now());
        telemetry.set_enabled(false);
        telemetry.set_enabled(true);
        let (tx, mut rx) = mpsc::channel(1);
        telemetry.delivery = Some(Delivery::new(move |batch| {
            let _ = tx.try_send(batch);
            async { true }
        }));
        telemetry.add_counter_without_flush("test.new", 1);
        telemetry.flush(Instant::now());
        let delivered = rx.recv().await.unwrap();
        let names: Vec<_> = delivered
            .observations
            .iter()
            .map(|observation| match observation {
                ClientMetricObservation::CounterDelta { name, .. }
                | ClientMetricObservation::Gauge { name, .. }
                | ClientMetricObservation::Histogram { name, .. } => name.as_str(),
            })
            .collect();
        assert!(names.contains(&"test.new"));
        assert!(!names.contains(&"test.old"));
    }

    #[tokio::test]
    async fn failed_upload_is_reported_without_blocking_next_delivery() {
        let (tx, mut rx) = mpsc::channel(2);
        let mut delivery = Delivery::new(move |batch| {
            let _ = tx.try_send(batch);
            async { false }
        });
        delivery.replace(batch(1));
        assert_eq!(rx.recv().await.unwrap(), batch(1));
        assert!(!delivery.outcomes.recv().await.unwrap().succeeded);
        delivery.replace(batch(2));
        assert_eq!(rx.recv().await.unwrap(), batch(2));
        assert!(!delivery.outcomes.recv().await.unwrap().succeeded);
    }

    #[tokio::test]
    async fn enabled_aggregation_and_delivery_batches_are_bounded() {
        let mut telemetry = TuiTelemetry::new(BcodeClient::default_endpoint(), true);
        telemetry.delivery = Some(Delivery::new(|_| std::future::pending::<bool>()));
        for value in 0..10_000 {
            telemetry.record_histogram("test.backlog", value);
            assert!(telemetry.observation_count() <= MAX_CLIENT_METRIC_OBSERVATIONS);
        }
        let pending_count = telemetry
            .delivery
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .unwrap()
            .observations
            .len();
        assert!(pending_count <= MAX_CLIENT_METRIC_OBSERVATIONS);
    }

    #[tokio::test]
    #[ignore = "manual deterministic performance baseline"]
    async fn telemetry_enabled_disabled_overhead_baseline_report() {
        const ITERATIONS: u64 = 100_000;
        for enabled in [false, true] {
            let endpoint = bcode_ipc::IpcEndpoint::unix_socket(
                std::env::temp_dir().join(format!("bcode-telemetry-baseline-{enabled}.sock")),
            );
            let client = BcodeClient::new(endpoint);
            let mut telemetry = TuiTelemetry::new(client, enabled);
            let started = Instant::now();
            for frame in 0..ITERATIONS {
                telemetry.add_counter("tui.frame.total", 1);
                if frame.is_multiple_of(16) {
                    telemetry.record_histogram("tui.frame.total_ms", frame % 32);
                }
            }
            println!(
                "BCODE_PERF_CASE {}",
                serde_json::json!({
                    "domain": "telemetry_overhead",
                    "enabled": enabled,
                    "iterations": ITERATIONS,
                    "pending_observations": telemetry.observation_count(),
                    "wall_us": u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                })
            );
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn runtime_stats_map_to_secret_safe_tui_metrics() {
        let client = BcodeClient::default_endpoint();
        let mut telemetry = TuiTelemetry::new(client, true);
        let mut recorder = super::super::runtime_adapter::RuntimeStatsRecorder::default();
        recorder.record(
            &mut telemetry,
            &bmux_tui_runtime::RuntimeStats {
                reliable_depth: 3,
                reliable_high_water: 7,
                terminal_depth: 2,
                terminal_high_water: 6,
                latest_depth: 1,
                latest_high_water: 4,
                subscription_depth: 8,
                subscription_high_water: 9,
                reliable_rejected: 2,
                terminal_rejected: 3,
                latest_rejected: 1,
                latest_replaced: 10,
                subscription_rejected: 1,
                timers_delivered: 12,
                redraw_requests: 17,
                redraw_coalesced: 11,
                frames_presented: 14,
                full_repaints: 6,
                presented_changed_cells: 42,
                updates_completed: 18,
                scheduler_budget_exhausted: 5,
                commands_rejected: 2,
                stale_command_completions: 4,
                presentation_delay_us: 13,
                presentation_time_us: 19,
                update_time_us: 23,
                ..bmux_tui_runtime::RuntimeStats::default()
            },
        );

        assert_eq!(
            telemetry
                .gauges
                .get(&("tui.runtime.reliable_depth".to_owned(), MetricLabels::new())),
            Some(&3)
        );
        assert_eq!(
            telemetry.gauges.get(&(
                "tui.runtime.subscription_high_water".to_owned(),
                MetricLabels::new()
            )),
            Some(&9)
        );
        assert_eq!(
            telemetry.counters.get(&(
                "tui.runtime.latest_replaced_total".to_owned(),
                MetricLabels::new()
            )),
            Some(&10)
        );
        assert_eq!(
            telemetry.counters.get(&(
                "tui.runtime.redraw_coalesced_total".to_owned(),
                MetricLabels::new()
            )),
            Some(&11)
        );
        assert_eq!(
            telemetry.counters.get(&(
                "tui.runtime.frames_presented_total".to_owned(),
                MetricLabels::new()
            )),
            Some(&14)
        );
        assert_eq!(
            telemetry.counters.get(&(
                "tui.runtime.full_repaints_total".to_owned(),
                MetricLabels::new()
            )),
            Some(&6)
        );
        assert_eq!(
            telemetry.counters.get(&(
                "tui.runtime.presented_changed_cells_total".to_owned(),
                MetricLabels::new()
            )),
            Some(&42)
        );
        assert_eq!(
            telemetry.counters.get(&(
                "tui.runtime.updates_completed_total".to_owned(),
                MetricLabels::new()
            )),
            Some(&18)
        );
        assert!(telemetry.histograms.iter().any(|((name, _), value)| {
            name == "tui.runtime.presentation_delay_us" && *value == 13
        }));
        assert!(telemetry.histograms.iter().any(|((name, _), value)| {
            name == "tui.runtime.presentation_time_us" && *value == 19
        }));
        assert!(
            telemetry
                .histograms
                .iter()
                .any(|((name, _), value)| { name == "tui.runtime.update_time_us" && *value == 23 })
        );

        recorder.record(
            &mut telemetry,
            &bmux_tui_runtime::RuntimeStats {
                reliable_depth: 1,
                reliable_high_water: 7,
                latest_replaced: 12,
                redraw_coalesced: 15,
                frames_presented: 16,
                full_repaints: 7,
                presented_changed_cells: 50,
                updates_completed: 21,
                presentation_delay_us: 18,
                presentation_time_us: 26,
                update_time_us: 34,
                ..bmux_tui_runtime::RuntimeStats::default()
            },
        );
        assert_eq!(
            telemetry.counters.get(&(
                "tui.runtime.latest_replaced_total".to_owned(),
                MetricLabels::new()
            )),
            Some(&12)
        );
        assert_eq!(
            telemetry.counters.get(&(
                "tui.runtime.redraw_coalesced_total".to_owned(),
                MetricLabels::new()
            )),
            Some(&15)
        );
        assert_eq!(
            telemetry.counters.get(&(
                "tui.runtime.frames_presented_total".to_owned(),
                MetricLabels::new()
            )),
            Some(&16)
        );
        assert_eq!(
            telemetry.counters.get(&(
                "tui.runtime.full_repaints_total".to_owned(),
                MetricLabels::new()
            )),
            Some(&7)
        );
        assert_eq!(
            telemetry.counters.get(&(
                "tui.runtime.presented_changed_cells_total".to_owned(),
                MetricLabels::new()
            )),
            Some(&50)
        );
        assert_eq!(
            telemetry.counters.get(&(
                "tui.runtime.updates_completed_total".to_owned(),
                MetricLabels::new()
            )),
            Some(&21)
        );
        assert!(telemetry.histograms.iter().any(|((name, _), value)| {
            name == "tui.runtime.presentation_delay_us" && *value == 5
        }));
        assert!(telemetry.histograms.iter().any(|((name, _), value)| {
            name == "tui.runtime.presentation_time_us" && *value == 7
        }));
        assert!(
            telemetry
                .histograms
                .iter()
                .any(|((name, _), value)| { name == "tui.runtime.update_time_us" && *value == 11 })
        );
    }

    #[tokio::test]
    async fn outcome_channel_has_one_pending_slot() {
        let (outcome_tx, mut outcome_rx) = mpsc::channel(1);
        outcome_tx
            .try_send(DeliveryOutcome {
                observations: 1,
                succeeded: true,
            })
            .expect("first outcome fits");
        assert!(
            outcome_tx
                .try_send(DeliveryOutcome {
                    observations: 2,
                    succeeded: false,
                })
                .is_err()
        );
        assert_eq!(
            outcome_rx.recv().await.map(|outcome| outcome.observations),
            Some(1)
        );
    }

    #[tokio::test]
    async fn disabled_telemetry_is_a_noop() {
        let client = BcodeClient::default_endpoint();
        let mut telemetry = TuiTelemetry::new(client, false);
        telemetry.add_counter("tui.frame.total", 1);
        telemetry.record_histogram("tui.frame.total_ms", 17);

        assert_eq!(telemetry.observation_count(), 0);
        assert!(telemetry.next_flush_at().is_none());
    }

    #[tokio::test]
    async fn replaced_pending_batches_are_counted_as_dropped() {
        let client = BcodeClient::default_endpoint();
        let mut telemetry = TuiTelemetry::new(client, true);
        telemetry.add_counter_without_flush("test.pending", 1);
        telemetry.flush(Instant::now());
        telemetry.add_counter_without_flush("test.observation", 1);

        telemetry.flush(Instant::now());

        assert_eq!(
            telemetry.counters.get(&(
                "tui.telemetry.dropped_total".to_owned(),
                MetricLabels::new()
            )),
            Some(&1)
        );
        assert_eq!(telemetry.observation_count(), 1);
    }

    #[tokio::test]
    async fn counters_aggregate_before_delivery() {
        let client = BcodeClient::default_endpoint();
        let mut telemetry = TuiTelemetry::new(client, true);
        telemetry.add_counter("tui.frame.total", 1);
        telemetry.add_counter("tui.frame.total", 2);

        assert_eq!(telemetry.observation_count(), 1);
        assert_eq!(
            telemetry
                .counters
                .get(&("tui.frame.total".to_owned(), MetricLabels::new())),
            Some(&3)
        );
    }
}
