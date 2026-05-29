#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::significant_drop_tightening)]

//! Lightweight in-process metrics for Bcode runtime diagnostics.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const HISTOGRAM_BUCKETS_MS: &[u64] = &[
    1, 2, 5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000,
];
const DEFAULT_MAX_EVENTS: usize = 10_000;

/// Metric labels used for filtering and grouping dashboard views.
pub type MetricLabels = BTreeMap<String, String>;

/// Shared metrics registry handle.
#[derive(Debug, Clone, Default)]
pub struct MetricsRegistry {
    inner: Arc<Mutex<MetricsState>>,
}

/// Point-in-time metrics snapshot suitable for IPC/status responses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    /// Monotonic counters by metric key.
    pub counters: BTreeMap<String, u64>,
    /// Last-observed gauge values by metric key.
    pub gauges: BTreeMap<String, i64>,
    /// Duration/value distributions by metric key.
    pub histograms: BTreeMap<String, HistogramSnapshot>,
}

/// Rich metrics report suitable for dashboards and offline analysis.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsReport {
    /// Report generation time in Unix milliseconds.
    pub generated_at_unix_ms: u64,
    /// Aggregate point-in-time metrics.
    pub snapshot: MetricsSnapshot,
    /// Recent metric events, including labeled samples for filtering.
    pub events: Vec<MetricEvent>,
    /// Known metric descriptors discovered by this registry.
    pub descriptors: BTreeMap<String, MetricDescriptor>,
}

/// Metric metadata displayed by dashboards and exporters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricDescriptor {
    /// Metric name.
    pub name: String,
    /// Metric kind.
    pub kind: MetricKind,
    /// Optional measurement unit, for example `ms` or `count`.
    #[serde(default)]
    pub unit: Option<String>,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// Label names observed for this metric.
    #[serde(default)]
    pub label_keys: Vec<String>,
}

/// Metric event kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    /// Monotonic counter delta.
    Counter,
    /// Last-observed point value.
    Gauge,
    /// Distribution sample.
    Histogram,
    /// Untyped timeline event.
    Event,
}

/// One metric timeline event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricEvent {
    /// Event time in Unix milliseconds.
    pub unix_ms: u64,
    /// Metric name.
    pub name: String,
    /// Metric kind.
    pub kind: MetricKind,
    /// Integer event value.
    pub value: i64,
    /// Event labels.
    #[serde(default)]
    pub labels: MetricLabels,
}

/// Fixed-bucket histogram snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistogramSnapshot {
    /// Total observed sample count.
    pub count: u64,
    /// Sum of all observed sample values.
    pub sum: u64,
    /// Smallest observed sample value.
    #[serde(default)]
    pub min: Option<u64>,
    /// Largest observed sample value.
    #[serde(default)]
    pub max: Option<u64>,
    /// Cumulative bucket upper-bound counts.
    pub buckets: Vec<HistogramBucketSnapshot>,
}

/// One cumulative histogram bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistogramBucketSnapshot {
    /// Inclusive bucket upper bound.
    pub le: u64,
    /// Cumulative count of values less than or equal to `le`.
    pub count: u64,
}

/// RAII timer that records elapsed milliseconds when explicitly observed.
#[derive(Debug)]
pub struct MetricsTimer {
    started_at: Instant,
}

#[derive(Debug, Default)]
struct MetricsState {
    counters: BTreeMap<String, u64>,
    gauges: BTreeMap<String, i64>,
    histograms: BTreeMap<String, Histogram>,
    descriptors: BTreeMap<String, MetricDescriptor>,
    events: Vec<MetricEvent>,
    event_log_path: Option<PathBuf>,
    max_events: usize,
}

#[derive(Debug, Clone)]
struct Histogram {
    count: u64,
    sum: u64,
    min: Option<u64>,
    max: Option<u64>,
    buckets: Vec<u64>,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            count: 0,
            sum: 0,
            min: None,
            max: None,
            buckets: vec![0; HISTOGRAM_BUCKETS_MS.len()],
        }
    }
}

