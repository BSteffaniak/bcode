//! Bounded non-blocking client telemetry for the TUI event loop.

use bcode_client::BcodeClient;
use bcode_metrics::{
    ClientMetricBatch, ClientMetricObservation, MAX_CLIENT_METRIC_OBSERVATIONS, MetricLabels,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

const TELEMETRY_FLUSH_INTERVAL: Duration = Duration::from_secs(1);

type MetricKey = (String, MetricLabels);

#[derive(Debug)]
struct DeliveryOutcome {
    observations: usize,
    succeeded: bool,
}

/// TUI-local telemetry aggregation with one in-flight and one replaceable pending batch.
pub struct TuiTelemetry {
    enabled: bool,
    counters: BTreeMap<MetricKey, u64>,
    gauges: BTreeMap<MetricKey, i64>,
    histograms: Vec<(MetricKey, u64)>,
    next_flush_at: Instant,
    batch_tx: watch::Sender<Option<ClientMetricBatch>>,
    outcome_rx: mpsc::UnboundedReceiver<DeliveryOutcome>,
    in_flight: Arc<AtomicBool>,
    pending: Arc<AtomicBool>,
    worker: JoinHandle<()>,
}

impl TuiTelemetry {
    /// Start a bounded sender using an independent daemon connection.
    pub(crate) fn new(client: BcodeClient, enabled: bool) -> Self {
        let (batch_tx, mut batch_rx) = watch::channel(None::<ClientMetricBatch>);
        let (outcome_tx, outcome_rx) = mpsc::unbounded_channel();
        let in_flight = Arc::new(AtomicBool::new(false));
        let pending = Arc::new(AtomicBool::new(false));
        let worker_in_flight = Arc::clone(&in_flight);
        let worker_pending = Arc::clone(&pending);
        let worker = tokio::spawn(async move {
            loop {
                if batch_rx.changed().await.is_err() {
                    break;
                }
                let Some(batch) = batch_rx.borrow_and_update().clone() else {
                    continue;
                };
                worker_in_flight.store(true, Ordering::Release);
                worker_pending.store(false, Ordering::Release);
                let observations = batch.observations.len();
                let succeeded = client.ingest_client_metrics(batch).await.is_ok();
                worker_in_flight.store(false, Ordering::Release);
                let _ = outcome_tx.send(DeliveryOutcome {
                    observations,
                    succeeded,
                });
            }
        });
        Self {
            enabled,
            counters: BTreeMap::new(),
            gauges: BTreeMap::new(),
            histograms: Vec::new(),
            next_flush_at: Instant::now() + TELEMETRY_FLUSH_INTERVAL,
            batch_tx,
            outcome_rx,
            in_flight,
            pending,
            worker,
        }
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.counters.clear();
            self.gauges.clear();
            self.histograms.clear();
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

    pub(crate) fn next_flush_at(&self) -> Option<Instant> {
        self.enabled.then_some(self.next_flush_at)
    }

    pub(crate) fn flush_if_due(&mut self, now: Instant) {
        self.drain_outcomes();
        if !self.enabled {
            return;
        }
        if now >= self.next_flush_at || self.observation_count() >= MAX_CLIENT_METRIC_OBSERVATIONS {
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
        while let Ok(outcome) = self.outcome_rx.try_recv() {
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
            self.next_flush_at = now + TELEMETRY_FLUSH_INTERVAL;
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

        let has_in_flight = self.in_flight.load(Ordering::Acquire);
        let replaced = self.pending.swap(true, Ordering::AcqRel);
        if has_in_flight && replaced {
            self.add_counter_without_flush("tui.telemetry.dropped_total", 1);
        }
        self.batch_tx.send_replace(Some(batch));
        self.next_flush_at = now + TELEMETRY_FLUSH_INTERVAL;
    }
}

impl Drop for TuiTelemetry {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