impl MetricsRegistry {
    /// Create a metrics registry that persists timeline events to `event_log_path`.
    #[must_use]
    pub fn with_event_log(event_log_path: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(MetricsState {
                event_log_path: Some(event_log_path.into()),
                max_events: DEFAULT_MAX_EVENTS,
                ..MetricsState::default()
            })),
        }
    }

    /// Increment a counter by one.
    pub fn increment_counter(&self, key: impl Into<String>) {
        self.add_counter(key, 1);
    }

    /// Add `value` to a counter.
    pub fn add_counter(&self, key: impl Into<String>, value: u64) {
        self.add_counter_with_labels(key, value, MetricLabels::new());
    }

    /// Add `value` to a labeled counter.
    pub fn add_counter_with_labels(
        &self,
        key: impl Into<String>,
        value: u64,
        labels: MetricLabels,
    ) {
        let key = key.into();
        let mut state = self.inner.lock().expect("metrics registry lock poisoned");
        let counter = state.counters.entry(key.clone()).or_default();
        *counter = counter.saturating_add(value);
        state.observe_descriptor(&key, MetricKind::Counter, &labels);
        state.push_event(MetricEvent {
            unix_ms: current_unix_millis(),
            name: key,
            kind: MetricKind::Counter,
            value: i64::try_from(value).unwrap_or(i64::MAX),
            labels,
        });
    }

    /// Set a gauge value.
    pub fn set_gauge(&self, key: impl Into<String>, value: i64) {
        self.set_gauge_with_labels(key, value, MetricLabels::new());
    }

    /// Set a labeled gauge value.
    pub fn set_gauge_with_labels(&self, key: impl Into<String>, value: i64, labels: MetricLabels) {
        let key = key.into();
        let mut state = self.inner.lock().expect("metrics registry lock poisoned");
        state.gauges.insert(key.clone(), value);
        state.observe_descriptor(&key, MetricKind::Gauge, &labels);
        state.push_event(MetricEvent {
            unix_ms: current_unix_millis(),
            name: key,
            kind: MetricKind::Gauge,
            value,
            labels,
        });
    }

    /// Record a histogram sample.
    pub fn record_histogram(&self, key: impl Into<String>, value: u64) {
        self.record_histogram_with_labels(key, value, MetricLabels::new());
    }

    /// Record a labeled histogram sample.
    pub fn record_histogram_with_labels(
        &self,
        key: impl Into<String>,
        value: u64,
        labels: MetricLabels,
    ) {
        let key = key.into();
        let mut state = self.inner.lock().expect("metrics registry lock poisoned");
        state
            .histograms
            .entry(key.clone())
            .or_default()
            .record(value);
        state.observe_descriptor(&key, MetricKind::Histogram, &labels);
        state.push_event(MetricEvent {
            unix_ms: current_unix_millis(),
            name: key,
            kind: MetricKind::Histogram,
            value: i64::try_from(value).unwrap_or(i64::MAX),
            labels,
        });
    }

    /// Record a labeled timeline event.
    pub fn record_event(&self, key: impl Into<String>, value: i64, labels: MetricLabels) {
        let key = key.into();
        let mut state = self.inner.lock().expect("metrics registry lock poisoned");
        state.observe_descriptor(&key, MetricKind::Event, &labels);
        state.push_event(MetricEvent {
            unix_ms: current_unix_millis(),
            name: key,
            kind: MetricKind::Event,
            value,
            labels,
        });
    }

    /// Start a timer for later elapsed observation.
    #[must_use]
    pub fn timer(&self) -> MetricsTimer {
        MetricsTimer::start()
    }

    /// Return a point-in-time metrics snapshot.
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        let state = self.inner.lock().expect("metrics registry lock poisoned");
        state.snapshot()
    }

    /// Return a rich metrics report with aggregate snapshots and recent events.
    #[must_use]
    pub fn report(&self) -> MetricsReport {
        let state = self.inner.lock().expect("metrics registry lock poisoned");
        MetricsReport {
            generated_at_unix_ms: current_unix_millis(),
            snapshot: state.snapshot(),
            events: state.report_events(),
            descriptors: state.descriptors.clone(),
        }
    }
}

impl MetricsTimer {
    /// Start a new timer.
    #[must_use]
    pub fn start() -> Self {
        Self {
            started_at: Instant::now(),
        }
    }

    /// Return elapsed milliseconds.
    #[must_use]
    pub fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

impl MetricsState {
    fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            counters: self.counters.clone(),
            gauges: self.gauges.clone(),
            histograms: self
                .histograms
                .iter()
                .map(|(key, histogram)| (key.clone(), histogram.snapshot()))
                .collect(),
        }
    }

    fn observe_descriptor(&mut self, key: &str, kind: MetricKind, labels: &MetricLabels) {
        let descriptor =
            self.descriptors
                .entry(key.to_owned())
                .or_insert_with(|| MetricDescriptor {
                    name: key.to_owned(),
                    kind,
                    unit: infer_unit(key),
                    description: None,
                    label_keys: Vec::new(),
                });
        for label in labels.keys() {
            if !descriptor.label_keys.contains(label) {
                descriptor.label_keys.push(label.clone());
            }
        }
    }

    fn push_event(&mut self, event: MetricEvent) {
        if let Some(path) = &self.event_log_path {
            append_event(path, &event);
        }
        self.events.push(event);
        let max_events = if self.max_events == 0 {
            DEFAULT_MAX_EVENTS
        } else {
            self.max_events
        };
        if self.events.len() > max_events {
            let excess = self.events.len() - max_events;
            self.events.drain(0..excess);
        }
    }

    fn report_events(&self) -> Vec<MetricEvent> {
        self.event_log_path.as_ref().map_or_else(
            || self.events.clone(),
            |path| read_recent_events(path, self.max_events.max(DEFAULT_MAX_EVENTS)),
        )
    }
}

impl Histogram {
    fn record(&mut self, value: u64) {
        self.count = self.count.saturating_add(1);
        self.sum = self.sum.saturating_add(value);
        self.min = Some(self.min.map_or(value, |min| min.min(value)));
        self.max = Some(self.max.map_or(value, |max| max.max(value)));
        for (index, bucket) in HISTOGRAM_BUCKETS_MS.iter().enumerate() {
            if value <= *bucket {
                self.buckets[index] = self.buckets[index].saturating_add(1);
            }
        }
    }

    fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            count: self.count,
            sum: self.sum,
            min: self.min,
            max: self.max,
            buckets: HISTOGRAM_BUCKETS_MS
                .iter()
                .zip(self.buckets.iter())
                .map(|(le, count)| HistogramBucketSnapshot {
                    le: *le,
                    count: *count,
                })
                .collect(),
        }
    }
}

fn append_event(path: &Path, event: &MetricEvent) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let Ok(line) = serde_json::to_string(event) else {
        return;
    };
    let _ = writeln!(file, "{line}");
}

fn read_recent_events(path: &Path, max_events: usize) -> Vec<MetricEvent> {
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if let Ok(event) = serde_json::from_str::<MetricEvent>(&line) {
            events.push(event);
        }
        if events.len() > max_events {
            let excess = events.len() - max_events;
            events.drain(0..excess);
        }
    }
    events
}

fn infer_unit(key: &str) -> Option<String> {
    key.ends_with("_ms").then(|| "ms".to_owned())
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_includes_counters_and_histograms() {
        let metrics = MetricsRegistry::default();
        metrics.increment_counter("counter");
        metrics.add_counter("counter", 2);
        metrics.set_gauge("gauge", 7);
        metrics.record_histogram("latency_ms", 5);
        metrics.record_histogram("latency_ms", 50);

        let snapshot = metrics.snapshot();

        assert_eq!(snapshot.counters.get("counter"), Some(&3));
        assert_eq!(snapshot.gauges.get("gauge"), Some(&7));
        let histogram = snapshot
            .histograms
            .get("latency_ms")
            .expect("histogram should exist");
        assert_eq!(histogram.count, 2);
        assert_eq!(histogram.sum, 55);
        assert_eq!(histogram.min, Some(5));
        assert_eq!(histogram.max, Some(50));
    }

    #[test]
    fn report_includes_labeled_events_and_descriptors() {
        let metrics = MetricsRegistry::default();
        let mut labels = MetricLabels::new();
        labels.insert("session_id".to_owned(), "session-a".to_owned());
        metrics.record_event("session.event", 1, labels);

        let report = metrics.report();

        assert_eq!(report.events.len(), 1);
        assert_eq!(
            report.events[0].labels.get("session_id"),
            Some(&"session-a".to_owned())
        );
        assert_eq!(
            report.descriptors["session.event"].label_keys,
            vec!["session_id".to_owned()]
        );
    }
}
